//! Player-specific sight. The octree supplies candidates, the game supplies types and distances.
use crate::*;
use bevy_ecs::prelude::{Entity, Resource, World};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SightPoint {
    pub kind: u32,
    pub mm: [i64; 3],
}

/// Return a game-defined target type and an exact point inside its physical presence.
#[derive(Resource)]
pub struct SightRule(pub fn(&World, Entity) -> Option<(Id, SightPoint)>);

#[derive(Clone, Resource)]
pub struct SightRanges {
    observer_kind: u32,
    pub(crate) radii: BTreeMap<u32, u64>,
    max: u64,
    bounds_kinds: BTreeSet<u32>,
}
impl SightRanges {
    pub fn millimetres(observer_kind: u32, radii: impl IntoIterator<Item = (u32, u64)>) -> Self {
        let radii: BTreeMap<_, _> = radii.into_iter().collect();
        Self {
            observer_kind,
            bounds_kinds: Default::default(),
            max: radii.values().copied().max().unwrap_or(0),
            radii,
        }
    }
    /// For these target kinds, sight ends at the nearest point of their registered physical AABB.
    /// Other kinds keep their exact game-provided anchor. Use the same policy on all workers.
    pub fn with_bounds(mut self, kinds: impl IntoIterator<Item = u32>) -> Self {
        self.bounds_kinds.extend(kinds);
        self
    }
    fn target(&self, origin: SightOrigin, point: SightPoint, bounds: Box3) -> SightPoint {
        if self.bounds_kinds.contains(&point.kind) {
            SightPoint {
                kind: point.kind,
                mm: std::array::from_fn(|axis| {
                    origin.mm[axis].clamp(bounds.lo[axis], bounds.hi[axis])
                }),
            }
        } else {
            point
        }
    }
    pub(crate) fn current_target(
        &self,
        world: &World,
        entity: Entity,
        origin: SightOrigin,
        frame: Id,
        point: SightPoint,
    ) -> Option<SightPoint> {
        if !self.bounds_kinds.contains(&point.kind) {
            return Some(point);
        }
        let bounds = (world.get_resource::<SpatialRule>()?.0)(world, entity)?.bounds;
        if bounds.frame != frame || (0..3).any(|a| bounds.lo[a] > bounds.hi[a]) {
            return None;
        }
        Some(self.target(origin, point, bounds))
    }
}
impl SightOrigin {
    pub(crate) fn includes(self, radii: &BTreeMap<u32, u64>, frame: Id, point: SightPoint) -> bool {
        let Some(&r) = radii.get(&point.kind) else {
            return false;
        };
        if frame != self.frame {
            return false;
        }
        // Each u64 difference squares exactly in u128. Subtract the available
        // budget per axis so even three extreme coordinates cannot overflow.
        let mut left = u128::from(r).pow(2);
        for (a, b) in self.mm.into_iter().zip(point.mm) {
            let Some(rest) = left.checked_sub(u128::from(a.abs_diff(b)).pow(2)) else {
                return false;
            };
            left = rest;
        }
        true
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SightOrigin {
    pub frame: Id,
    pub mm: [i64; 3],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SightPlan {
    pub origin: SightOrigin,
    observer_kind: u32,
    radii: BTreeMap<u32, u64>,
    pub objects: Vec<Id>,
    pub next: Option<Id>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SightPage {
    pub records: Vec<Record>,
    pub next: Option<Id>,
}
impl SightPage {
    /// Identifies the exact ordered public payload independently of the paging cursor.
    /// Include transient fields even when the durable object revision is unchanged.
    pub fn content_digest(&self) -> Digest {
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        for r in &self.records {
            hash.update(&r.id.0);
            hash.update(&r.group.0);
            hash.update(&r.revision.to_le_bytes());
            hash.update(&[u8::from(r.deleted)]);
            hash.update(&(r.fields.len() as u64).to_le_bytes());
            for (schema, field) in &r.fields {
                hash.update(&schema.to_le_bytes());
                hash.update(&field.version.to_le_bytes());
                hash.update(&(field.bytes.len() as u64).to_le_bytes());
                hash.update(&field.bytes);
            }
        }
        Digest(hash.finish().as_ref().try_into().unwrap())
    }
}
fn current_entity(
    world: &World,
    id: Id,
    worker: Worker,
    (group, owner): (Id, Authority),
) -> Option<Entity> {
    let state = world.resource::<MeshState>();
    if owner.worker != worker || state.authority(group) != Some(owner) {
        return None;
    }
    let &entity = state.entities.get(&id)?;
    world.get::<Simulated>(entity)?;
    (world.get::<Managed>(entity)?.group == group).then_some(entity)
}
impl Directory {
    pub(crate) fn visit_visible(
        &self,
        viewer: Id,
        rules: &SightRanges,
        mut point_for: impl FnMut(&Presence) -> Option<SightPoint>,
        mut visit: impl FnMut(Id, Worker),
    ) -> Result<SightOrigin> {
        let p = self
            .presence(viewer)
            .ok_or_else(|| Error::Missing("observer position".into()))?;
        let point = point_for(&p)
            .filter(|v| v.kind == rules.observer_kind && p.principal == viewer)
            .ok_or(Error::Denied)?;
        let origin = SightOrigin {
            frame: p.bounds.frame,
            mm: point.mm,
        };
        let offset = |v: i64, amount: i128| {
            (i128::from(v) + amount).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        };
        let r = i128::from(rules.max);
        let bounds = Box3::new(
            origin.frame,
            point.mm.map(|v| offset(v, -r)),
            point.mm.map(|v| offset(v, r)),
        )?;
        for id in self.sight_candidates(bounds, viewer) {
            if id == viewer {
                continue;
            }
            if let Some(target) = self.presence(id) {
                if point_for(&target).is_some_and(|point| {
                    origin.includes(
                        &rules.radii,
                        target.bounds.frame,
                        rules.target(origin, point, target.bounds),
                    )
                }) {
                    visit(id, target.owner);
                }
            }
        }
        Ok(origin)
    }
    pub(crate) fn sight_workers(
        &self,
        viewer: Id,
        rules: &SightRanges,
    ) -> Result<BTreeSet<Worker>> {
        let mut workers = BTreeSet::new();
        self.visit_visible(
            viewer,
            rules,
            |p| self.sight_points.get(&p.id).copied(),
            |_, w| {
                workers.insert(w);
            },
        )?;
        Ok(workers)
    }
    pub(crate) fn sight_plan(
        &self,
        viewer: Id,
        rules: &SightRanges,
        owner: Worker,
        after: Option<Id>,
    ) -> Result<SightPlan> {
        let mut objects = Vec::new();
        let origin = self.visit_visible(
            viewer,
            rules,
            |p| self.sight_points.get(&p.id).copied(),
            |id, w| {
                if w == owner && after.is_none_or(|cursor| id > cursor) {
                    objects.push(id);
                }
            },
        )?;
        let more = objects.len() > MAX_TARGETS;
        if more {
            objects.select_nth_unstable(MAX_TARGETS);
            objects.truncate(MAX_TARGETS);
        }
        objects.sort_unstable();
        let next = more.then(|| *objects.last().unwrap());
        Ok(SightPlan {
            origin,
            observer_kind: rules.observer_kind,
            radii: rules.radii.clone(),
            objects,
            next,
        })
    }
}

/// Project the current authoritative state and recheck the exact range. A target
/// can have moved since the last directory report. Never send a worker replica.
pub(crate) fn sight_page(
    world: &World,
    viewer: Id,
    worker: Worker,
    plan: SightPlan,
) -> Result<SightPage> {
    let state = world.resource::<MeshState>();
    let classify = world
        .get_resource::<SightRule>()
        .ok_or_else(|| invalid("missing sight rule"))?
        .0;
    let mut origin = plan.origin;
    if let Some(&e) = state
        .entities
        .get(&viewer)
        .filter(|&&e| world.get::<Simulated>(e).is_some())
    {
        let (frame, p) = classify(world, e)
            .filter(|(_, p)| p.kind == plan.observer_kind)
            .ok_or(Error::Denied)?;
        origin = SightOrigin { frame, mm: p.mm };
    }
    let owners = world.resource::<Store>().authorities(&plan.objects)?;
    let mut records = Vec::new();
    let mut next = plan.next;
    let mut previous = None;
    // The envelope includes its discriminant, vector length and optional cursor.
    let mut bytes = 32u64;
    for id in plan.objects {
        let Some(entity) = owners
            .get(&id)
            .and_then(|&owner| current_entity(world, id, worker, owner))
        else {
            continue;
        };
        let Some((frame, point)) = classify(world, entity) else {
            continue;
        };
        let rules = world
            .get_resource::<SightRanges>()
            .ok_or_else(|| invalid("missing sight ranges"))?;
        let Some(point) = rules.current_target(world, entity, origin, frame, point) else {
            continue;
        };
        if !origin.includes(&plan.radii, frame, point) {
            continue;
        }
        let record = capture(world, id, View::Public(Detail::Exact))?;
        use bincode::Options;
        let size = bincode::DefaultOptions::new()
            .with_varint_encoding()
            .serialized_size(&record)?;
        if bytes + size > MAX_FRAME as u64 {
            next = Some(previous.ok_or(Error::Busy("single public sight record exceeds frame"))?);
            break;
        }
        bytes += size;
        records.push(record);
        previous = Some(id);
    }
    Ok(SightPage { records, next })
}

impl Pool {
    pub(crate) fn sight_rules(&self, viewer: Id) -> Result<&SightRanges> {
        let owner = self
            .directory
            .presence(viewer)
            .ok_or_else(|| Error::Missing("observer".into()))?
            .owner;
        self.app(owner)?
            .world()
            .get_resource::<SightRanges>()
            .ok_or_else(|| invalid("player sight is not configured"))
    }
    pub(crate) fn local_sight_point(&self, p: &Presence) -> Option<SightPoint> {
        let world = self.app(p.owner).ok()?.world();
        let entity = *world.resource::<MeshState>().entities.get(&p.id)?;
        world.get::<Simulated>(entity)?;
        let (frame, point) = (world.get_resource::<SightRule>()?.0)(world, entity)?;
        (frame == p.bounds.frame).then_some(point)
    }
    /// Complete current public view for one player, including same-worker targets.
    pub fn player_sight(&self, viewer: Id) -> Result<BTreeMap<Id, Record>> {
        let rules = self.sight_rules(viewer)?;
        let mut sources = BTreeMap::<Worker, Vec<Id>>::new();
        let origin = self.directory.visit_visible(
            viewer,
            rules,
            |p| self.local_sight_point(p),
            |id, w| {
                sources.entry(w).or_default().push(id);
            },
        )?;
        let mut records = BTreeMap::new();
        for (w, mut ids) in sources {
            ids.sort_unstable();
            let mut remaining = ids.as_slice();
            while !remaining.is_empty() {
                let take = remaining.len().min(MAX_TARGETS);
                let plan = SightPlan {
                    origin,
                    observer_kind: rules.observer_kind,
                    radii: rules.radii.clone(),
                    objects: remaining[..take].to_vec(),
                    next: (take < remaining.len()).then(|| remaining[take - 1]),
                };
                let page = sight_page(self.app(w)?.world(), viewer, w, plan)?;
                records.extend(page.records.into_iter().map(|r| (r.id, r)));
                let Some(last) = page.next else { break };
                remaining = &remaining[remaining.partition_point(|&id| id <= last)..];
            }
        }
        Ok(records)
    }

    /// Serve many observers of one immutable ECS state. Nearby players can share
    /// a borrowed public record, with one projection and authority check per target
    /// in a batch. The callback also receives an empty slice for isolated players.
    pub fn visit_player_sights(
        &self,
        viewers: &[Id],
        mut visit: impl FnMut(Id, &[&Record]),
    ) -> Result<()> {
        for batch in viewers.chunks(MAX_TARGETS) {
            let mut points = HashMap::new();
            let mut targets = HashMap::new();
            let mut views = Vec::with_capacity(batch.len());
            for &viewer in batch {
                let rules = self.sight_rules(viewer)?;
                let mut ids = Vec::new();
                self.directory.visit_visible(
                    viewer,
                    rules,
                    |p| {
                        *points
                            .entry(p.id)
                            .or_insert_with(|| self.local_sight_point(p))
                    },
                    |id, owner| {
                        ids.push(id);
                        targets.insert(id, owner);
                    },
                )?;
                ids.sort_unstable();
                views.push(ids);
            }
            let ids: Vec<_> = targets.keys().copied().collect();
            let mut records = HashMap::with_capacity(ids.len());
            for group in ids.chunks(MAX_TARGETS) {
                for (id, authority) in self.store.authorities(group)? {
                    let worker = targets[&id];
                    let world = self.app(worker)?.world();
                    if current_entity(world, id, worker, authority).is_some() {
                        records.insert(id, capture(world, id, View::Public(Detail::Exact))?);
                    }
                }
            }
            let mut visible = Vec::new();
            for (&viewer, ids) in batch.iter().zip(views) {
                visible.clear();
                visible.extend(ids.iter().filter_map(|id| records.get(id)));
                visit(viewer, &visible);
            }
        }
        Ok(())
    }
}

/// Keeps authenticated connections to currently visible sources. Each successful
/// refresh replaces the entire view, so disappeared or out-of-range objects leave.
#[derive(Default)]
pub struct PlayerSightClient {
    peers: BTreeMap<Worker, (SocketAddr, wire::Client)>,
    records: BTreeMap<Id, Record>,
    certificate: Option<Digest>,
    pages: BTreeMap<PageKey, CachedPage>,
}
type PageKey = (Worker, Option<Id>);
struct CachedPage {
    digest: Digest,
    objects: Vec<Id>,
}
#[derive(Debug, Default)]
pub struct SightRefresh {
    pub changed_pages: usize,
    pub unchanged_pages: usize,
}
impl PlayerSightClient {
    pub fn records(&self) -> &BTreeMap<Id, Record> {
        &self.records
    }
    pub fn refresh(
        &mut self,
        bootstrap: &mut wire::Client,
        mut connect: impl FnMut(Worker, SocketAddr) -> Result<wire::Client>,
    ) -> Result<SightRefresh> {
        use wire::{Request, Response};
        if self.certificate != Some(bootstrap.certificate) {
            *self = Self {
                certificate: Some(bootstrap.certificate),
                ..Self::default()
            };
        }
        let sources = match bootstrap.call(&Request::SightSources)? {
            Response::SightSources(s) => s,
            Response::Error(e) => return Err(invalid(e)),
            _ => return Err(invalid("expected sight sources")),
        };
        self.peers
            .retain(|w, (addr, _)| sources.get(w) == Some(addr));
        let mut updates = Vec::new();
        let mut stats = SightRefresh::default();
        for (worker, addr) in sources {
            let peer = match self.peers.entry(worker) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    let source = connect(worker, addr)?;
                    if source.certificate != bootstrap.certificate {
                        return Err(Error::Denied);
                    }
                    e.insert((addr, source))
                }
            };
            let mut after = None;
            loop {
                let key = (worker, after);
                let known = self.pages.get(&key).map(|p| p.digest);
                let next = match peer.1.call(&Request::SightPage { after, known }) {
                    Ok(Response::SightPage(p)) => {
                        let next = p.next;
                        updates.push((key, Some(p)));
                        stats.changed_pages += 1;
                        next
                    }
                    Ok(Response::SightUnchanged { next }) if known.is_some() => {
                        updates.push((key, None));
                        stats.unchanged_pages += 1;
                        next
                    }
                    Ok(Response::Error(e)) => return Err(invalid(e)),
                    Ok(_) => return Err(invalid("expected player sight page")),
                    Err(e) => {
                        self.peers.remove(&worker);
                        return Err(e);
                    }
                };
                let Some(next) = next else {
                    break;
                };
                if after.is_some_and(|prev| next <= prev) {
                    return Err(invalid("sight cursor did not advance"));
                }
                after = Some(next);
            }
        }
        if stats.changed_pages == 0
            && updates.len() == self.pages.len()
            && (!self.pages.is_empty() || self.records.is_empty())
        {
            return Ok(stats);
        }
        // Commit only after every source succeeded. Reuse record buffers without cloning.
        // A handoff can briefly return the same id on two pages. Re-read those views
        // on the next refresh since a single displayed record has two possible origins.
        let mut pages = BTreeMap::new();
        let mut records = BTreeMap::new();
        let mut overlap = false;
        for (key, response) in updates {
            let page = if let Some(new) = response {
                let cached = CachedPage {
                    digest: new.content_digest(),
                    objects: new.records.iter().map(|r| r.id).collect(),
                };
                for r in new.records {
                    overlap |= records.insert(r.id, r).is_some();
                }
                cached
            } else {
                let cached = self.pages.remove(&key).expect("validated sight page");
                for id in &cached.objects {
                    let r = self.records.remove(id).expect("displayed sight object");
                    overlap |= records.insert(*id, r).is_some();
                }
                cached
            };
            pages.insert(key, page);
        }
        if overlap {
            pages.clear();
        }
        self.pages = pages;
        self.records = records;
        Ok(stats)
    }
}
