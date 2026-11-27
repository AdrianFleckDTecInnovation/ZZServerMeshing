//! Raw Managed entities have no gameplay authority until the worker consumes
//! an exact, time-bounded mesh admission. One bounded lane per prepared worker
//! isolates slow name lookups from periodic notices, player ingress and ticks.
use super::*;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
        Mutex,
    },
    time::{Duration, Instant},
};

const CAPACITY: usize = 32;
const ATTEMPT_LIMIT: Duration = Duration::from_secs(4);

#[derive(Component, Clone, Copy, Debug)]
pub struct PendingRegistration {
    pub attempt: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationStatus {
    Denied,
    TimedOut,
    AtCapacity,
    Unavailable,
}
#[derive(Component, Clone, Copy, Debug)]
pub struct RegistrationFailure {
    pub attempt: Id,
    pub reason: RegistrationStatus,
}

#[derive(Clone)]
struct Attempt {
    entity: Entity,
    object: Managed,
    id: Id,
    deadline: Instant,
    canceled: Arc<AtomicBool>,
    needs_restore: bool,
}
impl Attempt {
    fn current(&self, w: &World) -> bool {
        w.get::<PendingRegistration>(self.entity)
            .is_some_and(|p| p.attempt == self.id)
            && w.get::<Managed>(self.entity)
                .is_some_and(|m| m.id == self.object.id && m.group == self.object.group)
            && w.get::<Replica>(self.entity).is_none()
            && w.get::<Staged>(self.entity).is_none()
    }
    fn fail(&self, w: &mut World, reason: RegistrationStatus) {
        self.canceled.store(true, Ordering::Release);
        if w.get::<PendingRegistration>(self.entity)
            .is_some_and(|p| p.attempt == self.id)
        {
            let same = w
                .get::<Managed>(self.entity)
                .is_some_and(|m| m.id == self.object.id && m.group == self.object.group);
            let mut entity = w.entity_mut(self.entity);
            entity.remove::<PendingRegistration>();
            if same {
                entity.insert(RegistrationFailure {
                    attempt: self.id,
                    reason,
                });
            }
        }
    }
}
struct Reply<T>(Mutex<Receiver<Result<T>>>);
impl<T> Reply<T> {
    fn take(&mut self) -> Option<Result<T>> {
        match self
            .0
            .get_mut()
            .expect("exclusive registration receiver")
            .try_recv()
        {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(invalid("registration lane stopped"))),
        }
    }
}
type Grant = (Authority, u64, Id, Instant);
enum Stage {
    Reserve(Reply<Grant>),
    Commit {
        owner: Authority,
        base: u64,
        until: Instant,
        locally_paused: bool,
        reply: Reply<(u64, Record)>,
    },
}
#[derive(Resource)]
struct Registrations {
    db: Store,
    jobs: BulkExecutor,
    waiting: VecDeque<Attempt>,
    active: Option<(Attempt, Stage)>,
}
pub(super) fn engaged(w: &World, group: Id) -> bool {
    w.get_resource::<Registrations>()
        .and_then(|r| r.active.as_ref())
        .is_some_and(|(a, _)| a.object.group == group)
}
pub(super) fn committing(w: &World, group: Id) -> bool {
    w.get_resource::<Registrations>()
        .and_then(|r| r.active.as_ref())
        .is_some_and(|(a, s)| a.object.group == group && matches!(s, Stage::Commit { .. }))
}
fn release(r: &Registrations, w: Worker, g: Id, t: Id) {
    let db = r.db.clone();
    let _ = r.jobs.submit(move || db.release_install(g, w, t));
}
fn payload(w: &World, a: &Attempt) -> Result<(Record, Vec<Manifest>)> {
    let reg = w.resource::<Schemas>();
    let mut record = Record::new(a.object.id, a.object.group);
    record.fields = capture_fields(w, a.entity, reg, View::Checkpoint)?;
    let mut claims = vec![];
    for rule in reg.data.values() {
        claims.extend(rule(w, a.entity)?);
        if claims.len() > MAX_TARGETS {
            return Err(Error::Busy("raw member datasets"));
        }
    }
    encode(&(&record, &claims))?;
    Ok((record, claims))
}
fn paused(w: &mut World, a: &Attempt) {
    if w.get::<Managed>(a.entity)
        .is_some_and(|m| m.id == a.object.id && m.group == a.object.group)
    {
        w.entity_mut(a.entity)
            .remove::<(Simulated, MeshReady, ControlBinding, ValidatedInput)>()
            .insert(Replica);
    }
    let mut state = w.resource_mut::<MeshState>();
    if state.entities.get(&a.object.id) == Some(&a.entity) {
        state.mapped.remove(&a.entity);
        state.pending.remove(&a.object.id, a.object.group);
        state.input_sessions.remove(&a.object.id);
    }
    state.retry_groups.insert(a.object.group);
}
fn finish(
    w: &mut World,
    r: &mut Registrations,
    worker: Worker,
    mut a: Attempt,
    work: Stage,
) -> Option<(Attempt, Stage)> {
    let g = a.object.group;
    match work {
        Stage::Reserve(mut reply) => {
            if Instant::now() >= a.deadline {
                a.fail(w, RegistrationStatus::TimedOut);
            } else if !a.current(w) {
                a.fail(w, RegistrationStatus::Denied);
            }
            let Some(answer) = reply.take() else {
                return Some((a, Stage::Reserve(reply)));
            };
            let (owner, base, token, until) = match answer {
                Ok(v) => v,
                Err(Error::Busy(_)) if a.current(w) => {
                    r.waiting.push_back(a);
                    return None;
                }
                Err(e) => {
                    if a.current(w) {
                        a.fail(
                            w,
                            if matches!(e, Error::Denied | Error::Fenced) {
                                RegistrationStatus::Denied
                            } else {
                                RegistrationStatus::Unavailable
                            },
                        );
                    }
                    return None;
                }
            };
            if !a.current(w) || Instant::now() >= until || Instant::now() >= a.deadline {
                release(r, worker, g, token);
                a.fail(w, RegistrationStatus::TimedOut);
                return None;
            }
            let st = w.resource::<MeshState>();
            if st.entities.contains_key(&a.object.id) {
                release(r, worker, g, token);
                a.fail(w, RegistrationStatus::Denied);
                return None;
            }
            if st.external_staging.contains(&g)
                || checkpoint_io::pending(w, g)
                || st
                    .group_state(g)
                    .map_or(base != 0, |current| current != (owner, base))
            {
                release(r, worker, g, token);
                a.needs_restore = true;
                w.resource_mut::<MeshState>().retry_groups.insert(g);
                r.waiting.push_back(a);
                return None;
            }
            let result = payload(w, &a).and_then(|(record, claims)| {
                let db = r.db.clone();
                r.jobs.submit(move || {
                    let rev = db.commit_raw(owner, base, token, record.clone(), claims)?;
                    Ok((rev, record))
                })
            });
            let reply = match result {
                Ok(rx) => rx,
                Err(_) => {
                    release(r, worker, g, token);
                    a.fail(w, RegistrationStatus::Unavailable);
                    return None;
                }
            };
            // The real grant protects the member name, group revision and incarnation
            // until the first checkpoint atomically takes over or the grant expires.
            let mut st = w.resource_mut::<MeshState>();
            st.entities.insert(a.object.id, a.entity);
            st.mapped.insert(a.entity, a.object);
            st.groups.entry(g).or_insert((owner, base));
            st.dirty.insert(a.object.id, ObjectChange::lifecycle());
            w.entity_mut(a.entity)
                .remove::<PendingRegistration>()
                .insert(Simulated);
            Some((
                a,
                Stage::Commit {
                    owner,
                    base,
                    until,
                    locally_paused: false,
                    reply: Reply(Mutex::new(reply)),
                },
            ))
        }
        Stage::Commit {
            owner,
            base,
            until,
            mut locally_paused,
            mut reply,
        } => {
            if !locally_paused && Instant::now() >= until {
                paused(w, &a);
                locally_paused = true;
            }
            let Some(answer) = reply.take() else {
                return Some((
                    a,
                    Stage::Commit {
                        owner,
                        base,
                        until,
                        locally_paused,
                        reply,
                    },
                ));
            };
            match answer {
                Ok((rev, mut record))
                    if !locally_paused
                        && w.resource::<MeshState>().group_state(g) == Some((owner, base)) =>
                {
                    checkpoint_io::acknowledge(w, g, owner, rev, std::slice::from_mut(&mut record));
                    // Fields may have changed while the initial commit was in flight.
                    w.resource_mut::<MeshState>()
                        .pending
                        .urgent
                        .insert(a.object.id);
                }
                Ok(_) => {
                    if !locally_paused {
                        paused(w, &a);
                    }
                }
                Err(_) => {
                    if !locally_paused {
                        paused(w, &a);
                    }
                    // The write may have reached a remote store whose reply was
                    // lost. Only a verified restore may make this name live again.
                    if w.get::<Managed>(a.entity)
                        .is_some_and(|m| m.id == a.object.id && m.group == g)
                    {
                        w.entity_mut(a.entity).insert(RegistrationFailure {
                            attempt: a.id,
                            reason: RegistrationStatus::Unavailable,
                        });
                    }
                    w.resource_mut::<MeshState>().retry_groups.insert(g);
                }
            }
            None
        }
    }
}
pub(super) fn step(w: &mut World, db: Store, worker: Worker, fresh: Vec<(Entity, Managed)>) {
    if !w.contains_resource::<Registrations>() {
        if fresh.is_empty() {
            return;
        }
        let jobs = w.resource::<PreparedAuthority>().executor();
        w.insert_resource(Registrations {
            db: db.independent_link(),
            jobs,
            waiting: VecDeque::new(),
            active: None,
        });
    }
    w.resource_scope(|w, mut r: Mut<Registrations>| {
        for (entity, object) in fresh {
            if w.resource::<MeshState>().entities.get(&object.id) == Some(&entity)
                || w.get::<PendingRegistration>(entity).is_some()
            {
                continue;
            }
            let id = Id::new();
            if r.waiting.len() + usize::from(r.active.is_some()) >= CAPACITY {
                w.entity_mut(entity).insert(RegistrationFailure {
                    attempt: id,
                    reason: RegistrationStatus::AtCapacity,
                });
                continue;
            }
            let attempt = Attempt {
                entity,
                object,
                id,
                deadline: Instant::now() + ATTEMPT_LIMIT,
                canceled: Default::default(),
                needs_restore: false,
            };
            w.entity_mut(entity)
                .remove::<(AwaitingGrant, RegistrationFailure, Simulated, MeshReady)>()
                .insert(PendingRegistration { attempt: id });
            r.waiting.push_back(attempt);
        }
        if let Some((a, stage)) = r.active.take() {
            r.active = finish(w, &mut r, worker, a, stage);
        }
        r.waiting.retain(|a| {
            if !a.current(w) {
                a.fail(w, RegistrationStatus::Denied);
                false
            } else if Instant::now() >= a.deadline {
                a.fail(w, RegistrationStatus::TimedOut);
                false
            } else {
                true
            }
        });
        if r.active.is_some() {
            return;
        }
        let Some(index) = r.waiting.iter().position(|a| {
            !a.needs_restore
                || (w
                    .resource::<MeshState>()
                    .authority(a.object.group)
                    .is_some()
                    && !w
                        .resource::<MeshState>()
                        .retry_groups
                        .contains(&a.object.group))
        }) else {
            return;
        };
        let mut a = r.waiting.remove(index).unwrap();
        a.needs_restore = false;
        let db = r.db.clone();
        let task = a.clone();
        match r.jobs.submit(move || {
            if !matches!(db.lookup(task.object.id)?, Lookup::Unknown) {
                return Err(Error::Denied);
            }
            if task.canceled.load(Ordering::Acquire) || Instant::now() >= task.deadline {
                return Err(Error::Fenced);
            }
            let until = Instant::now() + crate::store::installation::INSTALL_WINDOW;
            let (owner, base, token) = db.reserve_raw(task.object.id, task.object.group, worker)?;
            if task.canceled.load(Ordering::Acquire) || Instant::now() >= task.deadline {
                let _ = db.release_install(task.object.group, worker, token);
                return Err(Error::Fenced);
            }
            Ok((owner, base, token, until))
        }) {
            Ok(rx) => r.active = Some((a, Stage::Reserve(Reply(Mutex::new(rx))))),
            Err(_) => r.waiting.push_front(a),
        }
    });
}
