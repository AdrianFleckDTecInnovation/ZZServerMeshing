//! Coarse directory reports and directed peer delivery for independent processes.
mod description;
use crate::{
    wire::{Client, Request, Response, Role},
    *,
};
use bevy_ecs::world::World;
pub use description::Description;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
};

const LINKS: usize = 8192;
pub(crate) fn describe<const REPORT_SIGHT: bool>(
    w: &World,
    worker: Worker,
    id: Id,
) -> Option<Description> {
    let &e = w.resource::<MeshState>().entities.get(&id)?;
    w.get::<Simulated>(e)?;
    let p = (w.get_resource::<SpatialRule>()?.0)(w, e)?;
    let presence = Presence {
        id,
        owner: worker,
        principal: p.principal,
        velocity: p.velocity,
        ahead: p.ahead,
        bounds: p.bounds,
    };
    let area = p.active.then(|| Area {
        id,
        worker,
        to: Recipient::Worker(worker),
        bounds: p.bounds,
        velocity: p.velocity,
        ahead: p.ahead,
        hysteresis: p.retention,
        activity: true,
        components: p.needs,
    });
    Some(Description {
        presence,
        area,
        group: w.get::<Managed>(e)?.group,
        sight: if REPORT_SIGHT {
            w.get_resource::<SightRule>()
                .and_then(|rule| (rule.0)(w, e))
                .filter(|(frame, _)| *frame == presence.bounds.frame)
                .map(|(_, point)| point)
        } else {
            None
        },
    })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub directory: Option<Worker>,
    pub sequence: u64,
    pub ack: u64,
    pub changes: Vec<(Id, Option<Description>)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub source: Worker,
    pub change: InterestChange,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Routes {
    pub directory: Worker,
    pub sequence: u64,
    pub through: u64,
    pub reset: bool,
    pub edges: Vec<Route>,
}
type Edge = (Recipient, Id);
/// Replaces unacknowledged route changes by the newest epoch for that edge.
pub struct Broker {
    here: Worker,
    db: Store,
    pub(crate) index: Directory,
    sources: BTreeMap<Edge, Worker>,
    pending: BTreeMap<Worker, BTreeMap<(Worker, Recipient, Id), Route>>,
    needs_snapshot: BTreeSet<Worker>,
    reset_seq: BTreeMap<Worker, u64>,
    members: BTreeMap<Worker, BTreeSet<Id>>,
    seq: BTreeMap<Worker, u64>,
    slots: BTreeMap<u32, Worker>,
}
impl Broker {
    pub fn new(db: Store, here: Worker) -> Result<Self> {
        Ok(Self {
            here,
            db,
            index: Directory::for_incarnation(here.incarnation)?,
            sources: Default::default(),
            pending: Default::default(),
            needs_snapshot: Default::default(),
            reset_seq: Default::default(),
            members: Default::default(),
            seq: Default::default(),
            slots: Default::default(),
        })
    }
    fn queue(&mut self, source: Worker, change: InterestChange, receiver: bool) {
        if receiver {
            if let Recipient::Worker(w) = change.to {
                self.enqueue(
                    w,
                    Route {
                        source,
                        change: change.clone(),
                    },
                );
            }
        }
        self.enqueue(source, Route { source, change });
    }
    fn enqueue(&mut self, w: Worker, route: Route) {
        if self.needs_snapshot.contains(&w) || self.slots.get(&w.slot) != Some(&w) {
            return;
        }
        let key = (route.source, route.change.to, route.change.object);
        let queue = self.pending.entry(w).or_default();
        if queue.len() >= LINKS && !queue.contains_key(&key) {
            queue.clear();
            self.needs_snapshot.insert(w);
            return;
        }
        queue.insert(key, route);
    }
    pub fn report(&mut self, role: Role, r: Report) -> Result<Routes> {
        let Role::Worker(from) = role else {
            return Err(Error::Denied);
        };
        self.db.check_worker(self.here)?;
        self.db.check_worker(from)?;
        let empty = Routes {
            directory: self.here,
            sequence: 0,
            through: 0,
            reset: false,
            edges: vec![],
        };
        if r.directory != Some(self.here) {
            return Ok(empty);
        }
        if r.changes.len() > MAX_TARGETS {
            return Err(Error::Busy("directory report"));
        }
        if !self.slots.contains_key(&from.slot) && self.slots.len() >= 64 {
            return Err(Error::Busy("directory worker capacity"));
        }
        if let Some(old) = self.slots.insert(from.slot, from) {
            if old != from {
                for id in self.members.remove(&old).unwrap_or_default() {
                    self.index.remove_owned(old, id);
                }
                self.pending.remove(&old);
                self.seq.remove(&old);
                self.needs_snapshot.remove(&old);
                self.reset_seq.remove(&old);
            }
        }
        if let Some(q) = self.pending.get_mut(&from) {
            if self.reset_seq.get(&from) != Some(&r.sequence) {
                q.retain(|_, rte| rte.change.epoch > r.ack);
            }
        }
        let seq = *self.seq.get(&from).unwrap_or(&0);
        if seq + 1 == r.sequence {
            let ids: Vec<_> = r
                .changes
                .iter()
                .filter(|(_, d)| d.is_some())
                .map(|(id, _)| *id)
                .collect();
            let owners = self.db.authorities(&ids)?;
            let members = self.members.entry(from).or_default();
            self.index.batch(|index| {
                for (id, d) in r.changes {
                    if let Some(d) = d {
                        if d.presence.owner != from
                            || d.presence.id != id
                            || d.area.as_ref().is_some_and(|a| {
                                a.id != id || a.worker != from || a.to != Recipient::Worker(from)
                            })
                            || !owners
                                .get(&id)
                                .is_some_and(|(g, a)| *g == d.group && a.worker == from)
                        {
                            continue;
                        }
                        index.describe(d)?;
                        members.insert(id);
                    } else {
                        index.remove_owned(from, id);
                        members.remove(&id);
                    }
                }
                Ok(())
            })?;
            for c in self.index.changes() {
                let key = (c.to, c.object);
                let prior = self.sources.remove(&key);
                if let Some(old) = prior {
                    if c.subscription.is_none() {
                        self.queue(old, c.clone(), true);
                        continue;
                    }
                    if !self
                        .index
                        .presence(c.object)
                        .is_some_and(|p| p.owner == old)
                    {
                        self.queue(
                            old,
                            InterestChange {
                                subscription: None,
                                ..c.clone()
                            },
                            false,
                        );
                    }
                }
                if c.subscription.is_some() {
                    if let Some(p) = self.index.presence(c.object) {
                        self.sources.insert(key, p.owner);
                        self.queue(p.owner, c, true);
                    }
                }
            }
            self.seq.insert(from, r.sequence);
        } else if seq != r.sequence {
            return Err(invalid("directory report sequence gap"));
        }
        if self.needs_snapshot.contains(&from) {
            let mut live = BTreeMap::new();
            for (&(to, id), &source) in &self.sources {
                if source == from || to == Recipient::Worker(from) {
                    if live.len() >= LINKS {
                        return Err(Error::Busy("active subscriptions exceed peer capacity"));
                    }
                    let sub = self
                        .index
                        .edges(id)
                        .and_then(|e| e.get(&to))
                        .ok_or_else(|| invalid("directory edge missing"))?;
                    live.insert(
                        (source, to, id),
                        Route {
                            source,
                            change: InterestChange {
                                object: id,
                                to,
                                epoch: sub.epoch,
                                subscription: Some(sub.clone()),
                            },
                        },
                    );
                }
            }
            self.pending.insert(from, live);
            self.needs_snapshot.remove(&from);
            self.reset_seq.insert(from, r.sequence);
        }
        let mut edges: Vec<_> = self
            .pending
            .get(&from)
            .into_iter()
            .flat_map(|q| q.values())
            .cloned()
            .collect();
        if edges.len() > LINKS {
            return Err(Error::Busy("directory route quota"));
        }
        edges.sort_by_key(|r| r.change.epoch);
        edges.truncate(MAX_TARGETS);
        let through = edges.last().map_or(r.ack, |e| e.change.epoch);
        Ok(Routes {
            directory: self.here,
            sequence: r.sequence,
            through,
            reset: self.reset_seq.get(&from) == Some(&r.sequence),
            edges,
        })
    }
}
type Connector = Arc<dyn Fn(Worker) -> Result<Client> + Send + Sync>;
#[derive(Clone, Copy)]
enum Destination {
    Directory,
    Worker(Worker),
}
type Task = (Destination, Request);
/// Advance over a continuously changing backlog. An ACK removes only matching
/// revisions, so a hot prefix can otherwise occupy every single report.
fn report_page(pending: &BTreeMap<Id, u64>, cursor: &mut Option<Id>) -> Vec<(Id, u64)> {
    use std::ops::Bound::{Excluded, Unbounded};
    let start = cursor.map_or(Unbounded, Excluded);
    let page: Vec<_> = pending
        .range((start, Unbounded))
        .chain(pending.iter())
        .take(MAX_TARGETS.min(pending.len()))
        .map(|(&id, &revision)| (id, revision))
        .collect();
    if let Some(&(last, _)) = page.last() {
        *cursor = Some(last);
    }
    page
}
struct Lane {
    send: SyncSender<Task>,
    receive: Receiver<(Destination, Result<Response>, Option<Report>)>,
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn continuously_moving_prefix_cannot_starve_the_last_directory_page() {
        let n = MAX_TARGETS * 3 + 7;
        let mut pending: BTreeMap<_, _> = (0..n).map(|i| (Id::numbered(i as u128), 1)).collect();
        let db = Store::open(":memory:").unwrap();
        let directory = db.start_worker(1).unwrap();
        let from = db.start_worker(2).unwrap();
        let group = Id::numbered(1000);
        let owner = db.create_group(group, from).unwrap();
        let ids: Vec<_> = pending.keys().copied().collect();
        let mut revision = 0;
        for batch in ids.chunks(MAX_TARGETS) {
            let records = batch.iter().map(|&id| Record::new(id, group)).collect();
            revision = db.checkpoint(group, owner, revision, records).unwrap();
        }
        let mut broker = Broker::new(db, directory).unwrap();
        let mut cursor = None;
        for tick in 1..=n.div_ceil(MAX_TARGETS) {
            pending.values_mut().for_each(|v| *v = tick as u64);
            let page = report_page(&pending, &mut cursor);
            assert_eq!(page.len(), MAX_TARGETS);
            assert!(page.iter().all(|(_, v)| *v == tick as u64));
            assert_eq!(
                page.iter().copied().collect::<BTreeSet<_>>().len(),
                page.len()
            );
            let changes = page
                .iter()
                .map(|&(id, revision)| {
                    let point = [revision as i64; 3];
                    let presence = Presence {
                        id,
                        owner: from,
                        principal: id,
                        bounds: Box3::new(group, point, point).unwrap(),
                        velocity: [0; 3],
                        ahead: 0,
                    };
                    (
                        id,
                        Some(Description {
                            presence,
                            group,
                            area: None,
                            sight: None,
                        }),
                    )
                })
                .collect();
            let report = Report {
                directory: Some(directory),
                sequence: tick as u64,
                ack: 0,
                changes,
            };
            for _ in 0..2 {
                assert_eq!(
                    broker
                        .report(Role::Worker(from), report.clone())
                        .unwrap()
                        .sequence,
                    tick as u64
                );
            }
        }
        assert!(pending
            .keys()
            .all(|&id| broker.index.presence(id).is_some()));
        pending.retain(|id, _| u128::from_be_bytes(id.0) % 101 == 0);
        let small = report_page(&pending, &mut cursor);
        assert_eq!(small.into_iter().collect::<BTreeMap<_, _>>(), pending);
    }
}
impl Lane {
    fn new(connect: Connector, directory: Store) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Task>(8);
        let (out, recv) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut peers = BTreeMap::<Worker, Client>::new();
            while let Ok((destination, msg)) = rx.recv() {
                let target = match destination {
                    Destination::Directory => directory.worker(1),
                    Destination::Worker(worker) => Ok(worker),
                };
                let reply = target.and_then(|w| {
                    if let Some(client) = peers.get_mut(&w) {
                        match client.call(&msg) {
                            // The listener releases idle sockets. Both lane messages carry
                            // sequence/epoch receipts, so replay is safe after an ambiguous close.
                            Err(Error::Io(_)) => {
                                peers.remove(&w);
                            }
                            Ok(response) => return Ok(response),
                            Err(error) => {
                                peers.remove(&w);
                                return Err(error);
                            }
                        }
                    }
                    if peers.len() >= 64 {
                        peers.pop_first();
                    }
                    let mut client = connect(w)?;
                    let response = client.call(&msg)?;
                    peers.insert(w, client);
                    Ok(response)
                });
                // The control lane returns its original page for an exact replay on
                // failure. Data batches can release their payload as soon as TLS returns.
                let report = match msg {
                    Request::Spatial(report) => Some(report),
                    _ => None,
                };
                if out.send((destination, reply, report)).is_err() {
                    break;
                }
            }
        });
        Self {
            send: tx,
            receive: recv,
        }
    }
}
#[derive(Default)]
struct LocalRoutes {
    descriptions: BTreeMap<Id, Description>,
    subs: BTreeMap<Id, BTreeMap<Recipient, Subscription>>,
}
impl Routing for LocalRoutes {
    fn presence(&self, id: Id) -> Option<Presence> {
        self.descriptions.get(&id).map(|d| d.presence)
    }
    fn edges(&self, id: Id) -> Option<&BTreeMap<Recipient, Subscription>> {
        self.subs.get(&id)
    }
}
pub struct PeerMesh {
    db: Store,
    here: Worker,
    control: Lane,
    updates: Lane,
    own: LocalRoutes,
    views: Views,
    publisher: Publisher,
    tick: u64,
    pending: BTreeMap<Id, u64>,
    report_cursor: Option<Id>,
    change: u64,
    sequence: u64,
    ack: u64,
    broker: Option<Worker>,
    inflight: Option<Vec<(Id, u64)>>,
    retry: Option<(Report, Vec<(Id, u64)>)>,
    pub frames_out: u64,
    pub frames_in: u64,
    pub last_error: Option<String>,
}
impl PeerMesh {
    pub fn new(
        db: Store,
        w: Worker,
        connect: impl Fn(Worker) -> Result<Client> + Send + Sync + 'static,
    ) -> Self {
        let connect: Connector = Arc::new(connect);
        let directory = db.independent_link();
        Self {
            db,
            here: w,
            control: Lane::new(connect.clone(), directory.clone()),
            updates: Lane::new(connect, directory),
            own: Default::default(),
            views: Default::default(),
            publisher: Publisher::new(LINKS),
            tick: 0,
            pending: Default::default(),
            report_cursor: None,
            change: 0,
            sequence: 1,
            ack: 0,
            broker: None,
            inflight: None,
            retry: None,
            frames_out: 0,
            frames_in: 0,
            last_error: None,
        }
    }
    fn reset(&mut self, w: &mut World, broker: Worker) {
        for id in self.views.objects() {
            crate::pool::unload(w, id);
        }
        self.broker = Some(broker);
        self.ack = 0;
        self.sequence = 1;
        self.pending.clear();
        self.views = Views::default();
        self.publisher = Publisher::new(LINKS);
        self.own.subs.clear();
        self.report_cursor = None;
        for &id in self.own.descriptions.keys() {
            self.change += 1;
            self.pending.insert(id, self.change);
        }
    }
    fn route(&mut self, w: &mut World, r: Route) -> Result<()> {
        let c = r.change;
        if r.source == self.here {
            if let Some(s) = &c.subscription {
                self.own
                    .subs
                    .entry(c.object)
                    .or_default()
                    .insert(c.to, s.clone());
            } else if let Some(e) = self.own.subs.get_mut(&c.object) {
                e.remove(&c.to);
                if e.is_empty() {
                    self.own.subs.remove(&c.object);
                }
            }
            self.publisher.route(&c)?;
        } else if c.to == Recipient::Worker(self.here) {
            self.views.route(&c);
            if c.subscription.is_none() {
                crate::pool::unload(w, c.object);
            }
        }
        Ok(())
    }
    pub fn step(&mut self, w: &mut World) -> Result<()> {
        let start = w
            .contains_resource::<WorkerTimings>()
            .then(std::time::Instant::now);
        self.tick += 1;
        let dirty = std::mem::take(&mut w.resource_mut::<MeshState>().dirty);
        let mut dirty = dirty.into_iter();
        while let Some((id, change)) = dirty.next() {
            if !change.spatial {
                self.publisher
                    .changed_fields(&self.own, id, self.tick, change.fields.slice());
                continue;
            }
            let desc = describe::<true>(w, self.here, id);
            if self.own.descriptions.get(&id) != desc.as_ref() {
                if self.pending.len() >= LINKS && !self.pending.contains_key(&id) {
                    w.resource_mut::<MeshState>().dirty.insert(id, change);
                    w.resource_mut::<MeshState>().dirty.extend(dirty);
                    return Err(Error::Busy("spatial report backlog"));
                }
                if let Some(d) = desc {
                    self.own.descriptions.insert(id, d);
                } else {
                    self.own.descriptions.remove(&id);
                }
                self.change += 1;
                self.pending.insert(id, self.change);
            }
            self.publisher
                .changed_fields(&self.own, id, self.tick, change.fields.slice());
        }
        while let Ok((_, response, Some(report))) = self.control.receive.try_recv() {
            let Some(sent) = self.inflight.take() else {
                continue;
            };
            match response {
                Ok(Response::Routes(routes)) if Some(routes.directory) != self.broker => {
                    self.reset(w, routes.directory)
                }
                Ok(Response::Routes(routes)) if routes.sequence == self.sequence => {
                    if routes.reset {
                        for id in self.views.objects() {
                            crate::pool::unload(w, id);
                        }
                        self.views = Views::default();
                        self.own.subs.clear();
                        self.publisher = Publisher::new(LINKS);
                    }
                    for r in routes.edges {
                        self.route(w, r)?;
                    }
                    self.ack = routes.through;
                    self.views.watermark(self.ack);
                    self.sequence += 1;
                    for (id, version) in sent {
                        if self.pending.get(&id) == Some(&version) {
                            self.pending.remove(&id);
                        }
                    }
                }
                other => {
                    self.retry = Some((report, sent));
                    self.last_error = Some(format!("directory: {other:?}"));
                }
            }
        }
        if self.inflight.is_none() {
            let (report, sent) = self.retry.take().unwrap_or_else(|| {
                let sent = report_page(&self.pending, &mut self.report_cursor);
                let changes = sent
                    .iter()
                    .map(|&(id, _)| (id, self.own.descriptions.get(&id).cloned()))
                    .collect();
                (
                    Report {
                        directory: self.broker,
                        sequence: self.sequence,
                        ack: self.ack,
                        changes,
                    },
                    sent,
                )
            });
            match self
                .control
                .send
                .try_send((Destination::Directory, Request::Spatial(report)))
            {
                Ok(()) => self.inflight = Some(sent),
                Err(e) => {
                    let (_, request) = match e {
                        mpsc::TrySendError::Full(task) | mpsc::TrySendError::Disconnected(task) => {
                            task
                        }
                    };
                    let Request::Spatial(report) = request else {
                        unreachable!()
                    };
                    self.retry = Some((report, sent));
                }
            }
        }
        while let Ok((Destination::Worker(to), response, _)) = self.updates.receive.try_recv() {
            match response {
                Ok(Response::Applied(acks)) => {
                    for (id, epoch, serial) in acks {
                        self.publisher.ack(Recipient::Worker(to), id, epoch, serial);
                    }
                }
                other => self.last_error = Some(format!("peer {to:?}: {other:?}")),
            }
        }
        let publish = start.map(|s| {
            w.resource_mut::<WorkerTimings>().spatial_routing_us += s.elapsed().as_micros() as u64;
            std::time::Instant::now()
        });
        for (to, batch) in self.publisher.flush(w, &self.own, self.tick, |id| {
            self.own
                .descriptions
                .get(&id)
                .and_then(|d| w.resource::<MeshState>().authority(d.group))
                .map_or(0, |a| a.generation)
        })? {
            if let Recipient::Worker(to) = to {
                let mut send = |part: Vec<Update>| {
                    let count = part.len();
                    if self
                        .updates
                        .send
                        .try_send((Destination::Worker(to), Request::Push(part)))
                        .is_ok()
                    {
                        self.frames_out += count as u64;
                    }
                };
                if batch.len() <= MAX_TARGETS {
                    send(batch);
                } else {
                    let mut rest = batch.into_iter();
                    while rest.len() != 0 {
                        send(rest.by_ref().take(MAX_TARGETS).collect());
                    }
                }
            }
        }
        if let Some(start) = publish {
            w.resource_mut::<WorkerTimings>().peer_publish_us += start.elapsed().as_micros() as u64;
        }
        Ok(())
    }
    pub fn receive(&mut self, w: &mut World, role: Role, updates: Vec<Update>) -> Result<Response> {
        let Role::Worker(from) = role else {
            return Err(Error::Denied);
        };
        self.db.check_worker(from)?;
        if updates.len() > MAX_TARGETS {
            return Err(Error::Busy("peer update batch"));
        }
        let ids: Vec<_> = updates
            .iter()
            .filter(|u| self.views.interested(u.object, u.epoch))
            .map(|u| u.object)
            .collect();
        let owners = self.db.authorities(&ids)?;
        self.receive_prepared(w, from, updates, &owners)
    }
    pub(crate) fn receive_prepared(
        &mut self,
        w: &mut World,
        from: Worker,
        updates: Vec<Update>,
        owners: &BTreeMap<Id, (Id, Authority)>,
    ) -> Result<Response> {
        let mut ack = vec![];
        for u in updates {
            if !self.views.interested(u.object, u.epoch) {
                continue;
            }
            let Some(&(group, owner)) = owners.get(&u.object) else {
                continue;
            };
            if owner.worker != from || owner.generation != u.generation {
                continue;
            }
            let receipt = (u.object, u.epoch, u.serial);
            match crate::pool::apply_view(w, &mut self.views, group, u)? {
                ApplyOutcome::Installed => {
                    self.frames_in += 1;
                    ack.push(receipt);
                }
                ApplyOutcome::Duplicate => ack.push(receipt),
                ApplyOutcome::Rejected => (),
            }
        }
        Ok(Response::Applied(ack))
    }
    pub fn counts(&self) -> (usize, u64, u64) {
        (self.views.len(), self.frames_out, self.frames_in)
    }
    pub(crate) fn affinity(&self, group: Id) -> BTreeMap<Worker, u64> {
        let mut totals = BTreeMap::new();
        for (id, routes) in &self.own.subs {
            if self
                .own
                .descriptions
                .get(id)
                .is_some_and(|d| d.group == group)
            {
                for (to, sub) in routes {
                    if let Recipient::Worker(w) = to {
                        *totals.entry(*w).or_default() += sub
                            .components
                            .values()
                            .map(|d| 100 / u64::from(d.period))
                            .sum::<u64>();
                    }
                }
            }
        }
        totals
    }
}
