//! Budgeted application-level frames. The transport owns delivery and packet reliability.
use crate::*;
use bevy_ecs::prelude::{IntoSystemConfigs, Resource, World};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

/// Install in every Pool world which contributes state to external subscribers.
/// A world without external subscribers pays no per-object bookkeeping here.
#[derive(Resource, Default)]
pub struct SubscriberSource {
    pub(crate) spatial: u64,
    versions: HashMap<Id, FieldVersions>,
    pending: Option<BTreeSet<Id>>,
}
#[derive(Default)]
struct FieldVersions {
    public: u64,
    personal: u64,
    fields: Vec<(u32, u64)>,
}
impl SubscriberSource {
    pub(crate) fn invalidate_spatial(&mut self, id: Id) {
        self.spatial = self.spatial.wrapping_add(1);
        if let Some(p) = &mut self.pending {
            p.insert(id);
        }
    }
    pub(crate) fn changed(
        &mut self,
        id: Id,
        schema: u32,
        visibility: Visibility,
        spatial: bool,
        tick: u64,
    ) {
        if spatial {
            self.spatial = self.spatial.wrapping_add(1);
            if let Some(p) = &mut self.pending {
                p.insert(id);
            }
        }
        if matches!(visibility, Visibility::Public | Visibility::Private) {
            let versions = self.versions.entry(id).or_default();
            versions.personal = tick;
            if let Some((_, last)) = versions.fields.iter_mut().find(|(id, _)| *id == schema) {
                *last = tick;
            } else {
                versions.fields.push((schema, tick));
            }
            if visibility == Visibility::Public {
                versions.public = tick;
            }
        }
    }
    pub(crate) fn remove(&mut self, id: Id) {
        self.spatial = self.spatial.wrapping_add(1);
        self.versions.remove(&id);
        if let Some(p) = &mut self.pending {
            p.insert(id);
        }
    }
}

/// Uses the same octree and sight rules for external subscribers in a standalone World.
pub struct LocalSubscriberPlugin;
#[derive(Resource, Default)]
struct LocalIndex(Directory, bool);
impl bevy_app::Plugin for LocalSubscriberPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.init_resource::<SubscriberSource>()
            .init_resource::<LocalIndex>();
        app.world_mut().resource_mut::<SubscriberSource>().pending = Some(BTreeSet::new());
        app.add_systems(
            bevy_app::PostUpdate,
            index_local
                .after(MeshSystems::Track)
                .before(MeshSystems::Output),
        );
    }
}
fn index_local(world: &mut World) {
    let ids = std::mem::take(
        world
            .resource_mut::<SubscriberSource>()
            .pending
            .as_mut()
            .unwrap(),
    );
    if ids.is_empty() {
        return;
    }
    world.resource_scope(|world, mut index: bevy_ecs::prelude::Mut<LocalIndex>| {
        let result = index.0.batch(|d| {
            for &id in &ids {
                let state = world.resource::<MeshState>();
                let owner = state
                    .entities
                    .get(&id)
                    .and_then(|e| state.authority(world.get::<Managed>(*e)?.group));
                if let Some(v) =
                    owner.and_then(|a| crate::network_mesh::describe::<true>(world, a.worker, id))
                {
                    d.describe(v)?;
                } else if let Some(p) = d.presence(id) {
                    d.remove_owned(p.owner, id);
                }
            }
            Ok(())
        });
        index.1 = result.is_err();
        if let Err(e) = result {
            world.resource_mut::<MeshState>().diagnose(e.to_string());
            world
                .resource_mut::<SubscriberSource>()
                .pending
                .as_mut()
                .unwrap()
                .extend(ids);
        }
    });
}
enum Source<'a> {
    Pool(&'a Pool),
    Local(&'a World),
    Worker(&'a World, &'a SubscriberPlan),
}
impl<'a> Source<'a> {
    fn world(&self, worker: Worker) -> Result<&'a World> {
        match self {
            Self::Pool(p) => Ok(p.app(worker)?.world()),
            Self::Local(w) | Self::Worker(w, _) => Ok(w),
        }
    }
    fn versions(&self) -> Result<Vec<(Worker, u64)>> {
        match self {
            Self::Pool(p) => p.subscriber_spatial_versions(),
            Self::Worker(w, plan) => Ok(vec![
                plan.version,
                (plan.source, w.resource::<SubscriberSource>().spatial),
            ]),
            Self::Local(w) => Ok(vec![(
                Worker {
                    slot: 0,
                    incarnation: 0,
                },
                w.get_resource::<SubscriberSource>()
                    .ok_or_else(|| invalid("missing LocalSubscriberPlugin"))?
                    .spatial,
            )]),
        }
    }
    fn tick(&self) -> u64 {
        match self {
            Self::Pool(p) => p.tick,
            Self::Local(w) | Self::Worker(w, _) => w.resource::<MeshState>().tick(),
        }
    }
    fn directory(&self) -> Result<&Directory> {
        match self {
            Self::Pool(p) => Ok(&p.directory),
            Self::Local(w) => {
                let index = w
                    .get_resource::<LocalIndex>()
                    .ok_or_else(|| invalid("missing LocalSubscriberPlugin"))?;
                if index.1 {
                    return Err(Error::Busy("rebuilding subscriber index"));
                }
                Ok(&index.0)
            }
            Self::Worker(..) => Err(invalid("source uses a directory plan")),
        }
    }
    fn rules(&self, actor: Id) -> Result<&SightRanges> {
        match self {
            Self::Pool(p) => p.sight_rules(actor),
            Self::Local(w) | Self::Worker(w, _) => w
                .get_resource::<SightRanges>()
                .ok_or_else(|| invalid("missing game sight ranges")),
        }
    }
    fn point(&self, p: &Presence) -> Option<SightPoint> {
        match self {
            Self::Pool(pool) => pool.local_sight_point(p),
            Self::Local(w) => w
                .resource::<LocalIndex>()
                .0
                .sight_points
                .get(&p.id)
                .copied(),
            Self::Worker(..) => None,
        }
    }
}
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriberPlan {
    pub(crate) version: (Worker, u64),
    pub(crate) source: Worker,
    pub(crate) origin: SightOrigin,
    pub(crate) members: Vec<Id>,
}
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct StreamPoll {
    pub binding: ControlBinding,
    pub subscriber: SubscriberId,
    pub stream: Option<Id>,
    /// Last directory version returned by this same source.
    pub known: Option<(Worker, u64)>,
    /// Last application frame installed for this source worker.
    pub applied: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamReply {
    pub stream: Id,
    pub source: Worker,
    pub version: (Worker, u64),
    pub frame: Option<SubscriberFrame>,
}
/// One cursor per source worker. A transport retains the cursor across reconnect
/// and explicitly acknowledges only a frame its game world has installed.
#[derive(Default)]
pub struct WorkerSubscription {
    stream: Option<Id>,
    known: Option<(Worker, u64)>,
    applied: Option<u64>,
    offered: Option<u64>,
}
impl WorkerSubscription {
    pub fn poll(
        &mut self,
        link: &mut wire::Client,
        binding: ControlBinding,
        subscriber: SubscriberId,
    ) -> Result<StreamReply> {
        for _ in 0..2 {
            match link.call(&wire::Request::SubscriberPoll(StreamPoll {
                binding,
                subscriber,
                stream: self.stream,
                known: self.known,
                applied: self.applied,
            }))? {
                wire::Response::Subscriber(reply) => {
                    if self.stream != Some(reply.stream) {
                        self.applied = None;
                        self.offered = None;
                    }
                    self.stream = Some(reply.stream);
                    self.known = Some(reply.version);
                    if let Some(f) = &reply.frame {
                        self.offered = Some(f.sequence);
                    }
                    return Ok(reply);
                }
                wire::Response::RefreshPlan => {
                    self.known = None;
                }
                wire::Response::Error(e) => return Err(invalid(e)),
                _ => return Err(invalid("expected subscriber frame")),
            }
        }
        Err(Error::Busy("subscriber source changed during discovery"))
    }
    pub fn applied(&mut self, stream: Id, sequence: u64) -> Result<()> {
        if self.stream != Some(stream) || self.offered != Some(sequence) {
            return Err(Error::Fenced);
        }
        self.applied = Some(sequence);
        Ok(())
    }
}

#[derive(Clone, Copy, Resource, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubscriberPolicy {
    pub bytes_per_poll: usize,
    pub events_per_poll: usize,
    pub max_visible: usize,
    pub near_mm: u64,
    pub near_period: u64,
    pub far_period: u64,
    pub ack_timeout_ticks: u64,
}
impl SubscriberPolicy {
    pub fn validate(&self) -> Result<()> {
        if !(32..=MAX_FRAME).contains(&self.bytes_per_poll)
            || self.events_per_poll == 0
            || self.max_visible == 0
            || self.ack_timeout_ticks == 0
        {
            return Err(invalid("invalid subscriber budget"));
        }
        Ok(())
    }
}
impl Default for SubscriberPolicy {
    fn default() -> Self {
        Self {
            bytes_per_poll: 128 * 1024,
            events_per_poll: 128,
            max_visible: 16384,
            near_mm: 100_000,
            near_period: 1,
            far_period: 4,
            ack_timeout_ticks: 40,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubscriberEvent {
    /// Discard all installed state before applying subsequent events.
    Reset,
    Enter {
        object: Id,
        authority: Authority,
        fields: Vec<(u32, Arc<Field>)>,
    },
    Leave(Id),
    Delta {
        object: Id,
        authority: Authority,
        fields: Vec<(u32, Option<Arc<Field>>)>,
    },
}
impl SubscriberEvent {
    /// Upper bound for field payload and mesh metadata, excluding transport framing.
    pub fn budget_bytes(&self) -> usize {
        match self {
            Self::Reset | Self::Leave(_) => 32,
            Self::Enter { fields, .. } => {
                64 + fields
                    .iter()
                    .map(|(_, f)| 16 + f.bytes.len())
                    .sum::<usize>()
            }
            Self::Delta { fields, .. } => {
                64 + fields
                    .iter()
                    .map(|(_, f)| 16 + f.as_ref().map_or(0, |f| f.bytes.len()))
                    .sum::<usize>()
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriberFrame {
    pub sequence: u64,
    pub events: Vec<SubscriberEvent>,
}
#[derive(Clone)]
struct Known {
    stamp: (bevy_ecs::world::WorldId, Worker, u64, u64),
    fields: Arc<BTreeMap<u32, Digest>>,
}
struct Projection {
    known: Known,
    fields: BTreeMap<u32, Arc<Field>>,
}
/// Share one instance between subscribers on a transport task. Stored payloads
/// expire at the next simulation tick; private projections are keyed by actor.
pub struct SubscriberEncoder {
    tick: Option<u64>,
    projections: HashMap<(Id, bool, Option<u64>), Projection>,
    cached_bytes: usize,
    capacity_bytes: usize,
}
impl SubscriberEncoder {
    pub fn with_capacity(bytes: std::num::NonZeroUsize) -> Self {
        Self {
            tick: None,
            projections: Default::default(),
            cached_bytes: 0,
            capacity_bytes: bytes.get(),
        }
    }
}
impl Default for SubscriberEncoder {
    fn default() -> Self {
        Self::with_capacity((64 * 1024 * 1024).try_into().unwrap())
    }
}
struct PendingFrame {
    bytes: usize,
    seq: u64,
    tick: u64,
    changes: Vec<(Id, Option<Known>)>,
}
pub struct SubscriberStream {
    binding: ControlBinding,
    subscriber: SubscriberId,
    policy: SubscriberPolicy,
    known: BTreeMap<Id, Known>,
    visible: BTreeMap<Id, (Worker, bool)>,
    spatial: Vec<(Worker, u64)>,
    pending: Option<PendingFrame>,
    applied: u64,
    sequence: u64,
    reset: bool,
    cursor: Option<Id>,
    allowance: Option<(u64, usize, usize)>,
}
impl SubscriberStream {
    /// Budgeted application payload awaiting confirmation in this source stream.
    pub fn unacked_bytes(&self) -> usize {
        self.pending.as_ref().map_or(0, |p| p.bytes)
    }
    pub fn open(
        directory: &SessionDirectory,
        binding: ControlBinding,
        subscriber: SubscriberId,
        policy: SubscriberPolicy,
    ) -> Result<Self> {
        if !directory.authorized_subscriber(binding, subscriber)? {
            return Err(Error::Denied);
        }
        policy.validate()?;
        Ok(Self {
            binding,
            subscriber,
            policy,
            known: Default::default(),
            visible: Default::default(),
            spatial: vec![],
            pending: None,
            applied: 0,
            sequence: 0,
            reset: true,
            cursor: None,
            allowance: None,
        })
    }
    fn clear(&mut self) {
        self.known.clear();
        self.visible.clear();
        self.spatial.clear();
        self.pending = None;
        self.reset = true;
        self.cursor = None;
    }
    /// The transport calls this only for a frame the client applied to its world.
    pub fn applied(&mut self, binding: ControlBinding, seq: u64) -> Result<()> {
        if self.binding != binding {
            return Err(Error::Fenced);
        }
        if seq == self.applied {
            return Ok(());
        }
        if self.pending.as_ref().is_none_or(|p| p.seq != seq) {
            return Err(Error::Fenced);
        }
        for (id, v) in self.pending.take().unwrap().changes {
            if let Some(v) = v {
                self.known.insert(id, v);
            } else {
                self.known.remove(&id);
            }
        }
        self.applied = seq;
        Ok(())
    }
    pub fn resume(
        &mut self,
        directory: &SessionDirectory,
        binding: ControlBinding,
        subscriber: SubscriberId,
        applied: u64,
    ) -> Result<()> {
        if !directory.authorized_subscriber(binding, subscriber)? {
            return Err(Error::Denied);
        }
        if binding.principal != self.binding.principal
            || binding.object != self.binding.object
            || binding.session != self.binding.session
            || subscriber != self.subscriber
            || applied != self.applied
            || self.pending.is_some()
        {
            self.clear();
        }
        self.spatial.clear();
        self.binding = binding;
        self.subscriber = subscriber;
        Ok(())
    }
    pub fn next(
        &mut self,
        pool: &Pool,
        encoder: &mut SubscriberEncoder,
    ) -> Result<Option<SubscriberFrame>> {
        let directory = SessionDirectory::persistent(pool.store.clone());
        self.poll(Source::Pool(pool), &directory, encoder)
    }
    pub fn next_local(
        &mut self,
        world: &World,
        encoder: &mut SubscriberEncoder,
    ) -> Result<Option<SubscriberFrame>> {
        self.poll(
            Source::Local(world),
            world
                .get_resource::<SessionDirectory>()
                .ok_or_else(|| invalid("missing MeshSessionPlugin"))?,
            encoder,
        )
    }
    pub(crate) fn next_worker(
        &mut self,
        w: &World,
        plan: &SubscriberPlan,
        encoder: &mut SubscriberEncoder,
    ) -> Result<Option<SubscriberFrame>> {
        self.poll(
            Source::Worker(w, plan),
            w.resource::<SessionDirectory>(),
            encoder,
        )
    }
    pub(crate) fn poll_binding(
        &mut self,
        directory: &SessionDirectory,
        poll: StreamPoll,
    ) -> Result<()> {
        if self.binding != poll.binding {
            self.resume(
                directory,
                poll.binding,
                poll.subscriber,
                poll.applied.unwrap_or(0),
            )?;
        } else if let Some(seq) = poll.applied {
            if self.applied != seq {
                self.applied(poll.binding, seq)?;
            }
        }
        Ok(())
    }
    fn poll(
        &mut self,
        source: Source<'_>,
        directory: &SessionDirectory,
        encoder: &mut SubscriberEncoder,
    ) -> Result<Option<SubscriberFrame>> {
        if !directory.authorized_subscriber(self.binding, self.subscriber)? {
            self.clear();
            return Err(Error::Denied);
        }
        if let Some(p) = &self.pending {
            if source.tick().saturating_sub(p.tick) < self.policy.ack_timeout_ticks {
                return Ok(None);
            }
            self.clear();
        }
        if self
            .allowance
            .is_none_or(|(tick, _, _)| tick != source.tick())
        {
            self.allowance = Some((
                source.tick(),
                self.policy.bytes_per_poll,
                self.policy.events_per_poll,
            ));
        }
        let (_, byte_limit, event_limit) = self.allowance.unwrap();
        if byte_limit < 32 || event_limit == 0 {
            return Ok(None);
        }
        let versions = source.versions()?;
        let actor = self.binding.object;
        if self.spatial != versions {
            if let Source::Worker(w, plan) = &source {
                let mut visible = BTreeMap::new();
                let rules = source.rules(actor)?;
                let near = BTreeMap::from([(0, self.policy.near_mm)]);
                for &id in &plan.members {
                    let Some(&e) = w.resource::<MeshState>().entities.get(&id) else {
                        continue;
                    };
                    if w.get::<Simulated>(e).is_none() {
                        continue;
                    }
                    let Some((frame, point)) = (w.resource::<SightRule>().0)(w, e) else {
                        continue;
                    };
                    let Some(point) = rules.current_target(w, e, plan.origin, frame, point) else {
                        continue;
                    };
                    if id == actor || plan.origin.includes(&rules.radii, frame, point) {
                        visible.insert(
                            id,
                            (
                                plan.source,
                                plan.origin
                                    .includes(&near, frame, SightPoint { kind: 0, ..point }),
                            ),
                        );
                    }
                }
                if visible.len() > self.policy.max_visible {
                    return Err(Error::Busy(
                        "subscriber visibility exceeds configured capacity",
                    ));
                }
                self.visible = visible;
            } else {
                let index = source.directory()?;
                let origin = index
                    .presence(actor)
                    .ok_or_else(|| Error::Missing("subscriber position".into()))?;
                let center = source.point(&origin).ok_or(Error::Fenced)?.mm;
                let mut visible = BTreeMap::from([(actor, (origin.owner, true))]);
                let mut overflow = false;
                index.visit_visible(
                    actor,
                    source.rules(actor)?,
                    |p| source.point(p),
                    |id, worker| {
                        if visible.len() >= self.policy.max_visible {
                            overflow = true;
                            return;
                        }
                        let near = index
                            .presence(id)
                            .and_then(|p| source.point(&p))
                            .is_some_and(|p| {
                                let mut remaining = u128::from(self.policy.near_mm).pow(2);
                                center.iter().zip(p.mm).all(|(a, b)| {
                                    if let Some(left) =
                                        remaining.checked_sub(u128::from(a.abs_diff(b)).pow(2))
                                    {
                                        remaining = left;
                                        true
                                    } else {
                                        false
                                    }
                                })
                            });
                        visible.insert(id, (worker, near));
                    },
                )?;
                if overflow {
                    self.clear();
                    return Err(Error::Busy(
                        "subscriber visibility exceeds configured capacity",
                    ));
                }
                self.visible = visible;
            }
            self.spatial = versions;
        }
        if encoder.tick != Some(source.tick()) {
            encoder.projections.clear();
            encoder.cached_bytes = 0;
            encoder.tick = Some(source.tick());
        }
        let mut events = Vec::new();
        let mut changes = Vec::new();
        let mut fences = BTreeMap::<Worker, BTreeMap<Id, Authority>>::new();
        let mut bytes = 0;
        if self.reset {
            events.push(SubscriberEvent::Reset);
            bytes = 32;
        }
        for &id in self
            .known
            .keys()
            .filter(|id| !self.visible.contains_key(id))
        {
            if events.len() >= event_limit || bytes + 32 > byte_limit {
                break;
            }
            bytes += 32;
            events.push(SubscriberEvent::Leave(id));
            changes.push((id, None));
        }
        let cursor = self.cursor;
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let first = self
            .visible
            .range((cursor.map_or(Unbounded, Excluded), Unbounded));
        let second = self
            .visible
            .range((Unbounded, cursor.map_or(Unbounded, Included)))
            .take(if cursor.is_some() { usize::MAX } else { 0 });
        for (&id, &(worker, near)) in first.chain(second) {
            if events.len() >= event_limit {
                break;
            }
            let private = id == actor;
            let w = source.world(worker)?;
            let state = w.resource::<MeshState>();
            let Some((&e, m)) = state
                .entities
                .get(&id)
                .and_then(|e| Some((e, w.get::<Managed>(*e)?)))
            else {
                return Err(Error::Fenced);
            };
            if w.get::<Simulated>(e).is_none() {
                return Err(Error::Fenced);
            }
            let grant = state
                .authority(m.group)
                .filter(|a| a.worker == worker)
                .ok_or(Error::Fenced)?;
            let versions = w
                .get_resource::<SubscriberSource>()
                .ok_or_else(|| invalid("worker lacks SubscriberSource"))?
                .versions
                .get(&id);
            let stamp = (
                w.id(),
                worker,
                grant.generation,
                versions.map_or(0, |v| if private { v.personal } else { v.public }),
            );
            let before = self.known.get(&id);
            if before.is_some_and(|k| k.stamp == stamp) {
                continue;
            }
            let period = if near {
                self.policy.near_period
            } else {
                self.policy.far_period
            }
            .max(1);
            if before.is_some() && source.tick() % period != 0 {
                continue;
            }
            let baseline = before.filter(|k| {
                k.stamp.0 == w.id() && k.stamp.1 == worker && k.stamp.2 == grant.generation
            });
            let key = (id, private, baseline.map(|known| known.stamp.3));
            if !encoder
                .projections
                .get(&key)
                .is_some_and(|v| v.known.stamp == stamp)
            {
                let view = if private {
                    View::Client {
                        principal: actor,
                        owner: actor,
                        detail: Detail::Exact,
                    }
                } else {
                    View::Public(Detail::Exact)
                };
                let mut hashes = baseline.map_or_else(BTreeMap::new, |b| (*b.fields).clone());
                let mut fields = BTreeMap::new();
                if let Some(baseline) = baseline {
                    for &(schema, changed) in versions.into_iter().flat_map(|v| &v.fields) {
                        if changed <= baseline.stamp.3 {
                            continue;
                        }
                        if let Some(field) = project_component(w, id, schema, view)? {
                            hashes.insert(schema, digest(&encode(&field)?));
                            fields.insert(schema, Arc::new(field));
                        } else {
                            hashes.remove(&schema);
                        }
                    }
                } else {
                    for (schema, field) in capture(w, id, view)?.fields {
                        hashes.insert(schema, digest(&encode(&field)?));
                        fields.insert(schema, Arc::new(field));
                    }
                }
                let size = 128
                    + hashes.len() * 80
                    + fields.values().map(|f| 96 + f.bytes.len()).sum::<usize>();
                if size > encoder.capacity_bytes {
                    return Err(Error::Busy("object exceeds subscriber projection cache"));
                }
                if size > encoder.capacity_bytes - encoder.cached_bytes {
                    encoder.projections.clear();
                    encoder.cached_bytes = 0;
                }
                encoder.cached_bytes += size;
                encoder.projections.insert(
                    key,
                    Projection {
                        known: Known {
                            stamp,
                            fields: Arc::new(hashes),
                        },
                        fields,
                    },
                );
            }
            let p = &encoder.projections[&key];
            let event = if let Some(old) = baseline {
                let mut fields = Vec::new();
                for (s, hash) in p.known.fields.iter() {
                    if old.fields.get(s) != Some(hash) {
                        fields.push((*s, Some(p.fields[s].clone())));
                    }
                }
                fields.extend(
                    old.fields
                        .keys()
                        .filter(|s| !p.known.fields.contains_key(s))
                        .map(|s| (*s, None)),
                );
                if fields.is_empty() {
                    self.known.insert(id, p.known.clone());
                    continue;
                }
                SubscriberEvent::Delta {
                    object: id,
                    authority: grant,
                    fields,
                }
            } else {
                SubscriberEvent::Enter {
                    object: id,
                    authority: grant,
                    fields: p.fields.iter().map(|(s, f)| (*s, f.clone())).collect(),
                }
            };
            let cost = event.budget_bytes();
            if cost > self.policy.bytes_per_poll {
                return Err(Error::Busy(
                    "object exceeds subscriber frame budget; use bulk storage",
                ));
            }
            if bytes + cost > byte_limit {
                break;
            }
            if w.contains_resource::<Store>() {
                fences.entry(worker).or_default().insert(m.group, grant);
            }
            bytes += cost;
            events.push(event);
            changes.push((id, Some(p.known.clone())));
            self.cursor = Some(id);
        }
        if events.is_empty() {
            return Ok(None);
        }
        // Fence every emitted projection, including shared cache hits. Validate
        // once per batch of distinct groups at the source, before publishing.
        for (worker, grants) in fences {
            let db = source.world(worker)?.resource::<Store>();
            let ids: Vec<_> = grants.keys().copied().collect();
            for batch in ids.chunks(MAX_TARGETS) {
                let states = db.group_states(batch, Some(worker))?;
                if batch
                    .iter()
                    .any(|id| states.get(id) != Some(&(grants[id], false)))
                {
                    self.clear();
                    return Err(Error::Fenced);
                }
            }
        }
        self.reset = false;
        self.allowance = Some((
            source.tick(),
            byte_limit - bytes,
            event_limit - events.len(),
        ));
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("subscriber sequence exhausted"))?;
        self.pending = Some(PendingFrame {
            bytes,
            seq: self.sequence,
            tick: source.tick(),
            changes,
        });
        Ok(Some(SubscriberFrame {
            sequence: self.sequence,
            events,
        }))
    }
}
