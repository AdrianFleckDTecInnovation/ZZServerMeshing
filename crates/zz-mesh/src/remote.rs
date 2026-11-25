//! Handoff over separate authenticated control and bulk connections. The caller can
//! reconnect and resume with the same ticket. SQLite is the decision authority.
use crate::{
    wire::{Client, Endpoint, Request, Response, Role, Service},
    *,
};
use bevy_ecs::world::World;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Preview,
    Offer(Snapshot),
    Freeze(Vec<Manifest>),
    Ready,
    Switch,
    Activate,
    Abort,
    Status,
    Cache(Manifest),
    Chunk {
        manifest: Manifest,
        offset: u64,
        bytes: Vec<u8>,
    },
    MakeDelta {
        old: Manifest,
        next: Manifest,
    },
    InstallDelta {
        old: Manifest,
        next: Manifest,
        patch: Patch,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Answer {
    Snapshot(Snapshot),
    Staged(Digest),
    State(Phase),
    Owner(Authority),
    Cursor {
        offset: u64,
        base: Option<Manifest>,
        complete: bool,
    },
    Patch(Option<Patch>),
    Done,
}
type Job = (Role, Request, SyncSender<Result<Response>>);
struct Downloads {
    cache: BlobCache,
    partial: Option<(Handoff, Download)>,
}
/// Network threads perform bounded blob processing. Only the main thread touches Bevy.
pub struct WorkerEndpoint {
    broker: Option<Mutex<crate::network_mesh::Broker>>,
    service: Arc<Service>,
    requests: SyncSender<Job>,
    downloads: Arc<Mutex<Downloads>>,
}
pub struct WorkerLoop {
    pub peers: Option<crate::network_mesh::PeerMesh>,
    service: Arc<Service>,
    rx: Receiver<Job>,
    downloads: Arc<Mutex<Downloads>>,
    staged: Option<(Handoff, Prepared)>,
    paused: BTreeMap<Id, Handoff>,
    next_decision_check: Instant,
    sessions: BTreeMap<Id, VecDeque<Job>>,
    turns: VecDeque<Id>,
    queued: usize,
}
pub fn worker(service: Arc<Service>, initial: BlobCache) -> (Arc<WorkerEndpoint>, WorkerLoop) {
    let (tx, rx) = mpsc::sync_channel(32);
    let downloads = Arc::new(Mutex::new(Downloads {
        cache: initial,
        partial: None,
    }));
    let broker = (service.worker.slot == 1)
        .then(|| crate::network_mesh::Broker::new(service.store.clone(), service.worker))
        .transpose()
        .expect("directory incarnation")
        .map(Mutex::new);
    (
        Arc::new(WorkerEndpoint {
            broker,
            service: service.clone(),
            requests: tx,
            downloads: downloads.clone(),
        }),
        WorkerLoop {
            peers: None,
            service,
            rx,
            downloads,
            staged: None,
            paused: Default::default(),
            next_decision_check: Instant::now(),
            sessions: Default::default(),
            turns: Default::default(),
            queued: 0,
        },
    )
}
fn allowed(service: &Service, role: Role, t: Handoff) -> Result<()> {
    if service.worker != t.source.worker && service.worker != t.target {
        return Err(Error::Denied);
    }
    match role {
        Role::Worker(w) if w == t.source.worker || w == t.target => {
            service.store.check_worker(w)?
        }
        Role::Operator => (),
        _ => return Err(Error::Denied),
    }
    service.store.check_worker(service.worker)
}
impl WorkerEndpoint {
    fn bulk(&self, t: Handoff, c: &Command) -> Result<Option<Answer>> {
        let (m, source) = match c {
            Command::MakeDelta { next, .. } => (next, true),
            Command::Cache(m)
            | Command::Chunk { manifest: m, .. }
            | Command::InstallDelta { next: m, .. } => (m, false),
            _ => return Ok(None),
        };
        if self.service.worker != if source { t.source.worker } else { t.target } {
            return Err(Error::Denied);
        }
        if !matches!(
            self.service.store.handoff_phase(t)?,
            Phase::Preparing | Phase::Frozen | Phase::Ready
        ) {
            return Err(Error::Fenced);
        }
        if !self.service.store.requires(t.group, m)? {
            return Err(Error::Missing("required dataset version changed".into()));
        }
        let provider = self.service.providers.get(m.key.provider)?;
        if let Command::MakeDelta { old, next } = c {
            return Ok(Some(Answer::Patch(provider.delta(old, next)?)));
        }
        let mut d = self
            .downloads
            .lock()
            .map_err(|_| invalid("download queue"))?;
        if let Command::Cache(_) = c {
            if !d.cache.ready(m) {
                if let Some(bytes) = d.cache.get(m) {
                    d.cache.install(m, bytes.to_vec(), provider.as_ref())?;
                }
            }
            if d.cache.ready(m) {
                d.cache.hits += 1;
                return Ok(Some(Answer::Cursor {
                    offset: m.length,
                    base: None,
                    complete: true,
                }));
            }
            if !d
                .partial
                .as_ref()
                .is_some_and(|(id, p)| *id == t && p.manifest == *m)
            {
                if let Some((other, _)) = &d.partial {
                    if *other != t
                        && matches!(
                            self.service.store.handoff_phase(*other),
                            Ok(Phase::Preparing | Phase::Frozen | Phase::Ready)
                        )
                    {
                        return Err(Error::Busy("another bulk transfer is active"));
                    }
                }
                d.partial = Some((t, Download::new(m.clone(), MAX_BLOB)?));
            }
        }
        match c {
            Command::Chunk { offset, bytes, .. } => {
                if !d.cache.ready(m) {
                    let (id, p) = d
                        .partial
                        .as_mut()
                        .ok_or_else(|| Error::Missing("request a resume cursor".into()))?;
                    if *id != t || p.manifest != *m {
                        return Err(Error::Fenced);
                    }
                    p.append(
                        usize::try_from(*offset).map_err(|_| invalid("offset"))?,
                        bytes,
                    )?;
                    if p.offset() as u64 == m.length {
                        let (_, p) = d.partial.take().unwrap();
                        p.finish(&mut d.cache, provider.as_ref())?;
                    }
                }
            }
            Command::InstallDelta { old, next, patch } => {
                if !d.cache.ready(m) {
                    let bytes = d
                        .cache
                        .get(old)
                        .ok_or_else(|| Error::Missing("delta base".into()))?;
                    let new = patch.apply(old, &bytes, next)?;
                    drop(bytes);
                    d.cache.install(next, new, provider.as_ref())?;
                    d.partial = None;
                }
            }
            _ => {}
        }
        let complete = d.cache.ready(m);
        let offset = if complete {
            m.length
        } else {
            d.partial.as_ref().map_or(0, |(_, p)| p.offset() as u64)
        };
        Ok(Some(Answer::Cursor {
            offset,
            base: d.cache.baseline(m.key).cloned(),
            complete,
        }))
    }
}
impl Endpoint for WorkerEndpoint {
    fn dispatch(&self, role: Role, request: Request) -> Result<Response> {
        if let Request::Spatial(report) = request {
            return self
                .broker
                .as_ref()
                .ok_or(Error::Denied)?
                .lock()
                .map_err(|_| invalid("directory lock"))?
                .report(role, report)
                .map(Response::Routes);
        }
        match &request {
            Request::Control(t, c) => {
                allowed(&self.service, role, *t)?;
                if let Some(a) = self.bulk(*t, c)? {
                    return Ok(Response::Control(a));
                }
            }
            Request::Sync { .. }
            | Request::Diagnostics
            | Request::Input(_)
            | Request::Actions(_)
            | Request::Push(_) => {}
            _ => return self.service.dispatch(role, request),
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.requests
            .try_send((role, request, tx))
            .map_err(|_| Error::Busy("worker request queue"))?;
        rx.recv_timeout(Duration::from_secs(4))
            .map_err(|_| Error::Busy("response timeout; retry same action or transfer"))?
    }
}
fn exact(world: &World, db: &Store, t: Handoff, view: View) -> Result<Snapshot> {
    let mut snapshot = db.snapshot(t.group)?;
    for r in snapshot.records.iter_mut().filter(|r| !r.deleted) {
        let item = capture(world, r.id, view)?;
        if r.revision != item.revision {
            return Err(Error::Busy("worker catching up to committed revision"));
        }
        *r = item;
    }
    Ok(snapshot)
}
impl WorkerLoop {
    /// A bounded round-robin turn per logical session. One atomic request may finish
    /// after the soft deadline. Both its target count and its bytes have hard limits.
    pub fn pump(&mut self, w: &mut World, max: usize, time: Duration) {
        if Instant::now() >= self.next_decision_check {
            self.next_decision_check = Instant::now() + Duration::from_millis(100);
            self.paused.retain(
                |group, ticket| match self.service.store.handoff_phase(*ticket) {
                    Ok(Phase::Aborted) => {
                        crate::bevy_runtime::pause_group(w, *group, false);
                        false
                    }
                    Ok(Phase::Switched) => {
                        crate::bevy_runtime::demote(w, *group);
                        false
                    }
                    _ => true,
                },
            );
            if let Some((t, _)) = &self.staged {
                if matches!(self.service.store.handoff_phase(*t), Ok(Phase::Aborted)) {
                    w.resource_mut::<MeshState>()
                        .external_staging
                        .remove(&t.group);
                    self.staged.take().unwrap().1.discard(w);
                }
            }
        }
        let start = Instant::now();
        while self.queued < 32 {
            let Ok(job) = self.rx.try_recv() else { break };
            let session = match &job.1 {
                Request::Actions(a) => a.first().map(|i| i.key.session).unwrap_or(Id::numbered(0)),
                _ => Id::numbered(0),
            };
            let queue = self.sessions.entry(session).or_default();
            if queue.len() >= 8 {
                let _ = job.2.send(Err(Error::Busy("per-session request queue")));
                continue;
            }
            if queue.is_empty() {
                self.turns.push_back(session)
            }
            queue.push_back(job);
            self.queued += 1;
        }
        for _ in 0..max {
            if start.elapsed() >= time {
                break;
            }
            let Some(session) = self.turns.pop_front() else {
                break;
            };
            let queue = self.sessions.get_mut(&session).unwrap();
            let (role, req, reply) = queue.pop_front().unwrap();
            if queue.is_empty() {
                self.sessions.remove(&session);
            } else {
                self.turns.push_back(session)
            }
            self.queued -= 1;
            let result = match req {
                Request::Sync { session, group } => match role {
                    Role::Player(id) => {
                        reconcile(w, session, id, group).map(|(revision, sequence, actor)| {
                            Response::Confirmed {
                                revision,
                                sequence,
                                actor,
                            }
                        })
                    }
                    _ => Err(Error::Denied),
                },
                Request::Diagnostics => match role {
                    Role::Worker(_) | Role::Operator => {
                        let caller = match role {
                            Role::Worker(w) => w,
                            _ => self.service.worker,
                        };
                        self.service.store.check_worker(caller).and_then(|_| {
                            let p = self
                                .peers
                                .as_ref()
                                .ok_or_else(|| invalid("network mesh disabled"))?;
                            let (views, sent, received) = p.counts();
                            Ok(Response::Diagnostics {
                                views,
                                sent,
                                received,
                                last_error: p.last_error.clone().or_else(|| {
                                    w.resource::<MeshState>().diagnostics.back().cloned()
                                }),
                            })
                        })
                    }
                    _ => Err(Error::Denied),
                },
                Request::Push(updates) => self
                    .peers
                    .as_mut()
                    .ok_or_else(|| invalid("peer transport disabled"))
                    .and_then(|p| p.receive(w, role, updates)),
                Request::Control(t, c) => self.control(w, role, t, c).map(Response::Control),
                Request::Input(i) => match role {
                    Role::Player(actor) => std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|_| invalid("server clock"))
                        .and_then(|n| apply_input(w, actor, &i, n.as_millis() as u64))
                        .map(Response::Input),
                    _ => Err(Error::Denied),
                },
                Request::Actions(batch) => {
                    let group = batch.first().map(|i| i.group);
                    group
                        .map_or(Ok(()), |g| {
                            if w.resource::<MeshState>().authority(g)
                                != Some(self.service.store.owner(g)?)
                                || w.resource::<MeshState>().external_staging.contains(&g)
                            {
                                return Err(Error::Fenced);
                            }
                            checkpoint_group(w, g)
                        })
                        .and_then(|_| self.service.dispatch(role, Request::Actions(batch)))
                }
                r => self.service.dispatch(role, r),
            };
            let _ = reply.send(result);
        }
    }
    fn control(&mut self, w: &mut World, role: Role, t: Handoff, c: Command) -> Result<Answer> {
        allowed(&self.service, role, t)?;
        let db = &self.service.store;
        let here = self.service.worker;
        match c {
            Command::Preview if here == t.source.worker => {
                db.begin_handoff(t)?;
                match db.handoff_phase(t)? {
                    Phase::Preparing => Ok(Answer::Snapshot(exact(w, db, t, View::Migration)?)),
                    Phase::Frozen | Phase::Ready => Ok(Answer::Snapshot(db.frozen(t)?)),
                    p => Ok(Answer::State(p)),
                }
            }
            Command::Offer(s) if here == t.target => {
                if !matches!(
                    db.handoff_phase(t)?,
                    Phase::Preparing | Phase::Frozen | Phase::Ready
                ) || s.group != t.group
                {
                    return Err(Error::Fenced);
                }
                let hash = s.hash()?;
                if let Some((old, p)) = &self.staged {
                    if *old == t && p.hash() == hash {
                        return Ok(Answer::Staged(hash));
                    }
                    if *old != t
                        && matches!(
                            db.handoff_phase(*old)?,
                            Phase::Preparing | Phase::Frozen | Phase::Ready
                        )
                    {
                        return Err(Error::Busy("target preparation occupied"));
                    }
                }
                if let Some((_, p)) = self.staged.take() {
                    p.discard(w);
                }
                w.insert_resource(
                    self.downloads
                        .lock()
                        .map_err(|_| invalid("bulk cache"))?
                        .cache
                        .clone(),
                );
                let p = prepare_world(w, &s)?;
                w.resource_mut::<MeshState>()
                    .external_staging
                    .insert(t.group);
                self.staged = Some((t, p));
                Ok(Answer::Staged(hash))
            }
            Command::Freeze(known) if here == t.source.worker => {
                if matches!(db.handoff_phase(t)?, Phase::Frozen | Phase::Ready) {
                    return Ok(Answer::Snapshot(db.frozen(t)?));
                }
                let s = exact(w, db, t, View::Migration)?;
                if s.required != known {
                    return Ok(Answer::State(Phase::Preparing));
                }
                let checkpoints = exact(w, db, t, View::Checkpoint)?;
                let snap = db.freeze_checkpoint(t, s.revision, &s.records, &checkpoints.records)?;
                crate::bevy_runtime::pause_group(w, t.group, true);
                self.paused.insert(t.group, t);
                Ok(Answer::Snapshot(snap))
            }
            Command::Ready if here == t.target => {
                let (id, p) = self
                    .staged
                    .as_ref()
                    .ok_or_else(|| Error::Missing("prepared group".into()))?;
                if *id != t {
                    return Err(Error::Fenced);
                }
                db.acknowledge(t, here, p)?;
                Ok(Answer::State(Phase::Ready))
            }
            Command::Switch => Ok(Answer::Owner(db.switch(t)?)),
            Command::Activate if here == t.target => {
                if db.handoff_phase(t)? != Phase::Switched || db.owner(t.group)?.worker != here {
                    return Err(Error::Fenced);
                }
                if self.staged.as_ref().is_some_and(|(x, _)| *x == t) {
                    let (_, p) = self.staged.take().unwrap();
                    w.resource_mut::<MeshState>()
                        .external_staging
                        .remove(&t.group);
                    p.activate(w, db, db.owner(t.group)?)?;
                } else if w.resource::<MeshState>().authority(t.group) != Some(db.owner(t.group)?) {
                    return Err(Error::Missing("target recovering".into()));
                }
                Ok(Answer::Done)
            }
            Command::Abort => {
                let phase = db.abort_handoff(t)?;
                if phase == Phase::Aborted {
                    if here == t.source.worker {
                        crate::bevy_runtime::pause_group(w, t.group, false);
                    }
                    if self.staged.as_ref().is_some_and(|(x, _)| *x == t) {
                        self.staged.take().unwrap().1.discard(w);
                        w.resource_mut::<MeshState>()
                            .external_staging
                            .remove(&t.group);
                    }
                    let mut dl = self.downloads.lock().map_err(|_| invalid("bulk cache"))?;
                    if dl.partial.as_ref().is_some_and(|(id, _)| *id == t) {
                        dl.partial = None;
                    }
                }
                Ok(Answer::State(phase))
            }
            Command::Status => Ok(Answer::State(db.handoff_phase(t)?)),
            _ => Err(Error::Denied),
        }
    }
}
fn call(c: &mut Client, t: Handoff, cmd: Command) -> Result<Answer> {
    match c.call(&Request::Control(t, cmd))? {
        Response::Control(a) => Ok(a),
        Response::Error(e) => Err(invalid(e)),
        _ => Err(invalid("unexpected worker response")),
    }
}
/// Reinvoke with the same ticket after a disconnect. Abort is a durable decision,
/// including when the reply to Switch was lost. No timeout grants write permission.
pub fn migrate(
    source: &mut Client,
    target: &mut Client,
    t: Handoff,
    budget: usize,
) -> Result<MigrationCost> {
    let start = Instant::now();
    let mut payload = 0usize;
    let mut pause = None;
    for _ in 0..4 {
        let s = match call(source, t, Command::Preview)? {
            Answer::Snapshot(s) => s,
            Answer::State(Phase::Switched) => {
                call(target, t, Command::Activate)?;
                return Ok(MigrationCost {
                    payload,
                    prepare: start.elapsed(),
                    pause: Duration::ZERO,
                    authority: Authority {
                        worker: t.target,
                        generation: t.source.generation + 1,
                    },
                });
            }
            _ => return Err(Error::Fenced),
        };
        for m in &s.required {
            let Answer::Cursor {
                mut offset,
                base,
                complete,
            } = call(target, t, Command::Cache(m.clone()))?
            else {
                return Err(invalid("bulk cursor"));
            };
            if complete {
                continue;
            }
            if let Some(old) =
                base.filter(|old| offset == 0 && old.basis == m.basis && old.revision < m.revision)
            {
                if let Answer::Patch(Some(patch)) = call(
                    source,
                    t,
                    Command::MakeDelta {
                        old: old.clone(),
                        next: m.clone(),
                    },
                )? {
                    let cost = encode(&patch)?.len();
                    if cost < (m.length as usize) && payload.saturating_add(cost) <= budget {
                        call(
                            target,
                            t,
                            Command::InstallDelta {
                                old,
                                next: m.clone(),
                                patch,
                            },
                        )?;
                        payload += cost;
                        continue;
                    }
                }
            }
            loop {
                let limit = (m.length - offset).min(BLOCK as u64) as u32;
                if payload.saturating_add(limit as usize) > budget {
                    return Err(Error::Busy("migration transfer budget"));
                }
                let Response::Block { offset: at, bytes } = source.call(&Request::Bulk {
                    manifest: m.clone(),
                    offset,
                    limit: limit.max(1),
                })?
                else {
                    return Err(Error::Missing("source bulk version".into()));
                };
                if at != offset || bytes.len() != limit as usize {
                    return Err(invalid("incomplete source block"));
                }
                payload += bytes.len();
                let Answer::Cursor {
                    offset: next,
                    complete,
                    ..
                } = call(
                    target,
                    t,
                    Command::Chunk {
                        manifest: m.clone(),
                        offset,
                        bytes,
                    },
                )?
                else {
                    return Err(invalid("missing bulk acknowledgement"));
                };
                if complete {
                    break;
                }
                if next <= offset {
                    return Err(invalid("bulk transfer stalled"));
                }
                offset = next;
            }
        }
        let Answer::Staged(hash) = call(target, t, Command::Offer(s.clone()))? else {
            return Err(invalid("missing prepared receipt"));
        };
        let cut = Instant::now();
        let final_state = match call(source, t, Command::Freeze(s.required))? {
            Answer::Snapshot(s) => s,
            Answer::State(Phase::Preparing) => continue,
            _ => return Err(Error::Fenced),
        };
        pause.get_or_insert(cut);
        if final_state.hash()? != hash {
            call(target, t, Command::Offer(final_state))?;
        }
        call(target, t, Command::Ready)?;
        let Answer::Owner(authority) = call(target, t, Command::Switch)? else {
            return Err(Error::Fenced);
        };
        call(target, t, Command::Activate)?;
        let cut = pause.unwrap();
        return Ok(MigrationCost {
            payload,
            prepare: cut.duration_since(start),
            pause: cut.elapsed(),
            authority,
        });
    }
    Err(Error::Busy("source changes exceed preparation rate"))
}
