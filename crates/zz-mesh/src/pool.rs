use crate::*;
use bevy_app::App;
use bevy_ecs::prelude::{Entity, Resource, World};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant},
};

/// The game's one-time rule for spatially addressable entities. Planet catalogue
/// roots can return None. The returned box is an actual interaction region.
pub struct SpatialProfile {
    pub bounds: Box3,
    pub velocity: [i64; 3],
    pub ahead: u16,
    pub retention: u32,
    pub principal: Id,
    pub active: bool,
    pub needs: BTreeMap<u32, Demand>,
}
#[derive(Resource)]
pub struct SpatialRule(pub fn(&World, Entity) -> Option<SpatialProfile>);
#[derive(Resource, Default)]
pub struct GroupTimings(pub BTreeMap<Id, u64>);
#[derive(Clone, Copy, Debug)]
pub struct BalancePolicy {
    pub every: u64,
    pub dwell: u64,
    pub tick_budget_us: u64,
    pub memory_bytes: u64,
    pub transfer_bytes: u64,
}
impl Default for BalancePolicy {
    fn default() -> Self {
        Self {
            every: 40,
            dwell: 80,
            tick_budget_us: 40_000,
            memory_bytes: 256 << 20,
            transfer_bytes: 8 << 20,
        }
    }
}
struct Node {
    app: App,
    publisher: Publisher,
    views: Views,
    pub last_tick: Duration,
    smoothed_us: u64,
}
#[derive(Clone, Debug)]
pub struct Delivery {
    pub from: Worker,
    pub to: Recipient,
    pub group: Id,
    pub update: Update,
}
pub struct Pool {
    nodes: BTreeMap<Worker, Node>,
    pub directory: Directory,
    pub messages: VecDeque<Delivery>,
    clients: BTreeMap<Id, Views>,
    pub store: Store,
    pub tick: u64,
    pub policy: BalancePolicy,
    pub overloaded: BTreeSet<Id>,
    balancing: Option<Migration>,
    moved: BTreeMap<Id, u64>,
    limit: usize,
    bulk: BulkExecutor,
    preparing: BTreeSet<Worker>,
    sources: BTreeMap<(Recipient, Id), Worker>,
}
pub struct Migration {
    pub ticket: Handoff,
    job: Receiver<Result<(BlobCache, usize)>>,
    pub rounds: u8,
    budget: usize,
    started: Instant,
    spent: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct MigrationCost {
    pub payload: usize,
    pub prepare: Duration,
    pub pause: Duration,
    pub authority: Authority,
}
impl Pool {
    pub fn new(store: Store, limit: usize) -> Self {
        Self {
            nodes: BTreeMap::new(),
            directory: Default::default(),
            messages: VecDeque::new(),
            clients: BTreeMap::new(),
            store,
            tick: 0,
            policy: Default::default(),
            overloaded: Default::default(),
            balancing: None,
            moved: Default::default(),
            limit,
            bulk: BulkExecutor::new(2),
            preparing: BTreeSet::new(),
            sources: Default::default(),
        }
    }
    pub fn add(&mut self, w: Worker, mut app: App) -> Result<()> {
        if self.nodes.contains_key(&w) || !app.world().contains_resource::<MeshState>() {
            return Err(invalid("duplicate worker or missing MeshPlugin"));
        }
        app.init_resource::<GroupTimings>();
        self.nodes.insert(
            w,
            Node {
                app,
                publisher: Publisher::new(self.limit),
                views: Default::default(),
                last_tick: Duration::ZERO,
                smoothed_us: 0,
            },
        );
        Ok(())
    }
    pub fn app(&self, w: Worker) -> Result<&App> {
        self.nodes
            .get(&w)
            .map(|n| &n.app)
            .ok_or_else(|| Error::Missing("worker".into()))
    }
    pub fn app_mut(&mut self, w: Worker) -> Result<&mut App> {
        self.nodes
            .get_mut(&w)
            .map(|n| &mut n.app)
            .ok_or_else(|| Error::Missing("worker".into()))
    }
    pub fn step(&mut self) -> Result<()> {
        self.tick += 1;
        let mut dirty = vec![];
        for (&worker, node) in &mut self.nodes {
            let start = Instant::now();
            node.app.update();
            node.last_tick = start.elapsed();
            let us = node.last_tick.as_micros() as u64;
            node.smoothed_us = (7 * node.smoothed_us + us) / 8;
            let changed =
                std::mem::take(&mut node.app.world_mut().resource_mut::<MeshState>().dirty);
            let world = node.app.world();
            for id in changed {
                if let Some(description) = crate::network_mesh::describe(world, worker, id) {
                    self.directory.upsert(description.presence)?;
                    if let Some(area) = description.area {
                        self.directory.register_area(area)?;
                    } else {
                        self.directory.remove_area(id);
                    }
                    dirty.push((worker, id));
                } else if self
                    .directory
                    .presence(id)
                    .is_some_and(|p| p.owner == worker)
                {
                    self.directory.remove_area(id);
                    self.directory.remove(id);
                }
            }
        }
        let mut touched = BTreeSet::new();
        for c in self.directory.changes() {
            touched.insert(c.to);
            // End routes can follow deletion or migration. Remove sender state on its previous node.
            if let Some(old) = self.sources.remove(&(c.to, c.object)) {
                if let Some(n) = self.nodes.get_mut(&old) {
                    n.publisher.route(&InterestChange {
                        subscription: None,
                        ..c.clone()
                    })?;
                }
            }
            if c.subscription.is_some() {
                if let Some(p) = self.directory.presence(c.object) {
                    if let Some(n) = self.nodes.get_mut(&p.owner) {
                        n.publisher.route(&c)?;
                        self.sources.insert((c.to, c.object), p.owner);
                    }
                }
            }
            match c.to {
                Recipient::Worker(w) => {
                    if let Some(n) = self.nodes.get_mut(&w) {
                        n.views.route(&c);
                        if c.subscription.is_none() {
                            unload(n.app.world_mut(), c.object);
                        }
                    }
                }
                Recipient::Client(id) => self.clients.entry(id).or_default().route(&c),
            }
        }
        for to in touched {
            let mark = self.directory.watermark();
            match to {
                Recipient::Worker(w) => {
                    if let Some(n) = self.nodes.get_mut(&w) {
                        n.views.watermark(mark);
                    }
                }
                Recipient::Client(id) => self.clients.entry(id).or_default().watermark(mark),
            }
        }
        for (w, id) in dirty {
            self.nodes
                .get_mut(&w)
                .unwrap()
                .publisher
                .changed(&self.directory, id, self.tick);
        }
        for (&worker, n) in &mut self.nodes {
            let w = n.app.world();
            let batches = n.publisher.flush(w, &self.directory, self.tick, |id| {
                w.resource::<MeshState>()
                    .entities
                    .get(&id)
                    .and_then(|e| w.get::<Managed>(*e))
                    .and_then(|m| w.resource::<MeshState>().authority(m.group))
                    .map_or(0, |a| a.generation)
            })?;
            for (to, frames) in batches {
                for update in frames {
                    let Some(e) = w.resource::<MeshState>().entities.get(&update.object) else {
                        continue;
                    };
                    let group = w.get::<Managed>(*e).unwrap().group;
                    if self.messages.len() >= self.limit {
                        return Err(Error::Busy(
                            "delivery queue; unacknowledged state will retry",
                        ));
                    }
                    self.messages.push_back(Delivery {
                        from: worker,
                        to,
                        group,
                        update,
                    });
                }
            }
        }
        if let Some(mut m) = self.balancing.take() {
            match self.poll(&mut m) {
                Ok(Some(_)) => {
                    self.moved.insert(m.ticket.group, self.tick);
                    self.overloaded.remove(&m.ticket.group);
                }
                Ok(None) => self.balancing = Some(m),
                Err(e) => {
                    self.cancel(&m)?;
                    self.overloaded.insert(m.ticket.group);
                    self.app_mut(m.ticket.source.worker)?
                        .world_mut()
                        .resource_mut::<MeshState>()
                        .diagnose(e.to_string());
                }
            }
        } else if self.policy.every > 0 && self.tick % self.policy.every == 0 {
            self.rebalance()?;
        }
        Ok(())
    }
    pub fn rebalance(&mut self) -> Result<()> {
        let mut caps = Vec::new();
        for (&worker, n) in &self.nodes {
            let w = n.app.world();
            let hints = w.resource::<GroupTimings>().0.values().sum::<u64>();
            caps.push(Capacity {
                worker,
                tick_us: n.smoothed_us.max(hints),
                budget_us: self.policy.tick_budget_us,
                used: w.resource::<BlobCache>().bytes() as u64,
                memory: self.policy.memory_bytes,
                draining: false,
            });
        }
        self.overloaded.clear();
        for current in caps
            .iter()
            .filter(|c| c.tick_us > c.budget_us || c.used > c.memory)
        {
            let source = self.nodes[&current.worker].app.world();
            let groups: Vec<_> = source.resource::<MeshState>().owned_groups().collect();
            for &group in &groups {
                let s = self.store.snapshot(group)?;
                let meta = s
                    .records
                    .iter()
                    .map(|r| r.fields.values().map(|v| v.bytes.len() as u64).sum::<u64>())
                    .sum::<u64>();
                let bytes = meta + s.required.iter().map(|m| m.length).sum::<u64>();
                let mut g = GroupLoad {
                    group,
                    owner: current.worker,
                    cost_us: source
                        .resource::<GroupTimings>()
                        .0
                        .get(&group)
                        .copied()
                        .unwrap_or(current.tick_us / groups.len().max(1) as u64),
                    bytes,
                    moved_at: self.moved.get(&group).copied().unwrap_or(0),
                    affinity: Default::default(),
                    cached: Default::default(),
                };
                for r in &s.records {
                    for (&recipient, sub) in self.directory.subscribers(r.id) {
                        if let Recipient::Worker(w) = recipient {
                            *g.affinity.entry(w).or_default() += sub
                                .components
                                .values()
                                .map(|need| 100 / u64::from(need.period))
                                .sum::<u64>();
                        }
                    }
                }
                for (&w, n) in &self.nodes {
                    g.cached.insert(
                        w,
                        s.required
                            .iter()
                            .filter(|m| n.app.world().resource::<BlobCache>().ready(m))
                            .map(|m| m.length)
                            .sum(),
                    );
                }
                match place(
                    &g,
                    &caps,
                    self.tick,
                    self.policy.dwell,
                    self.policy.transfer_bytes,
                ) {
                    Placement::Move(to)
                        if self.balancing.is_none() && !self.preparing.contains(&to) =>
                    {
                        self.balancing =
                            Some(self.begin(group, to, self.policy.transfer_bytes as usize)?);
                        return Ok(());
                    }
                    Placement::Overloaded => {
                        self.overloaded.insert(group);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
    pub fn deliver(&mut self, msg: Delivery) -> Result<bool> {
        self.deliver_with_ack(msg, true)
    }
    pub fn deliver_with_ack(&mut self, msg: Delivery, send_ack: bool) -> Result<bool> {
        let u = &msg.update;
        let owner = self.store.owner(msg.group)?;
        if owner.worker != msg.from || owner.generation != u.generation {
            return Ok(false);
        }
        let ok = match msg.to {
            Recipient::Client(id) => self.clients.entry(id).or_default().apply(u),
            Recipient::Worker(id) => {
                let n = self
                    .nodes
                    .get_mut(&id)
                    .ok_or_else(|| Error::Missing("recipient".into()))?;
                apply_view(n.app.world_mut(), &mut n.views, msg.group, u)?
            }
        };
        let duplicate = match msg.to {
            Recipient::Client(id) => self.clients.get(&id).is_some_and(|v| v.already_applied(u)),
            Recipient::Worker(w) => self
                .nodes
                .get(&w)
                .is_some_and(|n| n.views.already_applied(u)),
        };
        if send_ack && (ok || duplicate) {
            if let Some(n) = self.nodes.get_mut(&msg.from) {
                n.publisher.ack(msg.to, u.object, u.epoch, u.serial);
            }
        }
        Ok(ok)
    }
    pub fn deliver_all(&mut self) -> Result<usize> {
        let mut n = 0;
        while let Some(m) = self.messages.pop_front() {
            n += usize::from(self.deliver(m)?);
        }
        Ok(n)
    }
    pub fn routing(&self) -> (usize, usize) {
        (
            self.sources.len(),
            self.sources
                .iter()
                .map(|((to, _), from)| (*from, *to))
                .collect::<BTreeSet<_>>()
                .len(),
        )
    }
    pub fn link_stats(&self) -> (u64, usize) {
        (
            self.nodes.values().map(|n| n.publisher.encodes).sum(),
            self.nodes.values().map(|n| n.publisher.peak).sum(),
        )
    }
    pub fn client_view(&self, id: Id) -> Option<&Views> {
        self.clients.get(&id)
    }
    fn prefetch(
        &self,
        t: Handoff,
        s: &Snapshot,
        budget: usize,
    ) -> Result<Receiver<Result<(BlobCache, usize)>>> {
        let source = self.app(t.source.worker)?.world();
        let target = self.app(t.target)?.world();
        let mut cache = target.resource::<BlobCache>().clone();
        let mut jobs = vec![];
        for m in &s.required {
            jobs.push((
                m.clone(),
                source.resource::<Providers>().get(m.key.provider)?,
                target.resource::<Providers>().get(m.key.provider)?,
            ));
        }
        self.bulk.submit(move || {
            let mut bytes = 0;
            for (m, p, consumer) in jobs {
                if cache.ready(&m) {
                    cache.hits += 1;
                } else {
                    let cost = cache.fetch_bounded(&m, p.as_ref(), budget.saturating_sub(bytes))?;
                    bytes += cost;
                    if bytes > budget {
                        return Err(Error::Busy("migration byte budget"));
                    }
                }
                consumer.prepare(
                    &m,
                    &cache
                        .get(&m)
                        .ok_or_else(|| Error::Missing("prefetch evicted".into()))?,
                )?;
            }
            Ok((cache, bytes))
        })
    }
    pub fn begin(&mut self, group: Id, target: Worker, budget: usize) -> Result<Migration> {
        let started = Instant::now();
        if self.preparing.contains(&target) {
            return Err(Error::Busy("target has an active preparation"));
        }
        let source = self.store.owner(group)?;
        let t = Handoff {
            id: Id::new(),
            group,
            source,
            target,
        };
        let s = self.store.snapshot(group)?;
        let job = self.prefetch(t, &s, budget)?;
        self.store.begin_handoff(t)?;
        self.preparing.insert(target);
        Ok(Migration {
            ticket: t,
            job,
            rounds: 1,
            budget,
            started,
            spent: 0,
        })
    }
    pub fn cancel(&mut self, m: &Migration) -> Result<Phase> {
        let result = self.store.abort_handoff(m.ticket)?;
        self.preparing.remove(&m.ticket.target);
        Ok(result)
    }
    pub fn poll(&mut self, m: &mut Migration) -> Result<Option<MigrationCost>> {
        let result = self.poll_inner(m);
        if result.is_err() && self.preparing.contains(&m.ticket.target) {
            self.cancel(m)?;
        }
        result
    }
    fn poll_inner(&mut self, m: &mut Migration) -> Result<Option<MigrationCost>> {
        let (cache, cost) = match m.job.try_recv() {
            Ok(Ok(c)) => c,
            Err(TryRecvError::Empty) => return Ok(None),
            Ok(Err(Error::Missing(_))) => {
                let s = self.store.snapshot(m.ticket.group)?;
                return self.retry(m, &s, 0);
            }
            Ok(Err(e)) => {
                self.cancel(m)?;
                return Err(e);
            }
            Err(TryRecvError::Disconnected) => {
                self.cancel(m)?;
                return Err(invalid("prefetch stopped"));
            }
        };
        let t = m.ticket;
        self.app_mut(t.target)?.insert_resource(cache);
        // Capture exact migration fields from the source world, including components
        // that use infrequent checkpoints. Membership and revisions are checked at freeze.
        let mut s = self.store.snapshot(t.group)?;
        let source = self.app(t.source.worker)?.world();
        for r in s.records.iter_mut().filter(|r| !r.deleted) {
            let exact = capture(source, r.id, View::Migration)?;
            if exact.revision != r.revision {
                return self.retry(m, &s, cost);
            }
            *r = exact;
        }
        if s.required.iter().any(|d| {
            !self.nodes[&t.target]
                .app
                .world()
                .resource::<BlobCache>()
                .ready(d)
        }) {
            return self.retry(m, &s, cost);
        }
        let mut checkpoint = s.records.clone();
        for r in checkpoint.iter_mut().filter(|r| !r.deleted) {
            *r = capture(source, r.id, View::Checkpoint)?;
        }
        let prepared = match prepare_world(self.app_mut(t.target)?.world_mut(), &s) {
            Ok(p) => p,
            Err(e) => {
                self.cancel(m)?;
                return Err(e);
            }
        };
        let start = Instant::now();
        let cut = (|| {
            self.store
                .freeze_checkpoint(t, s.revision, &s.records, &checkpoint)?;
            self.store.acknowledge(t, t.target, &prepared)?;
            self.store.switch(t)
        })();
        let authority = match cut {
            Ok(a) => a,
            Err(e) => {
                prepared.discard(self.app_mut(t.target)?.world_mut());
                self.cancel(m)?;
                return Err(e);
            }
        };
        crate::bevy_runtime::demote(self.app_mut(t.source.worker)?.world_mut(), t.group);
        let db = self.store.clone();
        prepared.activate(self.app_mut(t.target)?.world_mut(), &db, authority)?;
        self.preparing.remove(&t.target);
        Ok(Some(MigrationCost {
            payload: m.spent + cost,
            prepare: start.duration_since(m.started),
            pause: start.elapsed(),
            authority,
        }))
    }
    fn retry(
        &mut self,
        m: &mut Migration,
        s: &Snapshot,
        spent: usize,
    ) -> Result<Option<MigrationCost>> {
        if m.rounds >= 4 {
            self.cancel(m)?;
            return Err(Error::Busy("source changes exceed preparation rate"));
        }
        m.budget = m.budget.saturating_sub(spent);
        m.spent += spent;
        m.rounds += 1;
        m.job = self.prefetch(m.ticket, s, m.budget)?;
        Ok(None)
    }
    pub fn transfer(&mut self, g: Id, target: Worker, budget: usize) -> Result<MigrationCost> {
        let mut m = self.begin(g, target, budget)?;
        loop {
            if let Some(c) = self.poll(&mut m)? {
                return Ok(c);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub fn drain(
        &mut self,
        worker: Worker,
        capacities: &[Capacity],
        groups: &[GroupLoad],
        mut budget: u64,
    ) -> Result<Vec<MigrationCost>> {
        let mut pool = capacities.to_vec();
        for c in &mut pool {
            if c.worker == worker {
                c.draining = true;
            }
        }
        let mut done = vec![];
        for g in groups.iter().filter(|g| g.owner == worker) {
            let Placement::Move(to) = place(g, &pool, self.tick, 0, budget) else {
                return Err(Error::Busy("indivisible group has no feasible worker"));
            };
            let cost = self.transfer(g.group, to, budget as usize)?;
            budget = budget.saturating_sub(cost.payload as u64);
            if let Some(c) = pool.iter_mut().find(|c| c.worker == to) {
                c.used += g.bytes;
                c.tick_us += g.cost_us;
            }
            done.push(cost);
        }
        Ok(done)
    }
}
pub(crate) fn apply_view(w: &mut World, views: &mut Views, group: Id, u: &Update) -> Result<bool> {
    views.apply_checked(u, |fields| {
        let Some(fields) = fields else {
            unload(w, u.object);
            return Ok(());
        };
        if w.resource::<MeshState>()
            .entities
            .get(&u.object)
            .is_some_and(|e| w.get::<Simulated>(*e).is_some())
        {
            return Err(Error::Fenced);
        }
        let r = Record {
            id: u.object,
            group,
            revision: u.revision,
            fields: fields.clone(),
            deleted: false,
        };
        let result = crate::bevy_runtime::apply_records(w, &[r], false);
        if result.is_err() {
            unload(w, u.object);
        }
        result
    })
}
pub(crate) fn unload(w: &mut World, id: Id) {
    let e = w.resource::<MeshState>().entities.get(&id).copied();
    if let Some(e) = e {
        if w.get::<Replica>(e).is_some() {
            w.resource_mut::<MeshState>().forget_view(id);
            w.despawn(e);
        }
    }
}
