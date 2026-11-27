//! Prepared session workers split the live handoff between the authenticated
//! socket worker (durable protocol, network) and short exclusive world steps.
use super::*;
use bevy_ecs::prelude::Resource;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};

type Claims = BTreeMap<Id, Vec<Manifest>>;
struct FrozenFields {
    checkpoint: Vec<Record>,
    changed: Vec<Record>,
    claims: Claims,
}
pub(super) enum Job {
    Handoff(Handoff, PhaseJob),
    Health(bool, usize),
    Diagnostics,
    Peer {
        from: Worker,
        updates: Vec<Update>,
        owners: BTreeMap<Id, (Id, Authority)>,
        until: Instant,
    },
}
pub(super) enum PhaseJob {
    Capture(Snapshot, Option<Arc<Freeze>>),
    Offer(Snapshot, BlobCache),
    Hash,
    Activate(Authority, u64, Instant, Vec<ControlBinding>),
    Resolve(Phase, Option<ResumePermit>),
}
pub(super) struct Freeze {
    fields: OnceLock<FrozenFields>,
    running: AtomicBool,
    checkpoint_started: AtomicBool,
    committed: OnceLock<(u64, u64)>,
}
#[derive(Clone, Copy)]
pub(super) struct ResumePermit {
    revision: u64,
    token: Id,
    until: Instant,
}
impl ResumePermit {
    fn reserve(db: &Store, t: Handoff) -> Result<Self> {
        let revision = db.check_authority(t.group, t.source, true)?;
        let until = Instant::now() + crate::store::installation::INSTALL_WINDOW;
        let token = db.reserve_install(t.group, t.source, revision)?;
        Ok(Self {
            revision,
            token,
            until,
        })
    }
    fn release(self, db: &Store, t: Handoff) -> Result<()> {
        db.release_install(t.group, t.source.worker, self.token)
    }
}
struct Decision {
    ticket: Handoff,
    phase: Phase,
    freeze: Option<Arc<Freeze>>,
    resume: Option<ResumePermit>,
}
#[derive(Resource, Default)]
struct Decisions {
    response: Option<Mutex<mpsc::Receiver<Result<Vec<Decision>>>>>,
    freezes: BTreeMap<Id, Arc<Freeze>>,
    db: Option<Store>,
}

fn world(ep: &WorkerEndpoint, role: Role, t: Handoff, job: PhaseJob) -> Result<Answer> {
    match ep.enqueue(role, Work::World(Job::Handoff(t, job)))? {
        Response::Control(a) => Ok(a),
        _ => Err(invalid("handoff world response")),
    }
}
pub(super) fn handoff(ep: &WorkerEndpoint, role: Role, t: Handoff, c: Command) -> Result<Answer> {
    let db = &ep.migration_store;
    let here = ep.service.worker;
    match c {
        Command::Preview if here == t.source.worker => {
            db.begin_handoff(t)?;
            match db.handoff_phase(t)? {
                Phase::Preparing => {
                    world(ep, role, t, PhaseJob::Capture(db.snapshot(t.group)?, None))
                }
                Phase::Frozen | Phase::Ready => Ok(Answer::Snapshot(db.frozen(t)?)),
                p => Ok(Answer::State(p)),
            }
        }
        Command::Freeze(known) if here == t.source.worker => {
            match db.handoff_phase(t)? {
                Phase::Preparing => (),
                Phase::Frozen | Phase::Ready => return Ok(Answer::Snapshot(db.frozen(t)?)),
                p => return Ok(Answer::State(p)),
            }
            let snapshot = db.snapshot(t.group)?;
            if snapshot.required != known {
                return Ok(Answer::State(Phase::Preparing));
            }
            let freeze = Arc::new(Freeze {
                fields: OnceLock::new(),
                running: AtomicBool::new(true),
                checkpoint_started: AtomicBool::new(false),
                committed: OnceLock::new(),
            });
            let result = (|| {
                let reply = world(
                    ep,
                    role,
                    t,
                    PhaseJob::Capture(snapshot, Some(freeze.clone())),
                )?;
                let Answer::Snapshot(mut live) = reply else {
                    return Ok(reply);
                };
                let fields = freeze.fields.get().ok_or(Error::Fenced)?;
                let mut checkpoint = fields.checkpoint.clone();
                if !fields.changed.is_empty() {
                    freeze.checkpoint_started.store(true, Ordering::Release);
                    let next = db.checkpoint_data(
                        t.group,
                        t.source,
                        live.revision,
                        fields.changed.clone(),
                        &fields.claims,
                    )?;
                    freeze
                        .committed
                        .set((live.revision, next))
                        .map_err(|_| Error::Fenced)?;
                    let changed: std::collections::BTreeSet<_> =
                        fields.changed.iter().map(|r| r.id).collect();
                    for r in live.records.iter_mut().chain(checkpoint.iter_mut()) {
                        if changed.contains(&r.id) {
                            r.revision = next;
                        }
                    }
                    live.revision = next;
                }
                Ok(Answer::Snapshot(db.freeze_checkpoint(
                    t,
                    live.revision,
                    &live.records,
                    &checkpoint,
                )?))
            })();
            freeze.running.store(false, Ordering::Release);
            result
        }
        Command::Offer(snapshot) if here == t.target => {
            if snapshot.group != t.group
                || !matches!(
                    db.handoff_phase(t)?,
                    Phase::Preparing | Phase::Frozen | Phase::Ready
                )
            {
                return Err(Error::Fenced);
            }
            let cache = ep
                .downloads
                .lock()
                .map_err(|_| invalid("bulk cache"))?
                .cache
                .clone();
            world(ep, role, t, PhaseJob::Offer(snapshot, cache))
        }
        Command::Ready if here == t.target => {
            let Answer::Staged(hash) = world(ep, role, t, PhaseJob::Hash)? else {
                return Err(Error::Fenced);
            };
            db.acknowledge_hash(t, here, hash)?;
            Ok(Answer::State(Phase::Ready))
        }
        Command::Switch => Ok(Answer::Owner(db.switch(t)?)),
        Command::Activate if here == t.target => {
            if db.handoff_phase(t)? != Phase::Switched {
                return Err(Error::Fenced);
            }
            let owner = db.owner(t.group)?;
            if owner.worker != here
                || owner.generation != t.source.generation.checked_add(1).ok_or(Error::Fenced)?
            {
                return Err(Error::Fenced);
            }
            let revision = db.check_authority(t.group, owner, false)?;
            // A quiet player has no movement sample to introduce it into the
            // destination's control set. Read its real grants on this socket
            // job, before taking the short installation lease.
            let controls = ep.prepared_controls.as_ref().ok_or(Error::Denied)?;
            let mut ready = Vec::new();
            let snapshot = db.snapshot(t.group)?;
            for members in snapshot.records.chunks(MAX_TARGETS) {
                let ids: Vec<_> = members.iter().map(|record| record.id).collect();
                let bindings: Vec<_> = db
                    .control_states(&ids)?
                    .into_iter()
                    .flatten()
                    .map(|state| state.binding)
                    .collect();
                if !bindings.is_empty() {
                    let accepted = controls.observe_independent(db, here, &bindings)?;
                    ready.extend(
                        bindings
                            .into_iter()
                            .zip(accepted)
                            .filter_map(|(binding, ok)| ok.then_some(binding)),
                    );
                }
            }
            let until = Instant::now() + crate::store::installation::INSTALL_WINDOW;
            let token = db.reserve_install(t.group, owner, revision)?;
            let result = world(
                ep,
                role,
                t,
                PhaseJob::Activate(owner, revision, until, ready),
            );
            let _ = db.release_install(t.group, here, token);
            result
        }
        Command::Abort => {
            let phase = db.abort_handoff(t)?;
            let resume = (here == t.source.worker && phase == Phase::Aborted)
                .then(|| ResumePermit::reserve(db, t).ok())
                .flatten();
            let result = world(ep, role, t, PhaseJob::Resolve(phase, resume));
            if let Some(p) = resume {
                let _ = p.release(db, t);
            }
            result?;
            let mut dl = ep.downloads.lock().map_err(|_| invalid("bulk cache"))?;
            if dl.partial.as_ref().is_some_and(|(held, _)| *held == t) {
                dl.partial = None;
            }
            Ok(Answer::State(phase))
        }
        Command::Status => Ok(Answer::State(db.handoff_phase(t)?)),
        _ => Err(Error::Denied),
    }
}

fn resume_source(w: &mut World, t: Handoff, freeze: &Freeze, permit: ResumePermit) {
    let accepted = if freeze.checkpoint_started.load(Ordering::Acquire) {
        match (freeze.committed.get(), freeze.fields.get()) {
            (Some(&(base, next)), Some(fields)) if next == permit.revision => {
                crate::bevy_runtime::acknowledge_frozen_checkpoint(
                    w,
                    t.group,
                    t.source,
                    base,
                    next,
                    &fields.changed,
                )
            }
            _ => false,
        }
    } else {
        w.resource::<MeshState>().group_state(t.group) == Some((t.source, permit.revision))
    };
    if accepted && Instant::now() < permit.until {
        crate::bevy_runtime::pause_group(w, t.group, false);
    }
    if !accepted || Instant::now() >= permit.until {
        // No receipt or the observed baseline changed. A fresh staged restore
        // must complete before the group can run gameplay again.
        crate::bevy_runtime::demote(w, t.group);
    }
    w.resource_mut::<MeshState>().retry_group(t.group);
}
fn resolve(
    loop_: &mut WorkerLoop,
    w: &mut World,
    t: Handoff,
    phase: Phase,
    resume: Option<ResumePermit>,
) {
    if loop_.paused.get(&t.group) == Some(&t) {
        let freeze = w
            .get_resource::<Decisions>()
            .and_then(|s| s.freezes.get(&t.id).cloned());
        let resolved = match (phase, freeze, resume) {
            (Phase::Switched, _, _) => {
                crate::bevy_runtime::demote(w, t.group);
                true
            }
            (Phase::Preparing | Phase::Aborted, Some(freeze), Some(permit))
                if !freeze.running.load(Ordering::Acquire) && Instant::now() < permit.until =>
            {
                resume_source(w, t, &freeze, permit);
                true
            }
            _ => false,
        };
        if resolved {
            loop_.paused.remove(&t.group);
            if let Some(mut c) = w.get_resource_mut::<Decisions>() {
                c.freezes.remove(&t.id);
            }
        }
    }
    if phase == Phase::Aborted && loop_.staged.as_ref().is_some_and(|(s, _)| *s == t) {
        w.resource_mut::<MeshState>()
            .external_staging
            .remove(&t.group);
        loop_.staged.take().unwrap().1.discard(w);
    }
}
pub(super) fn decisions(loop_: &mut WorkerLoop, w: &mut World) {
    if loop_.paused.is_empty() && loop_.staged.is_none() && !w.contains_resource::<Decisions>() {
        return;
    }
    w.init_resource::<Decisions>();
    let result = {
        let mut c = w.resource_mut::<Decisions>();
        c.response
            .as_mut()
            .and_then(|rx| rx.get_mut().ok()?.try_recv().ok())
    };
    if let Some(outcome) = result {
        w.resource_mut::<Decisions>().response = None;
        if let Ok(phases) = outcome {
            for Decision {
                ticket: t,
                phase,
                freeze: sampled,
                resume,
            } in phases
            {
                let current = w.resource::<Decisions>().freezes.get(&t.id);
                if match (current, sampled.as_ref()) {
                    (None, None) => true,
                    (Some(new), Some(old)) => {
                        Arc::ptr_eq(new, old) && !new.running.load(Ordering::Acquire)
                    }
                    _ => false,
                } {
                    resolve(loop_, w, t, phase, resume);
                }
                if let Some(permit) = resume {
                    let db = w.resource::<Decisions>().db.as_ref().unwrap().clone();
                    let _ = crate::bevy_runtime::background_worker(w)
                        .submit(move || permit.release(&db, t));
                }
            }
        }
    }
    if w.resource::<Decisions>().response.is_some() {
        return;
    }
    let tickets: Vec<_> = loop_
        .paused
        .values()
        .copied()
        .chain(loop_.staged.iter().map(|(t, _)| *t))
        .filter_map(|t| {
            let sample = w.resource::<Decisions>().freezes.get(&t.id).cloned();
            (!sample
                .as_ref()
                .is_some_and(|f| f.running.load(Ordering::Acquire)))
            .then_some((t, sample))
        })
        .collect();
    if tickets.is_empty() {
        return;
    }
    let db = w
        .resource_mut::<Decisions>()
        .db
        .get_or_insert_with(|| loop_.service.store.independent_link())
        .clone();
    if let Ok(rx) = crate::bevy_runtime::background_worker(w).submit(move || {
        Ok(tickets
            .into_iter()
            .filter_map(|(t, sample)| {
                let phase = db.handoff_phase(t).ok()?;
                let resume =
                    if sample.is_some() && matches!(phase, Phase::Preparing | Phase::Aborted) {
                        Some(ResumePermit::reserve(&db, t).ok()?)
                    } else {
                        None
                    };
                Some(Decision {
                    ticket: t,
                    phase,
                    freeze: sample,
                    resume,
                })
            })
            .collect())
    }) {
        w.resource_mut::<Decisions>().response = Some(Mutex::new(rx));
    }
}

pub(super) fn peer(
    ep: &WorkerEndpoint,
    role: Role,
    from: Worker,
    updates: Vec<Update>,
) -> Result<Response> {
    if updates.len() > MAX_TARGETS {
        return Err(Error::Busy("peer update batch"));
    }
    let until = Instant::now() + Duration::from_millis(200);
    let ids: Vec<_> = updates.iter().map(|u| u.object).collect();
    let owners = ep.replica_store.authorities(&ids)?;
    let result = ep.enqueue(
        role,
        Work::World(Job::Peer {
            from,
            updates,
            owners: owners.clone(),
            until,
        }),
    )?;
    // Replicas cannot become writers. Only confirm current generation data;
    // View epochs independently fence an out-of-order frame at the consumer.
    if let Response::Applied(acks) = result {
        let ids: Vec<_> = acks.iter().map(|(id, _, _)| *id).collect();
        let current = ep.replica_store.authorities(&ids)?;
        return Ok(Response::Applied(
            acks.into_iter()
                .filter(|(id, _, _)| current.get(id) == owners.get(id))
                .collect(),
        ));
    }
    Err(Error::Fenced)
}
pub(super) fn consume(loop_: &mut WorkerLoop, w: &mut World, job: Job) -> Result<Response> {
    let (t, job) = match job {
        Job::Health(draining, groups) => {
            return Ok(Response::Health {
                worker: loop_.service.worker,
                ready: !draining && w.resource::<MeshState>().tick() > 0,
                groups,
                load: loop_.process,
            })
        }
        Job::Diagnostics => {
            let p = loop_
                .peers
                .as_ref()
                .ok_or_else(|| invalid("peer transport disabled"))?;
            let (views, sent, received) = p.counts();
            return Ok(Response::Diagnostics {
                views,
                sent,
                received,
                last_error: p
                    .last_error
                    .clone()
                    .or_else(|| w.resource::<MeshState>().diagnostics.back().cloned()),
            });
        }
        Job::Peer {
            from,
            updates,
            owners,
            until,
        } => {
            if Instant::now() >= until {
                return Err(Error::Busy("peer projection expired"));
            }
            return loop_
                .peers
                .as_mut()
                .ok_or_else(|| invalid("peer transport disabled"))?
                .receive_prepared(w, from, updates, &owners);
        }
        Job::Handoff(t, job) => (t, job),
    };
    let answer = match job {
        PhaseJob::Capture(mut snapshot, freeze) => {
            if crate::bevy_runtime::preparing_group(w, t.group) {
                return Ok(Response::Control(Answer::State(Phase::Preparing)));
            }
            let state = w.resource::<MeshState>();
            if state.group_state(t.group) != Some((t.source, snapshot.revision))
                && !(state.authority(t.group).is_none() && snapshot.records.is_empty())
            {
                return Ok(Response::Control(Answer::State(Phase::Preparing)));
            }
            let mut checkpoint = Vec::new();
            let mut changed = Vec::new();
            for record in &mut snapshot.records {
                if record.deleted {
                    if freeze.is_some() {
                        checkpoint.push(record.clone());
                    }
                    continue;
                }
                let exact = capture(w, record.id, View::Migration)?;
                if record.revision != exact.revision {
                    return Ok(Response::Control(Answer::State(Phase::Preparing)));
                }
                if freeze.is_some() {
                    let ck = capture(w, record.id, View::Checkpoint)?;
                    if ck.fields != record.fields {
                        changed.push(ck.clone());
                    }
                    checkpoint.push(ck);
                }
                *record = exact;
            }
            if let Some(freeze) = freeze {
                if loop_.paused.get(&t.group).is_some_and(|old| *old != t) {
                    return Err(Error::Fenced);
                }
                if w.get_resource::<Decisions>()
                    .and_then(|c| c.freezes.get(&t.id))
                    .is_some_and(|f| f.running.load(Ordering::Acquire))
                {
                    return Err(Error::Busy("source freeze in progress"));
                }
                if !freeze.running.load(Ordering::Acquire) {
                    return Err(Error::Fenced);
                }
                let claims = crate::bevy_runtime::checkpoint_claims(w, &changed)?;
                freeze
                    .fields
                    .set(FrozenFields {
                        checkpoint,
                        changed,
                        claims,
                    })
                    .map_err(|_| Error::Fenced)?;
                crate::bevy_runtime::pause_group(w, t.group, true);
                loop_.paused.insert(t.group, t);
                w.init_resource::<Decisions>();
                w.resource_mut::<Decisions>().freezes.insert(t.id, freeze);
            }
            Answer::Snapshot(snapshot)
        }
        PhaseJob::Offer(s, cache) => {
            let hash = s.hash()?;
            if let Some((old, p)) = &loop_.staged {
                if *old != t {
                    return Err(Error::Busy("target preparation occupied"));
                }
                if p.hash() == hash {
                    return Ok(Response::Control(Answer::Staged(hash)));
                }
            }
            if let Some((_, p)) = loop_.staged.take() {
                p.discard(w);
            }
            w.insert_resource(cache);
            let p = prepare_world(w, &s)?;
            w.resource_mut::<MeshState>()
                .external_staging
                .insert(t.group);
            loop_.staged = Some((t, p));
            Answer::Staged(hash)
        }
        PhaseJob::Hash => {
            let (held, p) = loop_.staged.as_ref().ok_or(Error::Fenced)?;
            if *held != t {
                return Err(Error::Fenced);
            }
            Answer::Staged(p.hash())
        }
        PhaseJob::Activate(owner, rev, until, ready) => {
            if loop_
                .staged
                .as_ref()
                .is_some_and(|(held, p)| *held == t && p.revision() == rev)
            {
                let (_, p) = loop_.staged.take().unwrap();
                w.resource_mut::<MeshState>()
                    .external_staging
                    .remove(&t.group);
                p.install_until(w, owner, until)?;
            } else if Instant::now() >= until
                || w.resource::<MeshState>().group_state(t.group) != Some((owner, rev))
            {
                return Err(Error::Fenced);
            }
            for binding in ready {
                match crate::session::install_control_fence(w, binding) {
                    Ok(()) | Err(Error::Fenced | Error::Denied) => (),
                    Err(error) => return Err(error),
                }
            }
            Answer::Done
        }
        PhaseJob::Resolve(phase, resume) => {
            resolve(loop_, w, t, phase, resume);
            Answer::Done
        }
    };
    Ok(Response::Control(answer))
}
