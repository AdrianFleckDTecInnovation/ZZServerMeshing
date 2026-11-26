use crate::*;
mod checkpoint_io;
use bevy_app::{App, Plugin, PostUpdate, PreUpdate};
use bevy_ecs::prelude::*;
pub use checkpoint_io::AsyncCheckpoints;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    any::TypeId,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

/// Stable object ID and the group that moves with it between workers.
#[derive(Component, Clone, Copy, Debug)]
pub struct Managed {
    pub id: Id,
    pub group: Id,
}
impl Managed {
    pub fn new(id: Id, group: Id) -> Self {
        Self { id, group }
    }
}
impl Default for Managed {
    fn default() -> Self {
        let id = Id::new();
        Self { id, group: id }
    }
}
/// The authoritative copy. Filter game simulation systems with `With<Simulated>`.
#[derive(Component)]
pub struct Simulated;
/// A subscribed view of an object simulated by another worker.
#[derive(Component)]
pub struct Replica;
/// Imported data that has not yet received the simulation grant.
#[derive(Component)]
pub struct Staged;
#[derive(Component)]
pub(crate) struct Paused;
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Detail {
    Exact,
    Coarse,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Local,
    Public,
    Private,
    Workers,
}
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub visibility: Visibility,
    pub migrate: bool,
    pub persist: bool,
}
#[derive(Clone, Copy, Debug)]
pub enum View {
    /// Public gameplay fields for another player's visible character or ship.
    Public(Detail),
    Migration,
    Checkpoint,
    Client {
        principal: Id,
        owner: Id,
        detail: Detail,
    },
    Worker {
        detail: Detail,
    },
}
impl Rules {
    pub fn detail(self, v: View) -> Option<Detail> {
        match v {
            View::Public(detail) if self.visibility == Visibility::Public => Some(detail),
            View::Migration if self.migrate => Some(Detail::Exact),
            View::Checkpoint if self.persist => Some(Detail::Exact),
            View::Client { detail, .. } if self.visibility == Visibility::Public => Some(detail),
            View::Client {
                principal, owner, ..
            } if self.visibility == Visibility::Private && principal == owner => {
                Some(Detail::Exact)
            }
            View::Worker { detail }
                if matches!(self.visibility, Visibility::Workers | Visibility::Public) =>
            {
                Some(detail)
            }
            _ => None,
        }
    }
}
pub struct ComponentCodec<T> {
    pub encode: fn(&T, Detail) -> Result<Vec<u8>>,
    pub decode: fn(&[u8]) -> Result<T>,
    pub references: fn(&T) -> Vec<Reference>,
    pub resolve: fn(&mut T, &BTreeMap<Id, Entity>) -> Result<()>,
}
impl<T: Serialize + DeserializeOwned> Default for ComponentCodec<T> {
    fn default() -> Self {
        Self {
            encode: |v, _| encode(v),
            decode: |b| decode(b),
            references: |_| vec![],
            resolve: |_, _| Ok(()),
        }
    }
}
type Capture = Box<dyn Fn(&World, Entity, Detail) -> Result<Option<Vec<u8>>> + Send + Sync>;
type Import =
    Box<dyn Fn(&mut World, Entity, &[u8], &BTreeMap<Id, Entity>) -> Result<()> + Send + Sync>;
struct Entry {
    version: u16,
    rules: Rules,
    capture: Capture,
    import: Import,
    remove: fn(&mut World, Entity),
}
type Rebuild = fn(&mut World, Entity) -> Result<()>;
#[derive(Resource, Default)]
pub struct Schemas {
    entries: BTreeMap<u32, Entry>,
    types: HashMap<TypeId, u32>,
    rebuild: Vec<Rebuild>,
    data: BTreeMap<u32, Box<dyn Fn(&World, Entity) -> Result<Vec<Manifest>> + Send + Sync>>,
}
#[derive(Resource, Default)]
pub struct MeshState {
    pub entities: BTreeMap<Id, Entity>,
    mapped: HashMap<Entity, Managed>,
    pending: Checkpoints,
    removed: BTreeMap<Id, Id>,
    pub diagnostics: std::collections::VecDeque<String>,
    versions: BTreeMap<Id, u64>,
    groups: BTreeMap<Id, (Authority, u64)>,
    mark: u64,
    tick: u64,
    pub(crate) dirty: BTreeMap<Id, ObjectChange>,
    retry_groups: BTreeSet<Id>,
    pub(crate) external_staging: BTreeSet<Id>,
    pins: BTreeMap<Id, Vec<BlobPin>>,
    saved: BTreeMap<Id, BTreeMap<u32, Field>>,
    input_sessions: HashMap<Id, Id>,
    transient_names: BTreeSet<Id>,
}
const CHECKPOINT_PERIOD: usize = 20;
/// All registered fields read by the game's spatial and sight rules.
/// Without this opt-in, custom rules retain their conservative invalidation.
#[derive(Resource)]
pub struct SpatialDependencies(pub BTreeSet<u32>);

/// Call when an external frame transform changes or expires. Re-evaluate only its members.
/// Both spatial callbacks must return coordinates in the same currently valid comparison frame.
pub fn refresh_spatial(world: &mut World, ids: &[Id]) {
    if let Some(mut source) = world.get_resource_mut::<SubscriberSource>() {
        for &id in ids {
            source.invalidate_spatial(id);
        }
    }
    if world.contains_resource::<Node>() {
        let mut state = world.resource_mut::<MeshState>();
        for &id in ids {
            state
                .dirty
                .entry(id)
                .and_modify(|c| c.spatial = true)
                .or_insert(ObjectChange {
                    spatial: true,
                    fields: ChangedFields::SpatialOnly,
                });
        }
    }
}

pub(crate) struct ObjectChange {
    pub(crate) spatial: bool,
    // None denotes a lifecycle change that invalidates every field.
    pub(crate) fields: ChangedFields,
}
// Most moving entities change exactly position and input. Keep that pair inline.
#[derive(Default)]
pub(crate) enum ChangedFields {
    #[default]
    All,
    SpatialOnly,
    Inline([u32; 2]),
    More(Box<Vec<u32>>),
}
impl ChangedFields {
    fn new(id: u32) -> Self {
        Self::Inline([id, 0])
    }
    fn add(&mut self, id: u32) {
        match self {
            Self::All => (),
            Self::SpatialOnly => *self = Self::new(id),
            Self::Inline(v) if v.contains(&id) => (),
            Self::Inline(v) if v[1] == 0 => v[1] = id,
            Self::Inline(v) => *self = Self::More(Box::new(vec![v[0], v[1], id])),
            Self::More(v) => {
                if !v.contains(&id) {
                    v.push(id)
                }
            }
        }
    }
    pub(crate) fn slice(&self) -> Option<&[u32]> {
        match self {
            Self::All => None,
            Self::SpatialOnly => Some(&[]),
            Self::Inline(v) => Some(if v[1] == 0 { &v[..1] } else { v }),
            Self::More(v) => Some(v),
        }
    }
}
impl ObjectChange {
    fn lifecycle() -> Self {
        Self {
            spatial: true,
            fields: ChangedFields::All,
        }
    }
}
#[derive(Default)]
struct Checkpoints {
    due: [BTreeMap<Id, ChangedFields>; CHECKPOINT_PERIOD],
    urgent: BTreeSet<Id>,
}
impl Checkpoints {
    fn slot(group: Id) -> usize {
        (u128::from_be_bytes(group.0) % CHECKPOINT_PERIOD as u128) as usize
    }
    fn changed(&mut self, m: &Managed, schema: u32) {
        self.due[Self::slot(m.group)]
            .entry(m.id)
            .and_modify(|f| f.add(schema))
            .or_insert_with(|| ChangedFields::new(schema));
    }
    fn remove(&mut self, id: &Id, group: Id) {
        self.due[Self::slot(group)].remove(id);
        self.urgent.remove(id);
    }
    fn take_due(&mut self, tick: u64) -> BTreeMap<Id, ChangedFields> {
        let mut ids = std::mem::take(&mut self.due[(tick % CHECKPOINT_PERIOD as u64) as usize]);
        ids.extend(
            std::mem::take(&mut self.urgent)
                .into_iter()
                .map(|id| (id, ChangedFields::All)),
        );
        ids
    }
}
impl MeshState {
    pub fn tick(&self) -> u64 {
        self.tick
    }
    pub(crate) fn forget_view(&mut self, id: Id) {
        self.entities.remove(&id);
        self.input_sessions.remove(&id);
        self.saved.remove(&id);
        self.versions.remove(&id);
    }
    pub fn owned_groups(&self) -> impl Iterator<Item = Id> + '_ {
        self.groups.keys().copied()
    }
    pub fn version(&self, id: Id) -> u64 {
        self.versions.get(&id).copied().unwrap_or(0)
    }
    pub fn authority(&self, g: Id) -> Option<Authority> {
        self.group_state(g).map(|g| g.0)
    }
    pub(crate) fn group_state(&self, group: Id) -> Option<(Authority, u64)> {
        self.groups.get(&group).copied()
    }
    pub fn diagnose(&mut self, s: String) {
        self.report(s)
    }
    fn report(&mut self, s: String) {
        if self.diagnostics.len() == 64 {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(s);
    }
}
#[derive(Resource)]
pub struct InputHandler(pub fn(&mut World, Id, &Input, u64) -> Result<InputAck>);
pub fn apply_input(w: &mut World, actor: Id, input: &Input, now: u64) -> Result<InputAck> {
    let node = storage_node(w)?.clone();
    let (group, owner) = input_binding(w, &node, actor, input, now)?;
    node.0.check_authority(group, owner, true)?;
    let handler = w
        .get_resource::<InputHandler>()
        .ok_or_else(|| invalid("game input handler missing"))?
        .0;
    handler(w, actor, input, now)
}
fn input_binding(
    w: &mut World,
    n: &Node,
    actor: Id,
    input: &Input,
    now: u64,
) -> Result<(Id, Authority)> {
    if input.sequence == 0
        || input.at_ms > now.saturating_add(100)
        || now.saturating_sub(input.at_ms) > 500
    {
        return Err(invalid("input outside server time window"));
    }
    let e = *w
        .resource::<MeshState>()
        .entities
        .get(&actor)
        .ok_or_else(|| Error::Missing("input actor".into()))?;
    let managed = w.get::<Managed>(e).ok_or(Error::Denied)?;
    let group = managed.group;
    let owner = w
        .resource::<MeshState>()
        .authority(group)
        .ok_or(Error::Fenced)?;
    if owner.worker != n.1 || w.get::<Simulated>(e).is_none() {
        return Err(Error::Fenced);
    }
    if w.resource::<MeshState>().input_sessions.get(&actor) != Some(&input.session) {
        n.0.session(input.session, actor)?;
        w.resource_mut::<MeshState>()
            .input_sessions
            .insert(actor, input.session);
    }
    Ok((group, owner))
}
/// Admit a bounded batch of replaceable movement samples at one authority snapshot.
/// Results retain input order. Session, live incarnation, generation, freeze and
/// time checks also apply when a previous sample for the actor was accepted.
pub fn apply_inputs(
    w: &mut World,
    inputs: &[(Id, Input)],
    now: u64,
) -> Result<Vec<Result<InputAck>>> {
    if inputs.len() > MAX_TARGETS {
        return Err(Error::Busy("input batch"));
    }
    let node = storage_node(w)?.clone();
    let handler = w
        .get_resource::<InputHandler>()
        .ok_or_else(|| invalid("game input handler missing"))?
        .0;
    let bindings: Vec<_> = inputs
        .iter()
        .map(|(id, i)| input_binding(w, &node, *id, i, now))
        .collect();
    let mut ids: Vec<_> = bindings
        .iter()
        .filter_map(|r| r.as_ref().ok().map(|v| v.0))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let grants = node.0.group_states(&ids, Some(node.1))?;
    Ok(inputs
        .iter()
        .zip(bindings)
        .map(|((actor, i), binding)| {
            let (group, owner) = binding?;
            let (observed, frozen) = grants.get(&group).ok_or(Error::Fenced)?;
            if owner != *observed {
                return Err(Error::Fenced);
            }
            if *frozen {
                return Err(Error::Busy("unit frozen for cutover"));
            }
            let e = w
                .resource::<MeshState>()
                .entities
                .get(actor)
                .copied()
                .ok_or(Error::Fenced)?;
            if w.get::<Simulated>(e).is_none()
                || w.get::<Managed>(e).is_none_or(|m| m.group != group)
            {
                return Err(Error::Fenced);
            }
            handler(w, *actor, i, now)
        })
        .collect())
}
/// Synchronize dirty canonical fields before an action uses positions or inventory
/// as economic inputs. Ordinary movement remains on the checkpoint cadence.
pub fn reconcile(w: &World, session: Id, actor: Id, group: Id) -> Result<(u64, u64, Record)> {
    let st = w.resource::<MeshState>();
    let (owner, seen) = *st.groups.get(&group).ok_or(Error::Fenced)?;
    let node = storage_node(w)?;
    node.0.session(session, actor)?;
    let (rev, seq) = node.0.progress(session, actor, group, owner)?;
    if rev != seen {
        return Err(Error::Busy("waiting for confirmed world revision"));
    }
    let r = capture(
        w,
        actor,
        View::Client {
            owner: actor,
            principal: actor,
            detail: Detail::Exact,
        },
    )?;
    if r.group != group {
        return Err(Error::Denied);
    }
    Ok((rev, seq, r))
}
pub fn checkpoint_group(w: &mut World, group: Id) -> Result<()> {
    let n = storage_node(w)?.clone();
    checkpoint_io::barrier(w, group)?;
    let mut ids: Vec<_> = w.resource::<MeshState>().pending.due[Checkpoints::slot(group)]
        .keys()
        .chain(&w.resource::<MeshState>().pending.urgent)
        .copied()
        .filter(|id| {
            w.resource::<MeshState>()
                .entities
                .get(id)
                .and_then(|e| w.get::<Managed>(*e))
                .is_some_and(|m| m.group == group)
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let removed: Vec<_> = w
        .resource::<MeshState>()
        .removed
        .iter()
        .filter(|(_, g)| **g == group)
        .map(|(&id, _)| id)
        .collect();
    if ids.is_empty() && removed.is_empty() {
        return Ok(());
    }
    let (owner, mut base) = w
        .resource::<MeshState>()
        .groups
        .get(&group)
        .copied()
        .ok_or(Error::Fenced)?;
    let mut records = Vec::new();
    for id in ids.drain(..) {
        let r = capture(w, id, View::Checkpoint)?;
        if w.resource::<MeshState>().saved.get(&id) != Some(&r.fields) {
            records.push(r);
        }
    }
    for id in removed {
        let mut r = Record::new(id, group);
        r.deleted = true;
        r.revision = w.resource::<MeshState>().version(id);
        records.push(r);
    }
    for batch in records.chunks_mut(MAX_TARGETS) {
        base = write_checkpoint(w, &n.0, group, owner, base, batch)?;
        checkpoint_io::acknowledge(w, group, owner, base, batch);
        for r in batch {
            let mut state = w.resource_mut::<MeshState>();
            state.removed.remove(&r.id);
            state.pending.remove(&r.id, group);
        }
    }
    Ok(())
}

/// Restore a group assigned to the local worker slot before starting simulation.
/// Register its codecs, rebuild hooks and data providers first. A retry on an
/// already active grant preserves live ECS changes. Opening a newer incarnation
/// of the same slot fences the previous worker.
pub fn restore_group(w: &mut World, group: Id) -> Result<Authority> {
    let Node(db, worker) = w
        .get_resource::<Node>()
        .cloned()
        .ok_or_else(|| invalid("restore_group requires MeshPlugin"))?;
    let owner = db.recover(group, worker)?;
    if w.resource::<MeshState>().authority(group) == Some(owner) {
        db.check_authority(group, owner, true)?;
        return Ok(owner);
    }
    let snapshot = db.snapshot(group)?;
    snapshot.prepare_with_data(w)?.activate(w, &db, owner)?;
    Ok(owner)
}

#[derive(Resource, Clone)]
struct Node(Store, Worker);
fn storage_node(w: &World) -> Result<&Node> {
    w.get_resource::<Node>()
        .ok_or_else(|| invalid("operation requires MeshPlugin::persistent"))
}
#[derive(Default)]
pub struct MeshPlugin {
    node: Option<Node>,
    transient: bool,
}
impl MeshPlugin {
    /// A standalone authoritative World with no database, network or recovery service.
    pub fn transient() -> Self {
        Self {
            node: None,
            transient: true,
        }
    }
    /// Install after starting this worker, then register the game's component schemas.
    /// Call [`restore_group`] for each previously owned group before the first tick.
    pub fn persistent(store: Store, worker: Worker) -> Self {
        Self {
            node: Some(Node(store, worker)),
            transient: false,
        }
    }
}
impl Plugin for MeshPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Schemas>()
            .init_resource::<MeshState>()
            .init_resource::<Actions>()
            .init_resource::<Providers>()
            .init_resource::<BlobCache>();
        if !self.transient {
            let n = self.node.clone().unwrap_or_else(|| {
                let db = Store::open(":memory:").expect("local mesh database");
                let w = db.start_worker(1).expect("first worker");
                Node(db, w)
            });
            app.insert_resource(n.0.clone()).insert_resource(n);
        }
        app.configure_sets(
            PreUpdate,
            (
                MeshSystems::Ingress,
                MeshSystems::Authority,
                MeshSystems::Control,
            )
                .chain(),
        )
        .configure_sets(
            PostUpdate,
            (TrackFields, MeshSystems::Track, MeshSystems::Output).chain(),
        )
        .add_systems(
            PreUpdate,
            (pull, discover).chain().in_set(MeshSystems::Authority),
        )
        .add_systems(
            PostUpdate,
            (track_despawns, flush)
                .chain()
                .after(TrackFields)
                .in_set(MeshSystems::Track),
        );
    }
}
#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub enum MeshSystems {
    /// Trusted adapter commands, external revocations and spawns. Deferred commands are applied before authority/input checks.
    Ingress,
    /// Extract subscriber state after gameplay, field tracking and spatial indexing.
    Output,
    /// Restore and fence ownership, then discover newly spawned objects in PreUpdate.
    Authority,
    /// Validate external control before ordinary gameplay runs in Update.
    Control,
    /// PostUpdate field tracking and storage checkpoint barrier.
    Track,
}
#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub(crate) struct TrackFields;
pub trait MeshApp {
    fn mesh_component<T: Component>(
        &mut self,
        id: u32,
        version: u16,
        rules: Rules,
        codec: ComponentCodec<T>,
    ) -> Result<&mut Self>;
    fn mesh_action<A: Action>(
        &mut self,
        handler: fn(&mut Edit<'_>, A) -> Result<A::Output>,
    ) -> Result<&mut Self>;
    fn mesh_provider<P: DataProvider>(&mut self, p: P) -> Result<&mut Self>;
    fn mesh_datasets<T: Component>(
        &mut self,
        f: fn(&T) -> Result<Vec<Manifest>>,
    ) -> Result<&mut Self>;
    fn mesh_rebuild(&mut self, f: Rebuild) -> &mut Self;
}
impl MeshApp for App {
    fn mesh_component<T: Component>(
        &mut self,
        id: u32,
        version: u16,
        rules: Rules,
        codec: ComponentCodec<T>,
    ) -> Result<&mut Self> {
        let mut reg = self.world_mut().resource_mut::<Schemas>();
        if id == 0
            || version == 0
            || reg.entries.contains_key(&id)
            || reg.types.contains_key(&TypeId::of::<T>())
        {
            return Err(invalid(format!(
                "duplicate or invalid schema {id} v{version}"
            )));
        }
        reg.types.insert(TypeId::of::<T>(), id);
        let ComponentCodec {
            encode,
            decode,
            references,
            resolve,
        } = codec;
        reg.entries.insert(
            id,
            Entry {
                version,
                rules,
                remove: |w, e| {
                    w.entity_mut(e).remove::<T>();
                },
                capture: Box::new(move |w, e, detail| {
                    w.get::<T>(e).map(|v| encode(v, detail)).transpose()
                }),
                import: Box::new(move |w, e, bytes, map| {
                    let mut value = decode(bytes)?;
                    for r in references(&value) {
                        if let Reference::RequiredLocal(id) = r {
                            if !map.contains_key(&id) {
                                return Err(Error::Missing(format!(
                                    "required local reference {id}"
                                )));
                            }
                        }
                    }
                    resolve(&mut value, map)?;
                    w.entity_mut(e).insert(value);
                    Ok(())
                }),
            },
        );
        self.add_systems(PostUpdate, track::<T>.in_set(TrackFields));
        Ok(self)
    }
    fn mesh_action<A: Action>(
        &mut self,
        h: fn(&mut Edit<'_>, A) -> Result<A::Output>,
    ) -> Result<&mut Self> {
        self.world_mut().resource_mut::<Actions>().register(h)?;
        Ok(self)
    }
    fn mesh_provider<P: DataProvider>(&mut self, p: P) -> Result<&mut Self> {
        self.world_mut()
            .resource_mut::<Providers>()
            .register(Arc::new(p))?;
        Ok(self)
    }
    fn mesh_datasets<T: Component>(
        &mut self,
        f: fn(&T) -> Result<Vec<Manifest>>,
    ) -> Result<&mut Self> {
        let mut schemas = self.world_mut().resource_mut::<Schemas>();
        let id = *schemas
            .types
            .get(&TypeId::of::<T>())
            .ok_or_else(|| invalid("register component before its datasets"))?;
        if schemas.data.contains_key(&id) {
            return Err(invalid("duplicate component dataset rule"));
        }
        schemas.data.insert(
            id,
            Box::new(move |w, e| w.get::<T>(e).map(f).unwrap_or(Ok(vec![]))),
        );
        Ok(self)
    }
    fn mesh_rebuild(&mut self, f: Rebuild) -> &mut Self {
        self.world_mut().resource_mut::<Schemas>().rebuild.push(f);
        self
    }
}
#[derive(Component)]
struct AwaitingGrant;

type Unclaimed = (
    Or<(Added<Managed>, With<AwaitingGrant>)>,
    Without<Replica>,
    Without<Staged>,
);
fn discover(w: &mut World, added: &mut QueryState<(Entity, &Managed), Unclaimed>) {
    let fresh: Vec<_> = added.iter(w).map(|(e, m)| (e, *m)).collect();
    if fresh.is_empty() {
        return;
    }
    let node = w.get_resource::<Node>().cloned();
    for (entity, m) in fresh {
        if w.resource::<MeshState>().entities.get(&m.id) == Some(&entity) {
            continue;
        }
        let grant = (|| -> Result<Option<(Authority, u64)>> {
            if w.resource::<MeshState>().entities.contains_key(&m.id) {
                return Err(invalid(format!("duplicate live identity {}", m.id)));
            }
            let Some(Node(db, worker)) = &node else {
                let mut state = w.resource_mut::<MeshState>();
                if !state.transient_names.insert(m.id) {
                    return Err(invalid(format!(
                        "transient object ID {} already used",
                        m.id
                    )));
                }
                return Ok(Some((
                    Authority {
                        worker: Worker {
                            slot: 1,
                            incarnation: 1,
                        },
                        generation: 1,
                    },
                    0,
                )));
            };
            if !matches!(db.lookup(m.id)?, Lookup::Unknown) {
                return Err(invalid(format!(
                    "identity {} requires verified restore",
                    m.id
                )));
            }
            let owner = match db.owner(m.group) {
                Ok(a) => a,
                Err(Error::Missing(_)) => db.create_group(m.group, *worker)?,
                Err(e) => return Err(e),
            };
            if owner.worker != *worker {
                return Err(Error::Fenced);
            }
            let base = db.check_authority(m.group, owner, true)?;
            Ok(Some((owner, base)))
        })();
        match grant {
            Ok(grant) => {
                let mut st = w.resource_mut::<MeshState>();
                st.entities.insert(m.id, entity);
                st.mapped.insert(entity, m);
                if node.is_some() {
                    st.pending.urgent.insert(m.id);
                    st.dirty.insert(m.id, ObjectChange::lifecycle());
                }
                if let Some((a, rev)) = grant {
                    st.groups.entry(m.group).or_insert((a, rev));
                }
                w.entity_mut(entity)
                    .remove::<AwaitingGrant>()
                    .insert(Simulated);
            }
            Err(e) => {
                if matches!(e, Error::Io(_) | Error::Busy(_)) {
                    w.entity_mut(entity).insert(AwaitingGrant);
                } else {
                    w.entity_mut(entity).remove::<AwaitingGrant>();
                }
                w.resource_mut::<MeshState>().report(e.to_string());
            }
        }
    }
}
pub(crate) fn pause_group(w: &mut World, group: Id, paused: bool) {
    let ids: Vec<_> = w
        .query::<(Entity, &Managed, Option<&Simulated>, Option<&Paused>)>()
        .iter(w)
        .filter(|(_, m, s, p)| m.group == group && if paused { s.is_some() } else { p.is_some() })
        .map(|(e, _, _, _)| e)
        .collect();
    for e in ids {
        if paused {
            w.entity_mut(e).remove::<Simulated>().insert(Paused);
        } else {
            w.entity_mut(e).remove::<Paused>().insert(Simulated);
        }
    }
}
fn pull(w: &mut World) {
    w.resource_mut::<MeshState>().tick += 1;
    let Some(Node(db, worker)) = w.get_resource::<Node>().cloned() else {
        return;
    };
    checkpoint_io::collect(w);
    let mark = w.resource::<MeshState>().mark;
    let notices = match db.notices(worker, mark) {
        Ok(n) => n,
        Err(e) => {
            w.resource_mut::<MeshState>().report(e.to_string());
            return;
        }
    };
    let mut groups = std::mem::take(&mut w.resource_mut::<MeshState>().retry_groups);
    for (mark, g, a, rev) in notices {
        w.resource_mut::<MeshState>().mark = mark;
        if a.worker != worker {
            demote(w, g);
            continue;
        }
        if w.resource::<MeshState>().groups.get(&g) != Some(&(a, rev)) {
            groups.insert(g);
        }
    }
    for g in groups {
        if w.resource::<MeshState>().external_staging.contains(&g) || checkpoint_io::pending(w, g) {
            w.resource_mut::<MeshState>().retry_groups.insert(g);
            continue;
        }
        let applied = (|| -> Result<()> {
            let a = db.owner(g)?;
            if a.worker != worker {
                demote(w, g);
                return Ok(());
            }
            let base = w.resource::<MeshState>().groups.get(&g).copied();
            if let Some((old, base)) = base {
                if old == a {
                    if let Ok((rev, records)) = db.since(g, base) {
                        apply_records(w, records, true)?;
                        w.resource_mut::<MeshState>().groups.insert(g, (a, rev));
                        return Ok(());
                    }
                }
            }
            let s = db.snapshot(g)?;
            let p = prepare_world(w, &s)?;
            p.activate(w, &db, a)
        })();
        if let Err(e) = applied {
            let mut st = w.resource_mut::<MeshState>();
            st.report(e.to_string());
            st.retry_groups.insert(g);
        }
    }
}
pub(crate) fn demote(w: &mut World, group: Id) {
    let members: Vec<_> = w
        .query_filtered::<(Entity, &Managed), Or<(With<Simulated>, With<Paused>)>>()
        .iter(w)
        .filter(|(_, m)| m.group == group)
        .map(|(e, m)| (e, m.id))
        .collect();
    for (e, id) in members {
        if let Some(mut source) = w.get_resource_mut::<SubscriberSource>() {
            source.remove(id);
        }
        w.entity_mut(e)
            .remove::<(Simulated, Paused)>()
            .insert(Replica);
        let mut st = w.resource_mut::<MeshState>();
        st.mapped.remove(&e);
        st.input_sessions.remove(&id);
        st.pending.remove(&id, group);
        st.dirty.insert(id, ObjectChange::lifecycle());
    }
    w.resource_mut::<MeshState>().groups.remove(&group);
    w.resource_mut::<MeshState>().pins.remove(&group);
}
fn import_field(
    world: &mut World,
    state: &MeshState,
    schemas: &Schemas,
    entity: Entity,
    schema: u32,
    field: &Field,
    previous: Option<&Field>,
) -> Result<()> {
    let entry = schemas
        .entries
        .get(&schema)
        .ok_or_else(|| invalid(format!("missing codec {schema}")))?;
    if entry.version != field.version {
        return Err(invalid(format!("schema {schema} version mismatch")));
    }
    if previous != Some(field) {
        (entry.import)(world, entity, &field.bytes, &state.entities)?;
    }
    Ok(())
}

/// Patch an already installed replica. The receiver keeps its last successfully
/// installed bytes for removals and codec comparisons, reusing their allocation.
pub(crate) fn patch_replica(w: &mut World, e: Entity, u: &Update) -> Result<()> {
    w.resource_scope(|w, mut state: Mut<MeshState>| -> Result<()> {
        let before = state.saved.get(&u.object);
        w.resource_scope(|w, reg: Mut<Schemas>| -> Result<()> {
            for (&id, value) in &u.fields {
                let previous = before.and_then(|fields| fields.get(&id));
                if let Some(f) = value {
                    import_field(w, &state, &reg, e, id, f, previous)?;
                } else if previous.is_some() {
                    (reg.entries[&id].remove)(w, e);
                }
            }
            if u.base.is_none() {
                for id in before.into_iter().flat_map(|m| m.keys()) {
                    if !u.fields.contains_key(id) {
                        (reg.entries[id].remove)(w, e);
                    }
                }
            }
            for rebuild in &reg.rebuild {
                rebuild(w, e)?;
            }
            Ok(())
        })?;
        let saved = state.saved.entry(u.object).or_default();
        if u.base.is_none() {
            saved.retain(|k, _| u.fields.contains_key(k));
        }
        for (&id, field) in &u.fields {
            if let Some(f) = field {
                if let Some(old) = saved.get_mut(&id) {
                    old.version = f.version;
                    old.bytes.clone_from(&f.bytes);
                } else {
                    saved.insert(id, f.clone());
                }
            } else {
                saved.remove(&id);
            }
        }
        state.versions.insert(u.object, u.revision);
        Ok(())
    })
}

pub(crate) fn apply_records(
    w: &mut World,
    records: impl AsRef<[Record]> + IntoIterator<Item = Record>,
    active: bool,
) -> Result<()> {
    w.resource_scope(|w, mut state: Mut<MeshState>| -> Result<()> {
        for r in records.as_ref() {
            if r.deleted {
                if state
                    .entities
                    .get(&r.id)
                    .and_then(|e| w.get::<Managed>(*e))
                    .is_some_and(|m| m.group != r.group)
                {
                    continue;
                }
                if let Some(e) = state.entities.remove(&r.id) {
                    state.input_sessions.remove(&r.id);
                    state.mapped.remove(&e);
                    state.pending.remove(&r.id, r.group);
                    state.saved.remove(&r.id);
                    state.versions.remove(&r.id);
                    state.dirty.insert(r.id, ObjectChange::lifecycle());
                    if let Some(mut source) = w.get_resource_mut::<SubscriberSource>() {
                        source.remove(r.id);
                    }
                    w.despawn(e);
                }
            } else if let std::collections::btree_map::Entry::Vacant(slot) =
                state.entities.entry(r.id)
            {
                let mut e = w.spawn(Managed::new(r.id, r.group));
                if active {
                    e.insert(Simulated);
                } else {
                    e.insert(Replica);
                }
                slot.insert(e.id());
            }
        }
        w.resource_scope(|w, reg: Mut<Schemas>| -> Result<()> {
            for mut r in records.into_iter().filter(|r| !r.deleted) {
                if active {
                    r.fields.retain(|id, _| {
                        reg.entries.get(id).is_none_or(|entry| entry.rules.persist)
                    });
                }
                let e = state.entities[&r.id];
                if w.get::<Managed>(e).is_some_and(|m| m.group != r.group) {
                    w.entity_mut(e).insert(Managed::new(r.id, r.group));
                }
                if active {
                    w.entity_mut(e).remove::<Replica>().insert(Simulated);
                }
                let before = state.saved.get(&r.id);
                for (id, f) in &r.fields {
                    import_field(
                        w,
                        &state,
                        &reg,
                        e,
                        *id,
                        f,
                        before.and_then(|fields| fields.get(id)),
                    )?;
                }
                for id in before.into_iter().flat_map(|v| v.keys()) {
                    if !r.fields.contains_key(id) && (!active || reg.entries[id].rules.persist) {
                        (reg.entries[id].remove)(w, e);
                    }
                }
                for rebuild in &reg.rebuild {
                    rebuild(w, e)?;
                }
                state.saved.insert(r.id, r.fields);
                state.versions.insert(r.id, r.revision);
                if active {
                    if !state.mapped.contains_key(&e) {
                        state.dirty.insert(r.id, ObjectChange::lifecycle());
                    }
                    state.mapped.insert(e, Managed::new(r.id, r.group));
                }
            }
            Ok(())
        })
    })
}
fn track<T: Component>(
    changed: Query<&Managed, (Changed<T>, With<Simulated>)>,
    mut removed: RemovedComponents<T>,
    schemas: Res<Schemas>,
    spatial: Option<Res<SpatialDependencies>>,
    node: Option<Res<Node>>,
    mut source: Option<ResMut<SubscriberSource>>,
    mut s: ResMut<MeshState>,
) {
    if node.is_none() && source.is_none() {
        removed.clear();
        return;
    }
    let schema = schemas.types[&TypeId::of::<T>()];
    let rules = schemas.entries[&schema].rules;
    let spatial = spatial.is_none_or(|inputs| inputs.0.contains(&schema));
    let mut mark = |s: &mut MeshState, m: &Managed| {
        if let Some(source) = source.as_deref_mut() {
            source.changed(m.id, schema, rules.visibility, spatial, s.tick);
        }
        if node.is_none() {
            return;
        }
        if rules.persist {
            s.pending.changed(m, schema);
        }
        if !spatial && matches!(rules.visibility, Visibility::Local | Visibility::Private) {
            return;
        }
        s.dirty
            .entry(m.id)
            .and_modify(|change| {
                change.spatial |= spatial;
                change.fields.add(schema);
            })
            .or_insert_with(|| ObjectChange {
                spatial,
                fields: ChangedFields::new(schema),
            });
    };
    for m in &changed {
        mark(&mut s, m);
    }
    for e in removed.read() {
        if let Some(m) = s.mapped.get(&e).copied() {
            mark(&mut s, &m);
        }
    }
}
fn track_despawns(
    mut removed: RemovedComponents<Managed>,
    mut st: ResMut<MeshState>,
    node: Option<Res<Node>>,
    mut source: Option<ResMut<SubscriberSource>>,
) {
    for entity in removed.read() {
        if let Some(m) = st.mapped.remove(&entity) {
            if let Some(source) = source.as_deref_mut() {
                source.remove(m.id);
            }
            if st.entities.get(&m.id) == Some(&entity) {
                st.input_sessions.remove(&m.id);
                st.entities.remove(&m.id);
                st.pending.remove(&m.id, m.group);
                if node.is_some() {
                    st.removed.insert(m.id, m.group);
                    st.dirty.insert(m.id, ObjectChange::lifecycle());
                } else if !st.mapped.values().any(|member| member.group == m.group) {
                    st.groups.remove(&m.group);
                }
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalObject {
    Unknown,
    Unloaded,
    Deleted,
    Present(Entity),
}
pub fn local_object(w: &World, id: Id) -> Result<LocalObject> {
    if let Some(e) = w.resource::<MeshState>().entities.get(&id) {
        return Ok(LocalObject::Present(*e));
    }
    Ok(match w.resource::<Store>().lookup(id)? {
        Lookup::Present(_) => LocalObject::Unloaded,
        Lookup::Deleted => LocalObject::Deleted,
        Lookup::Unknown => LocalObject::Unknown,
    })
}
pub fn capture(w: &World, id: Id, view: View) -> Result<Record> {
    let st = w.resource::<MeshState>();
    let e = *st
        .entities
        .get(&id)
        .ok_or_else(|| Error::Missing(id.to_string()))?;
    let m = w
        .get::<Managed>(e)
        .ok_or_else(|| Error::Missing(id.to_string()))?;
    let mut r = Record::new(id, m.group);
    r.revision = st.versions.get(&id).copied().unwrap_or(0);
    r.fields = capture_fields(w, e, w.resource::<Schemas>(), view)?;
    Ok(r)
}

fn capture_fields(
    w: &World,
    e: Entity,
    schemas: &Schemas,
    view: View,
) -> Result<BTreeMap<u32, Field>> {
    let mut fields = BTreeMap::new();
    for (schema, entry) in &schemas.entries {
        if let Some(d) = entry.rules.detail(view) {
            if let Some(bytes) = (entry.capture)(w, e, d)? {
                if bytes.len() > MAX_FIELD {
                    return Err(Error::Busy("component bytes"));
                }
                fields.insert(
                    *schema,
                    Field {
                        version: entry.version,
                        bytes,
                    },
                );
            }
        }
    }
    Ok(fields)
}
pub fn project_component(w: &World, id: Id, schema: u32, view: View) -> Result<Option<Field>> {
    let s = w.resource::<Schemas>();
    let entry = s
        .entries
        .get(&schema)
        .ok_or_else(|| invalid(format!("unknown subscription schema {schema}")))?;
    let Some(detail) = entry.rules.detail(view) else {
        return Ok(None);
    };
    let e = *w
        .resource::<MeshState>()
        .entities
        .get(&id)
        .ok_or_else(|| Error::Missing(id.to_string()))?;
    let bytes = (entry.capture)(w, e, detail)?;
    Ok(bytes.map(|bytes| Field {
        version: entry.version,
        bytes,
    }))
}
fn checkpoint_claims(w: &World, records: &[Record]) -> Result<BTreeMap<Id, Vec<Manifest>>> {
    let mut claims = BTreeMap::new();
    let state = w.resource::<MeshState>();
    let schemas = w.resource::<Schemas>();
    for r in records.iter().filter(|r| !r.deleted) {
        let old = state.saved.get(&r.id);
        if !schemas
            .data
            .keys()
            .any(|id| old.and_then(|v| v.get(id)) != r.fields.get(id))
        {
            continue;
        }
        let e = state.entities[&r.id];
        let mut required = Vec::new();
        for rule in schemas.data.values() {
            required.extend(rule(w, e)?);
        }
        claims.insert(r.id, required);
    }
    Ok(claims)
}
fn write_checkpoint(
    w: &World,
    db: &Store,
    g: Id,
    a: Authority,
    base: u64,
    records: &[Record],
) -> Result<u64> {
    db.checkpoint_data(
        g,
        a,
        base,
        records.to_vec(),
        &checkpoint_claims(w, records)?,
    )
}
/// Commit a server-generated batch using the local simulation grant.
pub fn commit_batch(
    w: &mut World,
    actor: Id,
    intents: &[Intent],
    now_ms: u64,
) -> Result<Vec<Reply>> {
    let Some(first) = intents.first() else {
        return Ok(vec![]);
    };
    if intents
        .iter()
        .any(|i| i.group != first.group || i.key.session != first.key.session)
    {
        return Err(invalid("batch requires one group and session"));
    }
    let node = storage_node(w)?.clone();
    let owner = w
        .resource::<MeshState>()
        .authority(first.group)
        .ok_or(Error::Fenced)?;
    if owner.worker != node.1 {
        return Err(Error::Fenced);
    }
    checkpoint_group(w, first.group)?;
    node.0.session(first.key.session, actor)?;
    node.0
        .execute(w.resource::<Actions>(), actor, owner, now_ms, intents)
}
fn flush(w: &mut World) {
    let Some(Node(db, _)) = w.get_resource::<Node>().cloned() else {
        return;
    };
    let started = w
        .contains_resource::<WorkerTimings>()
        .then(std::time::Instant::now);
    let mut batches: BTreeMap<Id, Vec<Record>> = BTreeMap::new();
    let tick = w.resource::<MeshState>().tick;
    let changed = w.resource_mut::<MeshState>().pending.take_due(tick);
    for (id, changed_fields) in changed {
        let Some(&e) = w.resource::<MeshState>().entities.get(&id) else {
            continue;
        };
        if w.get::<Simulated>(e).is_none() {
            continue;
        }
        let group = w.get::<Managed>(e).unwrap().group;
        if checkpoint_io::pending(w, group) {
            w.resource_mut::<MeshState>().pending.urgent.insert(id);
            continue;
        }
        if changed_fields.slice().is_none() {
            // An urgent full checkpoint also covers the ordinary checkpoint slot.
            w.resource_mut::<MeshState>().pending.remove(&id, group);
        }
        match checkpoint_changes(w, id, e, &changed_fields) {
            Ok(Some(r)) => {
                batches.entry(r.group).or_default().push(r);
            }
            Ok(None) => (),
            Err(err) => {
                let mut st = w.resource_mut::<MeshState>();
                st.report(err.to_string());
                st.pending.urgent.insert(id);
            }
        }
    }
    let removed = std::mem::take(&mut w.resource_mut::<MeshState>().removed);
    for (id, g) in removed {
        let mut r = Record::new(id, g);
        r.revision = w
            .resource::<MeshState>()
            .versions
            .get(&id)
            .copied()
            .unwrap_or(0);
        r.deleted = true;
        batches.entry(g).or_default().push(r);
    }
    for (g, mut items) in batches {
        let Some((a, mut base)) = w.resource::<MeshState>().groups.get(&g).copied() else {
            continue;
        };
        if w.contains_resource::<AsyncCheckpoints>() {
            let rest = items.split_off(items.len().min(MAX_TARGETS));
            checkpoint_io::retry(w, g, &rest);
            if let Err(e) = checkpoint_io::schedule(w, &db, g, a, base, &mut items) {
                if !matches!(e, Error::Busy("checkpoint queue")) {
                    w.resource_mut::<MeshState>().report(e.to_string());
                }
                checkpoint_io::retry(w, g, &items);
            }
            continue;
        }
        for chunk in items.chunks_mut(MAX_TARGETS) {
            match write_checkpoint(w, &db, g, a, base, chunk) {
                Ok(next) => {
                    checkpoint_io::acknowledge(w, g, a, next, chunk);
                    base = next;
                }
                Err(e) => {
                    let mut st = w.resource_mut::<MeshState>();
                    st.report(e.to_string());
                    for r in chunk {
                        if r.deleted {
                            st.removed.insert(r.id, g);
                        } else {
                            st.pending.urgent.insert(r.id);
                        }
                    }
                }
            }
        }
    }
    if let Some(start) = started {
        w.resource_mut::<WorkerTimings>().checkpoint_us += start.elapsed().as_micros() as u64;
    }
}
fn checkpoint_changes(
    w: &World,
    id: Id,
    e: Entity,
    changed: &ChangedFields,
) -> Result<Option<Record>> {
    let state = w.resource::<MeshState>();
    let saved = state.saved.get(&id);
    if let Some((schemas, base)) = changed.slice().zip(saved) {
        let mut updated = None;
        for &schema in schemas {
            let field = project_component(w, id, schema, View::Checkpoint)?;
            if base.get(&schema) != field.as_ref() {
                let fields = updated.get_or_insert_with(|| base.clone());
                if let Some(value) = field {
                    fields.insert(schema, value);
                } else {
                    fields.remove(&schema);
                }
            }
        }
        return Ok(updated.map(|fields| Record {
            id,
            group: w.get::<Managed>(e).unwrap().group,
            revision: state.version(id),
            deleted: false,
            fields,
        }));
    }
    let r = capture(w, id, View::Checkpoint)?;
    Ok((saved != Some(&r.fields)).then_some(r))
}
struct PreparedMember {
    entity: Entity,
    revision: u64,
    checkpoint: BTreeMap<u32, Field>,
}

/// Staged entities belong to the preparing world. Activate or discard them in that world.
#[must_use = "activate or discard the prepared group in the same world"]
pub struct Prepared {
    world: bevy_ecs::world::WorldId,
    entities: BTreeMap<Id, PreparedMember>,
    pub(crate) manifest: Digest,
    group: Id,
    revision: u64,
    pins: Vec<BlobPin>,
}
impl Prepared {
    pub fn hash(&self) -> Digest {
        self.manifest
    }
    pub fn discard(self, w: &mut World) {
        if self.world == w.id() {
            for member in self.entities.into_values() {
                w.despawn(member.entity);
            }
        }
    }
    /// Activation verifies the durable owner and final revision again after the cutover.
    pub fn activate(self, w: &mut World, db: &Store, owner: Authority) -> Result<()> {
        if self.world != w.id() {
            return Err(invalid("use the world that prepared this group"));
        }
        let validation = db.check_authority(self.group, owner, true).and_then(|rev| {
            if rev != self.revision {
                return Err(Error::Fenced);
            }
            Ok(())
        });
        if let Err(err) = validation {
            self.discard(w);
            return Err(err);
        }
        if let Some(Node(_, local)) = w.get_resource::<Node>() {
            if *local != owner.worker {
                self.discard(w);
                return Err(Error::Denied);
            }
        }
        if self
            .entities
            .values()
            .any(|member| w.get::<Staged>(member.entity).is_none())
        {
            self.discard(w);
            return Err(invalid("staged group member disappeared before activation"));
        }
        let previous: Vec<_> = w
            .query_filtered::<(Entity, &Managed), Without<Staged>>()
            .iter(w)
            .filter(|(_, m)| m.group == self.group || self.entities.contains_key(&m.id))
            .map(|(e, m)| (e, *m))
            .collect();
        for (e, m) in previous {
            let mut st = w.resource_mut::<MeshState>();
            st.mapped.remove(&e);
            st.forget_view(m.id);
            st.pending.remove(&m.id, m.group);
            w.despawn(e);
        }
        for (id, member) in self.entities {
            let e = member.entity;
            w.entity_mut(e).remove::<Staged>().insert(Simulated);
            let mut st = w.resource_mut::<MeshState>();
            st.entities.insert(id, e);
            st.versions.insert(id, member.revision);
            st.mapped.insert(e, Managed::new(id, self.group));
            st.dirty.insert(id, ObjectChange::lifecycle());
            st.saved.insert(id, member.checkpoint);
        }
        w.resource_mut::<MeshState>()
            .pins
            .insert(self.group, self.pins);
        w.resource_mut::<MeshState>()
            .groups
            .insert(self.group, (owner, self.revision));
        Ok(())
    }
}
impl Snapshot {
    /// Synchronously fetch and verify required provider data before staging a stored group.
    /// For a live handoff, prefetch off the tick path and use [`prepare_world`].
    pub fn prepare_with_data(&self, w: &mut World) -> Result<Prepared> {
        for manifest in &self.required {
            let provider = w.resource::<Providers>().get(manifest.key.provider)?;
            w.resource_mut::<BlobCache>()
                .fetch(manifest, provider.as_ref())?;
        }
        prepare_world(w, self)
    }
}

pub fn prepare_world(w: &mut World, s: &Snapshot) -> Result<Prepared> {
    let mut pins = vec![];
    for required in &s.required {
        if !w.resource::<BlobCache>().ready(required) {
            return Err(Error::Missing(format!(
                "verified dataset {:?}",
                required.key
            )));
        }
        pins.push(w.resource::<BlobCache>().pin(required)?);
        for dep in &required.dependencies {
            if !s.required.iter().any(|m| m.key == *dep) {
                return Err(Error::Missing(format!("dataset dependency {dep:?}")));
            }
        }
    }
    let manifest = s.hash()?;
    let mut entities = BTreeMap::new();
    let mut map = w.resource::<MeshState>().entities.clone();
    let mut seen = BTreeSet::new();
    if s.records
        .iter()
        .any(|r| r.group != s.group || r.revision > s.revision || !seen.insert(r.id))
    {
        return Err(invalid("invalid snapshot member"));
    }
    map.retain(|_, e| w.get::<Managed>(*e).is_some_and(|m| m.group != s.group));
    for r in &s.records {
        if !r.deleted {
            let e = w.spawn((Managed::new(r.id, s.group), Staged)).id();
            map.insert(r.id, e);
            entities.insert(
                r.id,
                PreparedMember {
                    entity: e,
                    revision: r.revision,
                    checkpoint: BTreeMap::new(),
                },
            );
        } else {
            map.remove(&r.id);
        }
    }
    let result = w.resource_scope(|w, schemas: Mut<Schemas>| -> Result<()> {
        for r in s.records.iter().filter(|r| !r.deleted) {
            let e = entities[&r.id].entity;
            for (id, value) in &r.fields {
                let entry = schemas
                    .entries
                    .get(id)
                    .ok_or_else(|| invalid(format!("missing codec for schema {id}")))?;
                if !entry.rules.migrate && !entry.rules.persist {
                    continue;
                }
                if value.version != entry.version {
                    return Err(invalid(format!("incompatible version of schema {id}")));
                }
                (entry.import)(w, e, &value.bytes, &map)?;
            }
        }
        for member in entities.values() {
            for f in &schemas.rebuild {
                f(w, member.entity)?;
            }
        }
        // Game codecs can fail. Finish all fallible work while the source still owns the group.
        for member in entities.values_mut() {
            member.checkpoint = capture_fields(w, member.entity, &schemas, View::Checkpoint)?;
        }
        Ok(())
    });
    let prepared = Prepared {
        world: w.id(),
        entities,
        manifest,
        group: s.group,
        revision: s.revision,
        pins,
    };
    if let Err(err) = result {
        prepared.discard(w);
        return Err(err);
    }
    Ok(prepared)
}
