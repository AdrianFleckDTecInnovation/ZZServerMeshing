//! Coarse directory reports and directed peer delivery for independent processes.
use crate::{
    wire::{Client, Request, Response, Role},
    *,
};
use bevy_ecs::world::World;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
};

const LINKS: usize = 8192;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Description {
    pub presence: Presence,
    pub area: Option<Area>,
    pub group: Id,
}
pub(crate) fn describe(w: &World, worker: Worker, id: Id) -> Option<Description> {
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
    index: Directory,
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
    fn remove(&mut self, worker: Worker, id: Id) {
        if self.index.presence(id).is_some_and(|p| p.owner == worker) {
            self.index.remove_area(id);
            self.index.remove(id);
        }
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
                    self.remove(old, id);
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
                    self.index.upsert(d.presence)?;
                    if let Some(a) = d.area {
                        self.index.register_area(a)?;
                    } else {
                        self.index.remove_area(id);
                    }
                    self.members.entry(from).or_default().insert(id);
                } else {
                    self.remove(from, id);
                    if let Some(s) = self.members.get_mut(&from) {
                        s.remove(&id);
                    }
                }
            }
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
type Task = (Worker, Request);
struct Lane {
    send: SyncSender<Task>,
    receive: Receiver<(Worker, Result<Response>)>,
}
impl Lane {
    fn new(connect: Connector) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Task>(8);
        let (out, recv) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut peers = BTreeMap::<Worker, Client>::new();
            while let Ok((w, msg)) = rx.recv() {
                let reply = (|| {
                    if let Some(client) = peers.get_mut(&w) {
                        match client.call(&msg) {
                            // The listener releases idle sockets. Both lane messages carry
                            // sequence/epoch receipts, so replay is safe after an ambiguous close.
                            Err(Error::Io(_)) => {
                                peers.remove(&w);
                            }
                            other => return other,
                        }
                    }
                    if peers.len() >= 64 {
                        peers.pop_first();
                    }
                    let mut client = connect(w)?;
                    let response = client.call(&msg)?;
                    peers.insert(w, client);
                    Ok(response)
                })();
                if reply.is_err() {
                    peers.remove(&w);
                }
                if out.send((w, reply)).is_err() {
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
    pending: BTreeMap<Id, (u64, Option<Description>)>,
    change: u64,
    sequence: u64,
    ack: u64,
    broker: Option<Worker>,
    inflight: Option<(Report, Vec<(Id, u64)>)>,
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
        Self {
            db,
            here: w,
            control: Lane::new(connect.clone()),
            updates: Lane::new(connect),
            own: Default::default(),
            views: Default::default(),
            publisher: Publisher::new(LINKS),
            tick: 0,
            pending: Default::default(),
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
        for (&id, d) in &self.own.descriptions {
            self.change += 1;
            self.pending.insert(id, (self.change, Some(d.clone())));
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
        self.tick += 1;
        let dirty = std::mem::take(&mut w.resource_mut::<MeshState>().dirty);
        let mut dirty = dirty.into_iter();
        while let Some(id) = dirty.next() {
            let desc = describe(w, self.here, id);
            if self.own.descriptions.get(&id) != desc.as_ref() {
                if self.pending.len() >= LINKS && !self.pending.contains_key(&id) {
                    w.resource_mut::<MeshState>().dirty.insert(id);
                    w.resource_mut::<MeshState>().dirty.extend(dirty);
                    return Err(Error::Busy("spatial report backlog"));
                }
                if let Some(d) = &desc {
                    self.own.descriptions.insert(id, d.clone());
                } else {
                    self.own.descriptions.remove(&id);
                }
                self.change += 1;
                self.pending.insert(id, (self.change, desc));
            }
            self.publisher.changed(&self.own, id, self.tick);
        }
        while let Ok((_, response)) = self.control.receive.try_recv() {
            let Some((report, sent)) = self.inflight.take() else {
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
                        if self.pending.get(&id).is_some_and(|(n, _)| *n == version) {
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
            let broker = self.db.worker(1)?;
            let mut sent = vec![];
            let changes = self
                .pending
                .iter()
                .take(MAX_TARGETS)
                .map(|(&id, (n, d))| {
                    sent.push((id, *n));
                    (id, d.clone())
                })
                .collect();
            let (report, sent) = self.retry.take().unwrap_or((
                Report {
                    directory: self.broker,
                    sequence: self.sequence,
                    ack: self.ack,
                    changes,
                },
                sent,
            ));
            if self
                .control
                .send
                .try_send((broker, Request::Spatial(report.clone())))
                .is_ok()
            {
                self.inflight = Some((report, sent));
            } else {
                self.retry = Some((report, sent));
            }
        }
        while let Ok((to, response)) = self.updates.receive.try_recv() {
            match response {
                Ok(Response::Applied(acks)) => {
                    for (id, epoch, serial) in acks {
                        self.publisher.ack(Recipient::Worker(to), id, epoch, serial);
                    }
                }
                other => self.last_error = Some(format!("peer {to:?}: {other:?}")),
            }
        }
        for (to, batch) in self.publisher.flush(w, &self.own, self.tick, |id| {
            self.own
                .descriptions
                .get(&id)
                .and_then(|d| w.resource::<MeshState>().authority(d.group))
                .map_or(0, |a| a.generation)
        })? {
            if let Recipient::Worker(to) = to {
                for part in batch.chunks(MAX_TARGETS) {
                    if self
                        .updates
                        .send
                        .try_send((to, Request::Push(part.to_vec())))
                        .is_ok()
                    {
                        self.frames_out += part.len() as u64;
                    }
                }
            }
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
        let mut ack = vec![];
        let ids: Vec<_> = updates
            .iter()
            .filter(|u| self.views.interested(u.object, u.epoch))
            .map(|u| u.object)
            .collect();
        let owners = self.db.authorities(&ids)?;
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
            if crate::pool::apply_view(w, &mut self.views, group, &u)? {
                self.frames_in += 1;
                ack.push((u.object, u.epoch, u.serial));
            } else if self.views.already_applied(&u) {
                ack.push((u.object, u.epoch, u.serial));
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
