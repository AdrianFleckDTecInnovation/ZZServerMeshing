use crate::*;
use bevy_app::{App, Plugin, PostUpdate, PreUpdate};
use bevy_ecs::prelude::*;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pending: BTreeSet<Id>,
    removed: BTreeMap<Id, Id>,
    pub diagnostics: std::collections::VecDeque<String>,
    versions: BTreeMap<Id, u64>,
    groups: BTreeMap<Id, (Authority, u64)>,
    mark: u64,
    tick: u64,
    pub(crate) dirty: BTreeSet<Id>,
    retry_groups: BTreeSet<Id>,
    pub(crate) external_staging: BTreeSet<Id>,
    pins: BTreeMap<Id, Vec<BlobPin>>,
    saved: BTreeMap<Id, BTreeMap<u32, Field>>,
}
impl MeshState {
    pub(crate) fn forget_view(&mut self, id: Id) {
        self.entities.remove(&id);
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
        self.groups.get(&g).map(|g| g.0)
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
    if input.sequence == 0
        || input.at_ms > now.saturating_add(100)
        || now.saturating_sub(input.at_ms) > 500
    {
        return Err(invalid("input outside server time window"));
    }
    let n = w.resource::<Node>().clone();
    n.0.session(input.session, actor)?;
    let e = *w
        .resource::<MeshState>()
        .entities
        .get(&actor)
        .ok_or_else(|| Error::Missing("input actor".into()))?;
    let managed = w.get::<Managed>(e).ok_or(Error::Denied)?;
    let owner = n.0.owner(managed.group)?;
    if owner.worker != n.1 || w.get::<Simulated>(e).is_none() {
        return Err(Error::Fenced);
    }
    n.0.check_authority(managed.group, owner, true)?;
    let handler = w
        .get_resource::<InputHandler>()
        .ok_or_else(|| invalid("game input handler missing"))?
        .0;
    handler(w, actor, input, now)
}
/// Synchronize dirty canonical fields before an action uses positions or inventory
/// as economic inputs. Ordinary movement remains on the checkpoint cadence.
pub fn reconcile(w: &World, session: Id, actor: Id, group: Id) -> Result<(u64, u64, Record)> {
    let st = w.resource::<MeshState>();
    let (owner, seen) = *st.groups.get(&group).ok_or(Error::Fenced)?;
    let node = w.resource::<Node>();
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
    let n = w.resource::<Node>().clone();
    let mut ids: Vec<_> = w
        .resource::<MeshState>()
        .pending
        .iter()
        .copied()
        .filter(|id| {
            w.resource::<MeshState>()
                .entities
                .get(id)
                .and_then(|e| w.get::<Managed>(*e))
                .is_some_and(|m| m.group == group)
        })
        .collect();
    if ids.is_empty() {
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
    for batch in records.chunks(MAX_TARGETS) {
        base = write_checkpoint(w, &n.0, group, owner, base, batch)?;
        let mut state = w.resource_mut::<MeshState>();
        for r in batch {
            state.versions.insert(r.id, base);
            state.saved.insert(r.id, r.fields.clone());
            state.pending.remove(&r.id);
        }
        state.groups.insert(group, (owner, base));
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
#[derive(Default)]
pub struct MeshPlugin {
    node: Option<Node>,
}
impl MeshPlugin {
    /// Install after starting this worker, then register the game's component schemas.
    /// Call [`restore_group`] for each previously owned group before the first tick.
    pub fn persistent(store: Store, worker: Worker) -> Self {
        Self {
            node: Some(Node(store, worker)),
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
        let n = self.node.clone().unwrap_or_else(|| {
            let db = Store::open(":memory:").expect("local mesh database");
            let w = db.start_worker(1).expect("first worker");
            Node(db, w)
        });
        app.insert_resource(n.0.clone()).insert_resource(n);
        app.add_systems(PreUpdate, (pull, discover).chain())
            .add_systems(
                PostUpdate,
                (track_despawns, flush).chain().after(TrackFields),
            );
    }
}
#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
struct TrackFields;
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
                return Ok(None);
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
                st.pending.insert(m.id);
                st.dirty.insert(m.id);
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
        if w.resource::<MeshState>().external_staging.contains(&g) {
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
        w.entity_mut(e)
            .remove::<(Simulated, Paused)>()
            .insert(Replica);
        let mut st = w.resource_mut::<MeshState>();
        st.mapped.remove(&e);
        st.pending.remove(&id);
        st.dirty.insert(id);
    }
    w.resource_mut::<MeshState>().groups.remove(&group);
    w.resource_mut::<MeshState>().pins.remove(&group);
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
                    state.mapped.remove(&e);
                    state.pending.remove(&r.id);
                    state.saved.remove(&r.id);
                    state.versions.remove(&r.id);
                    state.dirty.insert(r.id);
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
            for r in records.into_iter().filter(|r| !r.deleted) {
                let e = state.entities[&r.id];
                if w.get::<Managed>(e).is_some_and(|m| m.group != r.group) {
                    w.entity_mut(e).insert(Managed::new(r.id, r.group));
                }
                if active {
                    w.entity_mut(e).remove::<Replica>().insert(Simulated);
                }
                let before = state.saved.get(&r.id);
                for (id, f) in &r.fields {
                    let entry = reg
                        .entries
                        .get(id)
                        .ok_or_else(|| invalid(format!("missing codec {id}")))?;
                    if entry.version != f.version {
                        return Err(invalid(format!("schema {id} version mismatch")));
                    }
                    if before.and_then(|fields| fields.get(id)) != Some(f) {
                        (entry.import)(w, e, &f.bytes, &state.entities)?;
                    }
                }
                for id in before.into_iter().flat_map(|v| v.keys()) {
                    if !r.fields.contains_key(id) {
                        (reg.entries[id].remove)(w, e);
                    }
                }
                for rebuild in &reg.rebuild {
                    rebuild(w, e)?;
                }
                state.saved.insert(r.id, r.fields);
                state.versions.insert(r.id, r.revision);
                if active {
                    state.dirty.insert(r.id);
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
    mut s: ResMut<MeshState>,
) {
    for m in &changed {
        s.pending.insert(m.id);
        s.dirty.insert(m.id);
    }
    for e in removed.read() {
        if let Some(m) = s.mapped.get(&e).copied() {
            s.pending.insert(m.id);
            s.dirty.insert(m.id);
        }
    }
}
fn track_despawns(mut removed: RemovedComponents<Managed>, mut st: ResMut<MeshState>) {
    for entity in removed.read() {
        if let Some(m) = st.mapped.remove(&entity) {
            if st.entities.get(&m.id) == Some(&entity) {
                st.entities.remove(&m.id);
                st.pending.remove(&m.id);
                st.removed.insert(m.id, m.group);
                st.dirty.insert(m.id);
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
fn write_checkpoint(
    w: &World,
    db: &Store,
    g: Id,
    a: Authority,
    base: u64,
    records: &[Record],
) -> Result<u64> {
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
    db.checkpoint_data(g, a, base, records.to_vec(), &claims)
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
    let node = w.resource::<Node>().clone();
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
    let state = w.resource::<MeshState>();
    if state.tick % 20 != 1
        && state.removed.is_empty()
        && state
            .pending
            .iter()
            .all(|id| state.versions.contains_key(id))
    {
        return;
    }
    let Some(Node(db, _)) = w.get_resource::<Node>().cloned() else {
        return;
    };
    let mut batches: BTreeMap<Id, Vec<Record>> = BTreeMap::new();
    let changed = std::mem::take(&mut w.resource_mut::<MeshState>().pending);
    for id in changed {
        let Some(&e) = w.resource::<MeshState>().entities.get(&id) else {
            continue;
        };
        if w.get::<Simulated>(e).is_none() {
            continue;
        }
        match capture(w, id, View::Checkpoint) {
            Ok(r) => {
                if w.resource::<MeshState>().saved.get(&id) == Some(&r.fields) {
                    continue;
                }
                batches.entry(r.group).or_default().push(r);
            }
            Err(err) => {
                let mut st = w.resource_mut::<MeshState>();
                st.report(err.to_string());
                st.pending.insert(id);
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
    for (g, items) in batches {
        let Some((a, mut base)) = w.resource::<MeshState>().groups.get(&g).copied() else {
            continue;
        };
        for chunk in items.chunks(MAX_TARGETS) {
            match write_checkpoint(w, &db, g, a, base, chunk) {
                Ok(next) => {
                    let mut st = w.resource_mut::<MeshState>();
                    for r in chunk {
                        st.versions.insert(r.id, next);
                        if r.deleted {
                            st.saved.remove(&r.id);
                            st.versions.remove(&r.id);
                        } else {
                            st.saved.insert(r.id, r.fields.clone());
                        }
                    }
                    st.groups.insert(g, (a, next));
                    base = next;
                }
                Err(e) => {
                    let mut st = w.resource_mut::<MeshState>();
                    st.report(e.to_string());
                    for r in chunk {
                        if r.deleted {
                            st.removed.insert(r.id, g);
                        } else {
                            st.pending.insert(r.id);
                        }
                    }
                }
            }
        }
    }
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
            st.pending.remove(&m.id);
            w.despawn(e);
        }
        for (id, member) in self.entities {
            let e = member.entity;
            w.entity_mut(e).remove::<Staged>().insert(Simulated);
            let mut st = w.resource_mut::<MeshState>();
            st.entities.insert(id, e);
            st.versions.insert(id, member.revision);
            st.mapped.insert(e, Managed::new(id, self.group));
            st.dirty.insert(id);
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
