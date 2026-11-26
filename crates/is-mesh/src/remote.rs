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
enum Work {
    Subscriber(StreamPoll, Option<SubscriberPlan>),
    Rpc(Request),
    Sight(SightPlan),
}
/// Independent admission budgets for the simulation thread.
#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestBudgets {
    pub interactive: std::num::NonZeroUsize,
    pub per_session: std::num::NonZeroUsize,
    pub peers: std::num::NonZeroUsize,
    pub administration: std::num::NonZeroUsize,
}
impl Default for RequestBudgets {
    fn default() -> Self {
        Self {
            interactive: 32.try_into().unwrap(),
            per_session: 8.try_into().unwrap(),
            peers: 8.try_into().unwrap(),
            administration: 8.try_into().unwrap(),
        }
    }
}
type Job = (Role, Work, SyncSender<Result<Response>>, Instant);
struct SightAccess {
    rules: SightRanges,
    connect: Box<dyn Fn(Worker) -> Result<Client> + Send + Sync>,
    directory: Mutex<Option<(Worker, Client)>>,
}
struct Downloads {
    cache: BlobCache,
    partial: Option<(Handoff, Download)>,
}
/// Network threads perform bounded blob processing. Only the main thread touches Bevy.
pub struct WorkerEndpoint {
    sight: Option<SightAccess>,
    broker: Option<Mutex<crate::network_mesh::Broker>>,
    service: Arc<Service>,
    requests: SyncSender<Job>,
    peer_requests: SyncSender<Job>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    management: SyncSender<Job>,
    downloads: Arc<Mutex<Downloads>>,
}
pub struct WorkerLoop {
    tick_us: u64,
    pub cpu_budget: std::num::NonZeroUsize,
    process: ProcessLoad,
    sample: (Instant, u64),
    request_work: Duration,
    queue_wait: Duration,
    pub peers: Option<crate::network_mesh::PeerMesh>,
    service: Arc<Service>,
    rx: Receiver<Job>,
    peer_requests: Receiver<Job>,
    budgets: RequestBudgets,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    next_lane: u8,
    traffic: Vec<Arc<wire::Traffic>>,
    last_detail: WorkerTimings,
    management: Receiver<Job>,
    downloads: Arc<Mutex<Downloads>>,
    staged: Option<(Handoff, Prepared)>,
    paused: BTreeMap<Id, Handoff>,
    next_decision_check: Instant,
    sessions: BTreeMap<Id, VecDeque<Job>>,
    turns: VecDeque<Id>,
    queued: usize,
    streams: BTreeMap<SubscriberId, LiveStream>,
    encoder: SubscriberEncoder,
}
struct LiveStream {
    id: Id,
    view: SubscriberStream,
    plan: SubscriberPlan,
    last: Instant,
}
pub fn worker(service: Arc<Service>, initial: BlobCache) -> (Arc<WorkerEndpoint>, WorkerLoop) {
    make_worker(service, initial, None, RequestBudgets::default())
}
pub fn worker_with_sight(
    service: Arc<Service>,
    initial: BlobCache,
    rules: SightRanges,
    connect: impl Fn(Worker) -> Result<Client> + Send + Sync + 'static,
) -> (Arc<WorkerEndpoint>, WorkerLoop) {
    worker_with_budgets(service, initial, rules, connect, RequestBudgets::default())
}
pub fn worker_with_budgets(
    service: Arc<Service>,
    initial: BlobCache,
    rules: SightRanges,
    connect: impl Fn(Worker) -> Result<Client> + Send + Sync + 'static,
    budgets: RequestBudgets,
) -> (Arc<WorkerEndpoint>, WorkerLoop) {
    make_worker(
        service,
        initial,
        Some(SightAccess {
            rules,
            connect: Box::new(connect),
            directory: Mutex::default(),
        }),
        budgets,
    )
}
fn make_worker(
    service: Arc<Service>,
    initial: BlobCache,
    sight: Option<SightAccess>,
    budgets: RequestBudgets,
) -> (Arc<WorkerEndpoint>, WorkerLoop) {
    let (tx, rx) = mpsc::sync_channel(budgets.interactive.get());
    let (peer_tx, peer_rx) = mpsc::sync_channel(budgets.peers.get());
    let (management, admin) = mpsc::sync_channel(budgets.administration.get());
    let pending = Arc::<std::sync::atomic::AtomicUsize>::default();
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
            sight,
            broker,
            service: service.clone(),
            requests: tx,
            peer_requests: peer_tx,
            pending: pending.clone(),
            management,
            downloads: downloads.clone(),
        }),
        WorkerLoop {
            tick_us: 0,
            cpu_budget: std::thread::available_parallelism().unwrap_or(std::num::NonZeroUsize::MIN),
            process: ProcessLoad {
                resident_bytes: process_usage().1,
                ..Default::default()
            },
            sample: (Instant::now(), process_usage().0),
            request_work: Duration::ZERO,
            queue_wait: Duration::ZERO,
            peers: None,
            service,
            rx,
            peer_requests: peer_rx,
            budgets,
            pending,
            next_lane: 0,
            traffic: Vec::new(),
            last_detail: WorkerTimings::default(),
            management: admin,
            downloads,
            staged: None,
            paused: Default::default(),
            next_decision_check: Instant::now(),
            sessions: Default::default(),
            turns: Default::default(),
            queued: 0,
            streams: Default::default(),
            encoder: Default::default(),
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
    fn plan(&self, poll: StreamPoll, source: Worker) -> Result<Response> {
        self.with_viewer(poll.binding.object, |index, rules| {
            let version = (self.service.worker, index.sight_revision);
            if Some(version) == poll.known {
                return Ok(Response::SubscriberPlan(None));
            }
            let actor = poll.binding.object;
            let mut members = Vec::new();
            if index.presence(actor).is_some_and(|p| p.owner == source) {
                members.push(actor);
            }
            let limit = SubscriberPolicy::default().max_visible;
            let origin = index.visit_visible(
                actor,
                rules,
                |p| index.sight_points.get(&p.id).copied(),
                |id, w| {
                    if w == source && members.len() <= limit {
                        members.push(id);
                    }
                },
            )?;
            if members.len() > limit {
                return Err(Error::Busy("subscriber visibility exceeds source limit"));
            }
            Ok(Response::SubscriberPlan(Some(SubscriberPlan {
                version,
                source,
                origin,
                members,
            })))
        })
    }
    fn with_viewer<T>(
        &self,
        viewer: Id,
        read: impl FnOnce(&Directory, &SightRanges) -> Result<T>,
    ) -> Result<T> {
        let access = self
            .sight
            .as_ref()
            .ok_or_else(|| invalid("player sight is not configured"))?;
        let db = &self.service.store;
        db.check_worker(self.service.worker)?;
        let owner = db
            .authorities(&[viewer])?
            .get(&viewer)
            .copied()
            .ok_or(Error::Denied)?
            .1
            .worker;
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| invalid("use the directory worker for sight discovery"))?
            .lock()
            .map_err(|_| invalid("directory lock"))?;
        if broker.index.presence(viewer).map(|p| p.owner) != Some(owner) {
            return Err(Error::Fenced);
        }
        read(&broker.index, &access.rules)
    }
    fn discover(&self, viewer: Id) -> Result<Response> {
        let workers =
            self.with_viewer(viewer, |index, rules| index.sight_workers(viewer, rules))?;
        let mut sources = BTreeMap::new();
        for w in workers {
            sources.insert(w, self.service.store.address(w)?);
        }
        Ok(Response::SightSources(sources))
    }
    fn directory_call(&self, request: &Request) -> Result<Response> {
        let access = self
            .sight
            .as_ref()
            .ok_or_else(|| invalid("player sight is not configured"))?;
        let broker = self.service.store.worker(1)?;
        let mut socket = access
            .directory
            .lock()
            .map_err(|_| invalid("sight connection lock"))?;
        if socket.as_ref().is_none_or(|(w, _)| *w != broker) {
            *socket = Some((broker, (access.connect)(broker)?));
        }
        let reply = socket.as_mut().unwrap().1.call(request);
        if reply.is_err() {
            *socket = None;
        }
        reply
    }
    fn player_page(
        &self,
        viewer: Id,
        after: Option<Id>,
        known: Option<Digest>,
    ) -> Result<Response> {
        let here = self.service.worker;
        let plan = if self.broker.is_some() {
            self.with_viewer(viewer, |index, rules| {
                index.sight_plan(viewer, rules, here, after)
            })?
        } else {
            match self.directory_call(&Request::SightPlan { viewer, after })? {
                Response::SightPlan(plan) => plan,
                Response::Error(e) => return Err(invalid(e)),
                _ => return Err(invalid("expected sight plan")),
            }
        };
        let response = self.enqueue(Role::Player(viewer), Work::Sight(plan))?;
        // Hash on the socket thread, after the simulation has released the public page.
        if let (Some(known), Response::SightPage(page)) = (known, &response) {
            if known == page.content_digest() {
                return Ok(Response::SightUnchanged { next: page.next });
            }
        }
        Ok(response)
    }
    fn enqueue(&self, role: Role, work: Work) -> Result<Response> {
        self.try_enqueue(role, work)?
            .recv_timeout(Duration::from_secs(4))
            .map_err(|_| Error::Busy("response timeout; retry same action or transfer"))?
    }
    fn try_enqueue(&self, role: Role, work: Work) -> Result<Receiver<Result<Response>>> {
        let (tx, rx) = mpsc::sync_channel(1);
        let queue = if matches!(
            work,
            Work::Rpc(
                Request::Health
                    | Request::Telemetry
                    | Request::Shutdown
                    | Request::Load { .. }
                    | Request::Control(..)
            )
        ) {
            &self.management
        } else if matches!(work, Work::Rpc(Request::Push(_))) {
            &self.peer_requests
        } else {
            &self.requests
        };
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if queue.try_send((role, work, tx, Instant::now())).is_err() {
            self.pending
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            return Err(Error::Busy("worker request queue"));
        }
        Ok(rx)
    }
    /// Inspect the directory on its host when it is idle. No requests or sampling
    /// are scheduled by the mesh. Keep the callback short as it borrows the index.
    pub fn try_inspect_directory<T>(&self, read: impl FnOnce(&crate::Directory) -> T) -> Option<T> {
        let broker = self.broker.as_ref()?.try_lock().ok()?;
        Some(read(&broker.index))
    }

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
            Request::SightSources => {
                let Role::Player(viewer) = role else {
                    return Err(Error::Denied);
                };
                return if self.broker.is_some() {
                    self.discover(viewer)
                } else {
                    self.directory_call(&Request::SightDiscover(viewer))
                };
            }
            Request::SightDiscover(viewer) => {
                let Role::Worker(source) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(source)?;
                return self.discover(*viewer);
            }
            Request::SightPlan { viewer, after } => {
                let Role::Worker(source) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(source)?;
                return self
                    .with_viewer(*viewer, |index, rules| {
                        index.sight_plan(*viewer, rules, source, *after)
                    })
                    .map(Response::SightPlan);
            }
            Request::SightPage { after, known } => {
                let Role::Player(viewer) = role else {
                    return Err(Error::Denied);
                };
                return self.player_page(viewer, *after, *known);
            }
            Request::Health | Request::Telemetry | Request::Shutdown => {
                if role != Role::Operator {
                    return Err(Error::Denied);
                }
            }
            Request::Load { required, .. } => {
                if role != Role::Operator {
                    return Err(Error::Denied);
                }
                if required.len() > MAX_TARGETS {
                    return Err(Error::Busy("placement probe manifests"));
                }
            }
            Request::SessionInputs(inputs) => {
                let Role::Worker(source) = role else {
                    return Err(Error::Denied);
                };
                if inputs.len() > MAX_TARGETS {
                    return Err(Error::Busy("routed input batch"));
                }
                self.service.store.check_worker(source)?;
            }
            Request::SessionProgress(_) => {
                let Role::Worker(source) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(source)?;
            }
            Request::SubscriberSources(binding, subscriber) => {
                let Role::Worker(w) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(w)?;
                let directory = SessionDirectory::persistent(self.service.store.clone());
                if !directory.authorized_subscriber(*binding, *subscriber)? {
                    return Err(Error::Denied);
                }
                return if self.broker.is_none() {
                    self.directory_call(&request)
                } else {
                    let Response::SightSources(mut sources) = self.discover(binding.object)? else {
                        return Err(invalid("source discovery"));
                    };
                    let owner = self
                        .service
                        .store
                        .authorities(&[binding.object])?
                        .get(&binding.object)
                        .ok_or(Error::Fenced)?
                        .1
                        .worker;
                    sources.insert(owner, self.service.store.address(owner)?);
                    Ok(Response::SightSources(sources))
                };
            }
            Request::SubscriberPlan(poll, source) => {
                let Role::Worker(w) = role else {
                    return Err(Error::Denied);
                };
                if *source != w {
                    return Err(Error::Denied);
                }
                self.service.store.check_worker(w)?;
                if !SessionDirectory::persistent(self.service.store.clone())
                    .authorized_subscriber(poll.binding, poll.subscriber)?
                {
                    return Err(Error::Denied);
                }
                return self.plan(*poll, *source);
            }
            Request::SubscriberPoll(poll) => {
                let Role::Worker(w) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(w)?;
                if !SessionDirectory::persistent(self.service.store.clone())
                    .authorized_subscriber(poll.binding, poll.subscriber)?
                {
                    return Err(Error::Denied);
                }
                let response = if self.broker.is_some() {
                    self.plan(*poll, self.service.worker)?
                } else {
                    self.directory_call(&Request::SubscriberPlan(*poll, self.service.worker))?
                };
                let Response::SubscriberPlan(plan) = response else {
                    return Err(invalid("subscriber directory unavailable"));
                };
                return self.enqueue(role, Work::Subscriber(*poll, plan));
            }
            Request::Control(t, c) => {
                allowed(&self.service, role, *t)?;
                if let Some(a) = self.bulk(*t, c)? {
                    return Ok(Response::Control(a));
                }
            }
            Request::Push(_) => {
                let Role::Worker(source) = role else {
                    return Err(Error::Denied);
                };
                self.service.store.check_worker(source)?;
            }
            Request::Sync { .. }
            | Request::Diagnostics
            | Request::Input(_)
            | Request::Actions(_) => {}
            _ => return self.service.dispatch(role, request),
        }
        self.enqueue(role, Work::Rpc(request))
    }
}
fn exact(world: &World, db: &Store, t: Handoff, view: View) -> Result<Option<Snapshot>> {
    let mut snapshot = db.snapshot(t.group)?;
    let Some((owner, revision)) = world.resource::<MeshState>().group_state(t.group) else {
        db.check_authority(t.group, t.source, false)?;
        if snapshot.records.is_empty() {
            return Ok(Some(snapshot));
        }
        return Ok(None);
    };
    if owner != t.source {
        return Err(Error::Fenced);
    }
    // A committed action can precede the source's next ECS tick, including a
    // spawn or despawn. Never capture an older World under that newer revision.
    if revision != snapshot.revision {
        return Ok(None);
    }
    for r in snapshot.records.iter_mut().filter(|r| !r.deleted) {
        let item = capture(world, r.id, view)?;
        if r.revision != item.revision {
            return Ok(None);
        }
        *r = item;
    }
    Ok(Some(snapshot))
}
fn flush_inputs(
    w: &mut World,
    samples: &mut Vec<(Id, Input)>,
    replies: &mut Vec<SyncSender<Result<Response>>>,
) {
    if samples.is_empty() {
        return;
    }
    let start = Instant::now();
    let batch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("system clock before Unix epoch"))
        .and_then(|t| apply_inputs(w, samples, t.as_millis() as u64));
    match batch {
        Ok(results) => {
            for (reply, result) in replies.drain(..).zip(results) {
                let _ = reply.send(result.map(Response::Input));
            }
        }
        Err(e) => {
            let failure = crate::authority_store::Failure::from(e);
            for reply in replies.drain(..) {
                let _ = reply.send(Err(failure.clone().into()));
            }
        }
    }
    samples.clear();
    if let Some(mut metrics) = w.get_resource_mut::<WorkerTimings>() {
        metrics.input_us += start.elapsed().as_micros() as u64;
    }
}
impl WorkerLoop {
    /// Register independent socket counters once, before serving.
    pub fn observe_transport(&mut self, traffic: Arc<wire::Traffic>) {
        if !self.traffic.iter().any(|t| Arc::ptr_eq(t, &traffic)) {
            self.traffic.push(traffic);
        }
    }
    fn next_job(&mut self) -> Option<Job> {
        // Across pump calls, every ready lane gets a turn even with a one-job time slice.
        for _ in 0..3 {
            let lane = self.next_lane;
            self.next_lane = (lane + 1) % 3;
            let job = match lane {
                0 => self.management.try_recv().ok(),
                1 => self.peer_requests.try_recv().ok(),
                _ => self.turns.pop_front().and_then(|session| {
                    let q = self.sessions.get_mut(&session)?;
                    let job = q.pop_front()?;
                    if q.is_empty() {
                        self.sessions.remove(&session);
                    } else {
                        self.turns.push_back(session);
                    }
                    self.queued -= 1;
                    Some(job)
                }),
            };
            if job.is_some() {
                return job;
            }
        }
        None
    }
    pub fn record_tick(&mut self, elapsed: Duration) {
        self.record_cycle(elapsed, Duration::ZERO, Duration::ZERO, Default::default());
    }
    pub fn record_cycle(
        &mut self,
        ecs: Duration,
        peers: Duration,
        lateness: Duration,
        detail: WorkerTimings,
    ) {
        let elapsed = ecs + peers;
        let requests = std::mem::take(&mut self.request_work);
        let queue = std::mem::take(&mut self.queue_wait);
        self.process.simulation_and_peer_us = elapsed.as_micros() as u64;
        self.process.request_work_us = requests.as_micros() as u64;
        self.process.queue_wait_us = queue.as_micros() as u64;
        // ECS/peer and pump execute consecutively. Background CPU runs in parallel.
        let busy = elapsed + requests;
        self.process.ecs_us = ecs.as_micros() as u64;
        self.process.peer_us = peers.as_micros() as u64;
        self.process.checkpoint_us = detail.checkpoint_us;
        self.process.input_us = detail.input_us;
        self.process.action_us = detail.action_us;
        self.process.migration_us = detail.migration_us;
        self.last_detail = detail;
        if busy.max(elapsed + lateness) > Duration::from_millis(50) {
            self.process.deadline_misses += 1;
        }
        let sample_elapsed = self.sample.0.elapsed();
        if sample_elapsed >= Duration::from_millis(500) {
            let (cpu, rss) = process_usage();
            self.process.cpu_permille = (u128::from(cpu.saturating_sub(self.sample.1)) * 1000
                / sample_elapsed.as_nanos()) as u64;
            self.process.resident_bytes = rss;
            self.sample = (Instant::now(), cpu);
        }
        let cpu_us = self.process.cpu_permille.saturating_mul(50) / self.cpu_budget.get() as u64;
        let demand = (busy.as_micros() as u64)
            .max(cpu_us)
            .max(self.process.queue_wait_us);
        self.tick_us = (self.tick_us * 7 + demand) / 8;
    }
    /// A bounded round-robin turn per logical session. One atomic request may finish
    /// after the soft deadline. Both its target count and its bytes have hard limits.
    pub fn pump(&mut self, w: &mut World, max: usize, time: Duration) {
        let pump_start = Instant::now();
        self.process.pending_requests = self.pending.load(std::sync::atomic::Ordering::Relaxed);
        self.process.max_queue_depth = self
            .process
            .max_queue_depth
            .max(self.process.pending_requests);
        if Instant::now() >= self.next_decision_check {
            self.streams
                .retain(|_, s| s.last.elapsed() < Duration::from_secs(60));
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
        for _ in self.queued..self.budgets.interactive.get() {
            let Ok(job) = self.rx.try_recv() else {
                break;
            };
            let session = match &job.1 {
                Work::Rpc(Request::Actions(a)) => {
                    a.first().map(|i| i.key.session).unwrap_or(Id::numbered(0))
                }
                Work::Rpc(Request::Input(i)) => i.session,
                Work::Rpc(Request::SessionInputs(batch)) => batch
                    .first()
                    .map_or(Id::numbered(0), |i| i.binding.session.0),
                Work::Rpc(Request::SessionProgress(binding)) => binding.session.0,
                Work::Rpc(Request::Sync { session, .. }) => *session,
                Work::Sight(_) => match job.0 {
                    Role::Player(id) => id,
                    _ => Id::numbered(0),
                },
                Work::Subscriber(poll, _) => poll.binding.session.0,
                _ => Id::numbered(0),
            };
            let queue = self.sessions.entry(session).or_default();
            if queue.len() >= self.budgets.per_session.get() {
                self.pending
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let _ = job.2.send(Err(Error::Busy("per-session request queue")));
                continue;
            }
            if queue.is_empty() {
                self.turns.push_back(session);
            }
            queue.push_back(job);
            self.queued += 1;
            self.process.max_queue_depth = self.process.max_queue_depth.max(self.queued);
        }
        if let Some(oldest) = self
            .sessions
            .values()
            .filter_map(|q| q.front())
            .map(|j| j.3.elapsed())
            .max()
        {
            self.queue_wait = self.queue_wait.max(oldest);
        }
        let mut samples = Vec::new();
        let mut replies = Vec::new();
        for _ in 0..max {
            if start.elapsed() >= time {
                break;
            }
            let Some((role, req, reply, enqueued)) = self.next_job() else {
                break;
            };
            self.process.pending_requests = self
                .pending
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                - 1;
            self.queue_wait = self.queue_wait.max(enqueued.elapsed());
            let req = match req {
                Work::Rpc(r) => r,
                Work::Subscriber(poll, plan) => {
                    flush_inputs(w, &mut samples, &mut replies);
                    let start = Instant::now();
                    let result = self.stream(w, poll, plan);
                    if let Some(mut t) = w.get_resource_mut::<WorkerTimings>() {
                        t.subscriber_us += start.elapsed().as_micros() as u64;
                    }
                    let _ = reply.send(result);
                    continue;
                }
                Work::Sight(plan) => {
                    flush_inputs(w, &mut samples, &mut replies);
                    let start = Instant::now();
                    let result = match role {
                        Role::Player(viewer) => {
                            crate::sight::sight_page(w, viewer, self.service.worker, plan)
                                .map(Response::SightPage)
                        }
                        _ => Err(Error::Denied),
                    };
                    if let Some(mut t) = w.get_resource_mut::<WorkerTimings>() {
                        t.subscriber_us += start.elapsed().as_micros() as u64;
                    }
                    let _ = reply.send(result);
                    continue;
                }
            };
            if let Request::Input(input) = req {
                if let Role::Player(actor) = role {
                    samples.push((actor, input));
                    replies.push(reply);
                } else {
                    let _ = reply.send(Err(Error::Denied));
                }
                continue;
            }
            flush_inputs(w, &mut samples, &mut replies);
            let start_request = Instant::now();
            let action = matches!(req, Request::Actions(_));
            let migration = matches!(req, Request::Control(..));
            let result = match req {
                Request::Telemetry => {
                    let mut result = WorkerTelemetry {
                        process: self.process,
                        spatial_routing_us: self.last_detail.spatial_routing_us,
                        peer_publish_us: self.last_detail.peer_publish_us,
                        subscriber_us: self.last_detail.subscriber_us,
                        pending_input_bindings: w
                            .get_resource::<SessionIngress>()
                            .map_or(0, SessionIngress::pending),
                        pending_checkpoint_bytes: w
                            .get_resource::<AsyncCheckpoints>()
                            .map_or(0, |c| c.pending().1),
                        subscriber_unacked_bytes: self
                            .streams
                            .values()
                            .map(|s| s.view.unacked_bytes())
                            .sum(),
                        ..Default::default()
                    };
                    for counters in &self.traffic {
                        counters.add_to(&mut result);
                    }
                    Ok(Response::Telemetry(result))
                }
                Request::Health => self.service.store.drain_status(self.service.worker).map(
                    |(draining, groups)| Response::Health {
                        worker: self.service.worker,
                        ready: !draining && w.resource::<MeshState>().tick() > 0,
                        groups,
                        load: self.process,
                    },
                ),
                Request::Shutdown => self
                    .service
                    .store
                    .retire(self.service.worker)
                    .map(|_| Response::Stopped),
                Request::SessionProgress(binding) => {
                    confirmed_session_input(w, binding).map(Response::Input)
                }
                Request::SessionInputs(inputs) => (|| {
                    let ingress = w
                        .get_resource::<SessionIngress>()
                        .ok_or_else(|| invalid("external session ingress missing"))?;
                    for input in inputs {
                        let state = w.resource::<MeshState>();
                        let authoritative = state
                            .entities
                            .get(&input.binding.object)
                            .is_some_and(|e| w.get::<Simulated>(*e).is_some());
                        if !authoritative {
                            return Err(Error::Fenced);
                        }
                        ingress.submit(input)?;
                    }
                    Ok(Response::Approved)
                })(),
                Request::Load { group, required } => {
                    let cache = w.resource::<BlobCache>();
                    let policy = w
                        .get_resource::<BalancePolicy>()
                        .copied()
                        .unwrap_or_default();
                    let hints = w.get_resource::<GroupTimings>();
                    Ok(Response::Load(WorkerLoad {
                        worker: self.service.worker,
                        budget_us: policy.tick_budget_us,
                        memory_bytes: policy.memory_bytes,
                        tick_us: self
                            .tick_us
                            .max(hints.and_then(|h| h.0.values().copied().max()).unwrap_or(0)),
                        cache_bytes: cache.bytes() as u64,
                        process: self.process,
                        groups: w.resource::<MeshState>().owned_groups().count(),
                        group_us: group.and_then(|g| hints?.0.get(&g).copied()),
                        affinity: group
                            .and_then(|g| Some(self.peers.as_ref()?.affinity(g)))
                            .unwrap_or_default(),
                        cached_required: required
                            .iter()
                            .filter(|m| cache.ready(m))
                            .map(|m| m.length)
                            .sum(),
                    }))
                }
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
                Request::Input(_) => unreachable!("input already queued"),
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
            if action || migration {
                if let Some(mut t) = w.get_resource_mut::<WorkerTimings>() {
                    let us = start_request.elapsed().as_micros() as u64;
                    if action {
                        t.action_us += us;
                    } else {
                        t.migration_us += us;
                    }
                }
            }
        }
        flush_inputs(w, &mut samples, &mut replies);
        self.request_work += pump_start.elapsed();
    }
    fn stream(
        &mut self,
        w: &mut World,
        poll: StreamPoll,
        plan: Option<SubscriberPlan>,
    ) -> Result<Response> {
        let directory = w
            .get_resource::<SessionDirectory>()
            .ok_or_else(|| invalid("external session plugin missing"))?
            .clone();
        if !directory.authorized_subscriber(poll.binding, poll.subscriber)? {
            return Err(Error::Denied);
        }
        w.init_resource::<SubscriberSource>();
        let existing = self.streams.get(&poll.subscriber);
        if existing.is_none_or(|s| Some(s.id) != poll.stream) {
            let Some(plan) = plan else {
                return Ok(Response::RefreshPlan);
            };
            if existing.is_none() && self.streams.len() >= w.resource::<SessionIngress>().capacity()
            {
                return Err(Error::Busy("worker subscriber capacity"));
            }
            let live = LiveStream {
                id: Id::new(),
                view: SubscriberStream::open(
                    &directory,
                    poll.binding,
                    poll.subscriber,
                    w.get_resource::<SubscriberPolicy>()
                        .copied()
                        .unwrap_or_default(),
                )?,
                plan,
                last: Instant::now(),
            };
            self.streams.insert(poll.subscriber, live);
        } else {
            let live = self.streams.get_mut(&poll.subscriber).unwrap();
            if let Some(plan) = plan {
                live.plan = plan;
            } else if Some(live.plan.version) != poll.known {
                return Ok(Response::RefreshPlan);
            }
            live.view.poll_binding(&directory, poll)?;
        }
        let live = self.streams.get_mut(&poll.subscriber).unwrap();
        live.last = Instant::now();
        let frame = live.view.next_worker(w, &live.plan, &mut self.encoder)?;
        Ok(Response::Subscriber(StreamReply {
            stream: live.id,
            source: self.service.worker,
            version: live.plan.version,
            frame,
        }))
    }
    fn control(&mut self, w: &mut World, role: Role, t: Handoff, c: Command) -> Result<Answer> {
        allowed(&self.service, role, t)?;
        let db = &self.service.store;
        let here = self.service.worker;
        match c {
            Command::Preview if here == t.source.worker => {
                db.begin_handoff(t)?;
                match db.handoff_phase(t)? {
                    Phase::Preparing => Ok(exact(w, db, t, View::Migration)?
                        .map_or(Answer::State(Phase::Preparing), Answer::Snapshot)),
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
                if w.contains_resource::<AsyncCheckpoints>() {
                    checkpoint_group(w, t.group)?;
                }
                let Some(s) = exact(w, db, t, View::Migration)? else {
                    return Ok(Answer::State(Phase::Preparing));
                };
                if s.required != known {
                    return Ok(Answer::State(Phase::Preparing));
                }
                let Some(checkpoints) = exact(w, db, t, View::Checkpoint)? else {
                    return Ok(Answer::State(Phase::Preparing));
                };
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
        let deadline = Instant::now() + Duration::from_secs(4);
        let s = loop {
            match call(source, t, Command::Preview)? {
                Answer::Snapshot(s) => break s,
                Answer::State(Phase::Preparing) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Busy(
                            "source world has not reached the committed revision",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
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
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saturated_sessions_leave_peer_and_operator_turns_available() -> Result<()> {
        let db = Store::open(":memory:")?;
        let here = db.start_worker(1)?;
        let other = db.start_worker(2)?;
        let mut app = bevy_app::App::new();
        app.add_plugins(MeshPlugin::persistent(db.clone(), here));
        app.update();
        let service = Arc::new(Service {
            worker: here,
            store: db.clone(),
            actions: Default::default(),
            providers: Default::default(),
            replicas: Default::default(),
            bulk: Default::default(),
        });
        let (endpoint, mut worker) = worker(service, BlobCache::default());
        worker.peers = Some(crate::network_mesh::PeerMesh::new(db, here, |_| {
            Err(Error::Denied)
        }));
        let player = Role::Player(Id::numbered(0));
        let job = || Work::Rpc(Request::Diagnostics);
        let mut public = Vec::new();
        for _ in 0..worker.budgets.interactive.get() {
            public.push(endpoint.try_enqueue(player, job())?);
        }
        assert!(matches!(
            endpoint.try_enqueue(player, job()),
            Err(Error::Busy(_))
        ));
        assert!(matches!(
            endpoint.dispatch(player, Request::Push(vec![])),
            Err(Error::Denied)
        ));
        assert!(matches!(
            endpoint.dispatch(player, Request::Telemetry),
            Err(Error::Denied)
        ));
        let peer = endpoint.try_enqueue(Role::Worker(other), Work::Rpc(Request::Push(vec![])))?;
        let admin = endpoint.try_enqueue(Role::Operator, Work::Rpc(Request::Health))?;
        for _ in 0..3 {
            worker.pump(app.world_mut(), 1, Duration::from_secs(1));
        }
        assert!(matches!(
            admin.try_recv().unwrap()?,
            Response::Health { ready: true, .. }
        ));
        assert!(matches!(peer.try_recv().unwrap()?, Response::Applied(_)));
        assert!(matches!(public[0].try_recv().unwrap(), Err(Error::Denied)));
        assert!(matches!(
            public.last().unwrap().try_recv().unwrap(),
            Err(Error::Busy(_))
        ));
        // Placement must see work performed between ECS ticks, including backpressure.
        worker.queue_wait = Duration::from_millis(70);
        worker.request_work += Duration::from_millis(55);
        worker.record_tick(Duration::from_micros(1));
        assert!(worker.process.request_work_us >= 55_000);
        assert_eq!(worker.process.deadline_misses, 1);
        assert!(worker.tick_us >= 70_000 / 8);
        assert!(worker.process.max_queue_depth >= 34);
        Ok(())
    }
    #[test]
    fn preview_and_freeze_wait_for_a_committed_spawn_before_the_next_tick() -> Result<()> {
        let db = Store::open(":memory:")?;
        let from = db.start_worker(1)?;
        let to = db.start_worker(2)?;
        let group = Id::new();
        let source = db.create_group(group, from)?;
        let rev = db.checkpoint(group, source, 0, vec![Record::new(group, group)])?;
        let mut app = bevy_app::App::new();
        app.add_plugins(MeshPlugin::persistent(db.clone(), from));
        restore_group(app.world_mut(), group)?;
        let service = Arc::new(Service {
            worker: from,
            store: db.clone(),
            actions: Default::default(),
            providers: Default::default(),
            replicas: Default::default(),
            bulk: Default::default(),
        });
        let (_, mut worker) = worker(service, BlobCache::default());
        let t = Handoff {
            id: Id::new(),
            group,
            source,
            target: to,
        };
        let added = Id::new();
        let latest = db.checkpoint(group, source, rev, vec![Record::new(added, group)])?;
        for cmd in [Command::Preview, Command::Freeze(vec![])] {
            assert!(matches!(
                worker.control(app.world_mut(), Role::Operator, t, cmd)?,
                Answer::State(Phase::Preparing)
            ));
            assert_eq!(db.handoff_phase(t)?, Phase::Preparing);
        }
        app.update();
        for cmd in [Command::Preview, Command::Freeze(vec![])] {
            let Answer::Snapshot(snapshot) =
                worker.control(app.world_mut(), Role::Operator, t, cmd)?
            else {
                panic!("source did not catch up")
            };
            assert!(snapshot.revision >= latest && snapshot.records.iter().any(|r| r.id == added));
        }
        assert_eq!(db.handoff_phase(t)?, Phase::Frozen);
        assert_eq!(db.owner(group)?, source);
        Ok(())
    }
}
