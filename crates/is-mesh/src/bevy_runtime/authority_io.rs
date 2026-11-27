//! Optional pull path for persistent workers. The authority cursor advances only
//! after receiving a bounded page. Jobs capture their exact local baseline. A
//! handoff or a checkpoint which completes meanwhile invalidates their results.
use super::*;
use std::{
    ops::Bound::{Excluded, Unbounded},
    sync::{
        mpsc::{Receiver, TryRecvError},
        Mutex,
    },
    time::{Duration, Instant},
};

const NOTICE_INTERVAL: Duration = Duration::from_millis(25);
const WORKER_LIVENESS: Duration = Duration::from_secs(2);
type Notice = (u64, Id, Authority, u64);
type Baseline = Option<(Authority, u64)>;

struct Reply<T>(Mutex<Receiver<Result<T>>>);
impl<T> Reply<T> {
    fn take(&mut self) -> Option<Result<T>> {
        match self.0.get_mut().ok()?.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(invalid("authority I/O worker stopped"))),
        }
    }
}

enum Change {
    Elsewhere(Authority),
    Delta(Authority, u64, Vec<Record>),
    Restore(Authority, Snapshot),
}
struct Pending {
    group: Id,
    base: Baseline,
    invalidated: bool,
    stage: Stage,
}
enum Stage {
    Fetch(Reply<Change>),
    Grant(Candidate, Reply<(Id, Instant)>),
    Finished,
}
enum Candidate {
    Delta(Authority, u64, Vec<Record>),
    Restore(Authority, Prepared),
}
impl Candidate {
    fn version(&self) -> (Authority, u64) {
        match self {
            Self::Delta(a, rev, _) => (*a, *rev),
            Self::Restore(a, prepared) => (*a, prepared.revision),
        }
    }
    fn discard(self, w: &mut World) {
        if let Self::Restore(_, p) = self {
            p.discard(w);
        }
    }
    fn install(self, w: &mut World, group: Id) -> Result<()> {
        match self {
            Self::Delta(owner, revision, records) => {
                apply_records(w, records, true)?;
                w.resource_mut::<MeshState>()
                    .groups
                    .insert(group, (owner, revision));
                Ok(())
            }
            Self::Restore(owner, prepared) => prepared.install(w, owner),
        }
    }
}

/// Bounded authority notice and recovery work for a persistent world. Session
/// workers install it alongside `PreparedSessionControls`. A world holds at most
/// one notice reply and one group payload. Periodic checkpoints can share the executor.
#[derive(Resource)]
pub struct PreparedAuthority {
    executor: BulkExecutor,
    next_notice: Instant,
    notices: Option<Reply<Vec<Notice>>>,
    notice_started: Instant,
    authority_until: Instant,
    quiesced: bool,
    pending: Option<Pending>,
    last_group: Option<Id>,
    next_group: Instant,
    source: Option<Store>,
}
impl Default for PreparedAuthority {
    fn default() -> Self {
        Self::new(BulkExecutor::new(4))
    }
}
impl PreparedAuthority {
    pub(super) fn executor(&self) -> BulkExecutor {
        self.executor.clone()
    }
    pub fn new(executor: BulkExecutor) -> Self {
        let now = Instant::now();
        Self {
            executor,
            next_notice: now,
            notices: None,
            notice_started: now,
            authority_until: now + WORKER_LIVENESS,
            quiesced: false,
            pending: None,
            last_group: None,
            next_group: now,
            source: None,
        }
    }
}

pub(super) fn invalidate(w: &mut World, group: Id) {
    if let Some(mut queue) = w.get_resource_mut::<PreparedAuthority>() {
        if let Some(job) = &mut queue.pending {
            if job.group == group {
                job.invalidated = true;
            }
        }
    }
}

fn change(db: &Store, worker: Worker, group: Id, base: Baseline) -> Result<Change> {
    let owner = db.owner(group)?;
    if owner.worker != worker {
        return Ok(Change::Elsewhere(owner));
    }
    if let Some((old, revision)) = base {
        if old == owner {
            if let Ok((head, records)) = db.since(group, revision) {
                return Ok(Change::Delta(owner, head, records));
            }
        }
    }
    Ok(Change::Restore(owner, db.snapshot(group)?))
}

fn stage(w: &mut World, group: Id, base: Baseline, result: Change) -> Result<Option<Candidate>> {
    match result {
        Change::Elsewhere(owner) => {
            if base.is_some_and(|(a, _)| a.generation <= owner.generation) {
                demote(w, group);
            }
            Ok(None)
        }
        Change::Delta(a, rev, records) => {
            Ok((base != Some((a, rev))).then_some(Candidate::Delta(a, rev, records)))
        }
        Change::Restore(a, snapshot) => {
            Ok(Some(Candidate::Restore(a, prepare_world(w, &snapshot)?)))
        }
    }
}

fn finish(
    w: &mut World,
    jobs: &BulkExecutor,
    db: &Store,
    worker: Worker,
    job: &mut Pending,
) -> Result<bool> {
    let group = job.group;
    let state = w.resource::<MeshState>();
    let current = !job.invalidated
        && state.group_state(group) == job.base
        && !state.external_staging.contains(&group)
        && !checkpoint_io::pending(w, group)
        && !registration::engaged(w, group);
    match &mut job.stage {
        Stage::Fetch(fetch) => {
            let Some(result) = fetch.take() else {
                return Ok(false);
            };
            if !current {
                return Err(Error::Fenced);
            }
            let Some(candidate) = stage(w, group, job.base, result?)? else {
                return Ok(true);
            };
            let (owner, revision) = candidate.version();
            let source = db.clone();
            match jobs.submit(move || {
                // Start before sending. Time spent in the network, the authority,
                // the return queue and the host scheduler all spends this lease.
                let until = Instant::now() + crate::store::installation::INSTALL_WINDOW;
                Ok((source.reserve_install(group, owner, revision)?, until))
            }) {
                Ok(rx) => job.stage = Stage::Grant(candidate, Reply(Mutex::new(rx))),
                Err(e) => {
                    candidate.discard(w);
                    return Err(e);
                }
            }
            Ok(false)
        }
        Stage::Grant(_, grant) => {
            let Some(result) = grant.take() else {
                return Ok(false);
            };
            let Stage::Grant(candidate, _) = std::mem::replace(&mut job.stage, Stage::Finished)
            else {
                unreachable!()
            };
            let (token, until) = match result {
                Ok(permit) => permit,
                Err(e) => {
                    candidate.discard(w);
                    return Err(e);
                }
            };
            let result = if current && Instant::now() < until {
                let applied = candidate.install(w, group);
                if applied.is_err() || Instant::now() >= until {
                    // The World is still exclusive. No game system sees a partial
                    // or expired install; the next attempt stages a fresh snapshot.
                    demote(w, group);
                    applied.and(Err(Error::Fenced))
                } else {
                    Ok(true)
                }
            } else {
                candidate.discard(w);
                Err(Error::Fenced)
            };
            let source = db.clone();
            // A dropped release is bounded by the authority's monotonic reaper.
            let _ = jobs.submit(move || source.release_install(group, worker, token));
            result
        }
        Stage::Finished => Ok(true),
    }
}

pub(super) fn poll(w: &mut World, db: Store, worker: Worker) {
    w.resource_scope(|w, mut queue: Mut<PreparedAuthority>| {
        let now = Instant::now();
        let db = queue
            .source
            .get_or_insert_with(|| db.independent_link())
            .clone();
        let jobs = w
            .get_resource::<AsyncCheckpoints>()
            .map(AsyncCheckpoints::executor)
            .unwrap_or_else(|| queue.executor.clone());
        if let Some(notices) = queue.notices.as_mut().and_then(Reply::take) {
            queue.notices = None;
            queue.next_notice = now + NOTICE_INTERVAL;
            let mut state = w.resource_mut::<MeshState>();
            match notices {
                Ok(page) => {
                    queue.authority_until = queue.notice_started + WORKER_LIVENESS;
                    if now < queue.authority_until {
                        queue.quiesced = false;
                    }
                    for (mark, group, a, rev) in page {
                        if state.retry_groups.len() >= MAX_TARGETS {
                            break;
                        }
                        state.mark = mark;
                        if state.group_state(group) != Some((a, rev))
                            && (a.worker == worker || state.authority(group).is_some())
                        {
                            state.retry_groups.insert(group);
                        }
                    }
                }
                Err(error) => {
                    if matches!(error, Error::Denied | Error::Fenced) {
                        queue.authority_until = now;
                    }
                    state.report(error.to_string());
                }
            }
        }
        if now >= queue.authority_until && !queue.quiesced {
            // An old process can lose its notices link when the slot is fenced.
            // Quiesce existing gameplay as well as pending restores. Restoring
            // from the durable baseline requires fresh authority for each group.
            queue.quiesced = true;
            if let Some(job) = &mut queue.pending {
                job.invalidated = true;
            }
            demote_all(w);
        }
        if let Some(mut job) = queue.pending.take() {
            match finish(w, &jobs, &db, worker, &mut job) {
                Ok(false) => queue.pending = Some(job),
                Ok(true) => (),
                Err(error) => {
                    let mut state = w.resource_mut::<MeshState>();
                    state.report(error.to_string());
                    state.retry_groups.insert(job.group);
                    queue.next_group = now + NOTICE_INTERVAL;
                }
            }
        }
        let state = w.resource::<MeshState>();
        if queue.notices.is_none() && now >= queue.next_notice {
            let mark = state.mark;
            let source = db.clone();
            if let Ok(rx) = jobs.submit(move || source.notices(worker, mark)) {
                queue.notice_started = now;
                queue.notices = Some(Reply(Mutex::new(rx)));
            }
        }
        if queue.quiesced || queue.pending.is_some() || now < queue.next_group {
            return;
        }
        let eligible = |g: &&Id| {
            !state.external_staging.contains(*g)
                && !checkpoint_io::pending(w, **g)
                && !registration::engaged(w, **g)
        };
        let next = queue
            .last_group
            .and_then(|last| {
                state
                    .retry_groups
                    .range((Excluded(last), Unbounded))
                    .find(eligible)
            })
            .or_else(|| state.retry_groups.iter().find(eligible))
            .copied();
        if let Some(group) = next {
            let base = state.group_state(group);
            if let Ok(rx) = jobs.submit(move || change(&db, worker, group, base)) {
                queue.last_group = Some(group);
                queue.pending = Some(Pending {
                    group,
                    base,
                    invalidated: false,
                    stage: Stage::Fetch(Reply(Mutex::new(rx))),
                });
                w.resource_mut::<MeshState>().retry_groups.remove(&group);
            }
        }
    });
}
