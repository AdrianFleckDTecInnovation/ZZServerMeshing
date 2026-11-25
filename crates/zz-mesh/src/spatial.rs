use crate::*;
use rstar::{RTree, RTreeObject, AABB};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Millimetres in a named local reference frame. The broad phase rounds outwards;
/// final intersection tests use integers, including well beyond f64's exact range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Box3 {
    pub frame: Id,
    pub lo: [i64; 3],
    pub hi: [i64; 3],
}
impl Box3 {
    pub fn new(frame: Id, lo: [i64; 3], hi: [i64; 3]) -> Result<Self> {
        if (0..3).any(|i| lo[i] > hi[i]) {
            return Err(invalid("inverted spatial bounds"));
        }
        Ok(Self { frame, lo, hi })
    }
    pub fn intersects(self, b: Self) -> bool {
        self.frame == b.frame && (0..3).all(|i| self.lo[i] <= b.hi[i] && b.lo[i] <= self.hi[i])
    }
    pub fn sweep(self, velocity: [i64; 3], ticks: u16, padding: u32) -> Self {
        let mut b = self;
        for (i, v) in velocity.iter().enumerate() {
            let d = v.saturating_mul(i64::from(ticks));
            b.lo[i] = b.lo[i]
                .saturating_add(d.min(0))
                .saturating_sub(i64::from(padding));
            b.hi[i] = b.hi[i]
                .saturating_add(d.max(0))
                .saturating_add(i64::from(padding));
        }
        b
    }
    fn envelope(self) -> AABB<[f64; 3]> {
        AABB::from_corners(
            self.lo.map(|v| (v as f64).next_down()),
            self.hi.map(|v| (v as f64).next_up()),
        )
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Entry(Id, Box3);
impl RTreeObject for Entry {
    type Envelope = AABB<[f64; 3]>;
    fn envelope(&self) -> Self::Envelope {
        self.1.envelope()
    }
}
#[derive(Default)]
struct Index(BTreeMap<Id, RTree<Entry>>);
impl Index {
    fn insert(&mut self, id: Id, b: Box3) {
        self.0.entry(b.frame).or_default().insert(Entry(id, b));
    }
    fn remove(&mut self, id: Id, b: Box3) {
        if let Some(tree) = self.0.get_mut(&b.frame) {
            tree.remove(&Entry(id, b));
            if tree.size() == 0 {
                self.0.remove(&b.frame);
            }
        }
    }
    fn query(&self, b: Box3) -> Vec<Id> {
        self.0
            .get(&b.frame)
            .into_iter()
            .flat_map(|tree| tree.locate_in_envelope_intersecting(b.envelope()))
            .filter(|e| e.1.intersects(b))
            .map(|e| e.0)
            .collect()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Recipient {
    Worker(Worker),
    Client(Id),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Demand {
    pub detail: Detail,
    pub period: u16,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub id: Id,
    pub worker: Worker,
    pub to: Recipient,
    pub bounds: Box3,
    pub velocity: [i64; 3],
    pub ahead: u16,
    pub hysteresis: u32,
    pub activity: bool,
    pub components: BTreeMap<u32, Demand>,
}
impl Area {
    fn needed(&self) -> Box3 {
        self.bounds.sweep(self.velocity, self.ahead, 0)
    }
    fn retained(&self) -> Box3 {
        self.bounds
            .sweep(self.velocity, self.ahead, self.hysteresis)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    pub id: Id,
    pub owner: Worker,
    pub principal: Id,
    pub bounds: Box3,
    pub velocity: [i64; 3],
    pub ahead: u16,
}
impl Presence {
    fn predicted(self) -> Box3 {
        self.bounds.sweep(self.velocity, self.ahead, 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub epoch: u64,
    pub components: BTreeMap<u32, Demand>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InterestChange {
    pub object: Id,
    pub to: Recipient,
    pub epoch: u64,
    pub subscription: Option<Subscription>,
}
#[derive(Default)]
pub struct Directory {
    areas: BTreeMap<Id, Area>,
    objects: BTreeMap<Id, Presence>,
    area_index: Index,
    object_index: Index,
    area_objects: BTreeMap<Id, BTreeSet<Id>>,
    object_areas: BTreeMap<Id, BTreeSet<Id>>,
    subs: BTreeMap<Id, BTreeMap<Recipient, Subscription>>,
    clock: u64,
    changes: BTreeMap<(Recipient, Id), InterestChange>,
    pub candidates: u64,
    limit: Option<usize>,
}
impl Directory {
    pub fn for_incarnation(inc: u64) -> Result<Self> {
        if inc >= 1 << 24 {
            return Err(invalid("directory incarnation limit"));
        }
        Ok(Self {
            clock: inc << 40,
            ..Default::default()
        })
    }
    pub fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Default::default()
        }
    }
    pub fn watermark(&self) -> u64 {
        self.clock
    }
    pub fn register_area(&mut self, a: Area) -> Result<()> {
        Box3::new(a.bounds.frame, a.bounds.lo, a.bounds.hi)?;
        if a.components.len() > 128 || a.components.values().any(|d| d.period == 0) {
            return Err(invalid("invalid component demands"));
        }
        if self.areas.len() >= 65536 && !self.areas.contains_key(&a.id) {
            return Err(Error::Busy("directory areas"));
        }
        let id = a.id;
        if self.areas.get(&id) == Some(&a) {
            return Ok(());
        }
        let needed = a.needed();
        let retained = a.retained();
        let previous = self.area_objects.get(&id);
        let demand_changed = self
            .areas
            .get(&id)
            .is_some_and(|old| old.to != a.to || old.components != a.components);
        // Moving an area inside a dense crowd usually leaves all memberships intact.
        // Only changed memberships or demands require re-aggregating an object's peers.
        let mut touched = BTreeSet::new();
        for &object in previous.into_iter().flatten() {
            let p = &self.objects[&object];
            if demand_changed
                || a.to == Recipient::Worker(p.owner)
                || !retained.intersects(p.predicted())
            {
                touched.insert(object);
            }
        }
        for object in self.object_index.query(needed) {
            if !previous.is_some_and(|members| members.contains(&object))
                && a.to != Recipient::Worker(self.objects[&object].owner)
            {
                touched.insert(object);
            }
        }
        if self
            .changes
            .len()
            .saturating_add(touched.len().saturating_mul(2))
            > self.limit.unwrap_or(1_000_000)
        {
            return Err(Error::Busy("drain pending interest changes"));
        }
        // A newly assigned recipient must first enter the required range itself.
        // Retention belongs to the old route and cannot confer a new subscription.
        if self.areas.get(&id).is_some_and(|old| old.to != a.to) {
            for object in self.area_objects.remove(&id).unwrap_or_default() {
                if let Some(areas) = self.object_areas.get_mut(&object) {
                    areas.remove(&id);
                }
            }
        }
        if let Some(old) = self.areas.get(&id) {
            self.area_index.remove(id, old.retained());
        }
        self.area_index.insert(id, retained);
        self.areas.insert(id, a);
        for object in touched {
            self.refresh(object);
        }
        Ok(())
    }
    pub fn remove_area(&mut self, id: Id) {
        if let Some(a) = self.areas.remove(&id) {
            self.area_index.remove(id, a.retained());
        }
        for object in self.area_objects.remove(&id).unwrap_or_default() {
            self.refresh(object);
        }
    }
    pub fn upsert(&mut self, p: Presence) -> Result<()> {
        Box3::new(p.bounds.frame, p.bounds.lo, p.bounds.hi)?;
        if self.objects.get(&p.id) == Some(&p) {
            return Ok(());
        }
        let predicted = p.predicted();
        let candidates = self.area_index.query(predicted);
        if self
            .changes
            .len()
            .saturating_add(candidates.len())
            .saturating_add(self.object_areas.get(&p.id).map_or(0, BTreeSet::len))
            > self.limit.unwrap_or(1_000_000)
        {
            return Err(Error::Busy("drain pending interest changes"));
        }
        let previous = self.objects.insert(p.id, p);
        let identity_changed =
            previous.is_none_or(|old| old.owner != p.owner || old.principal != p.principal);
        if let Some(old) = previous {
            self.object_index.remove(p.id, old.predicted());
            if identity_changed {
                if let Some(routes) = self.subs.remove(&p.id) {
                    for (to, _) in routes {
                        self.clock += 1;
                        self.changes.insert(
                            (to, p.id),
                            InterestChange {
                                object: p.id,
                                to,
                                epoch: self.clock,
                                subscription: None,
                            },
                        );
                    }
                }
            }
        }
        self.object_index.insert(p.id, predicted);
        self.refresh_candidates(p.id, candidates, identity_changed);
        Ok(())
    }
    pub fn remove(&mut self, id: Id) {
        if let Some(p) = self.objects.remove(&id) {
            self.object_index.remove(id, p.predicted());
        }
        self.refresh(id);
    }
    fn refresh(&mut self, id: Id) {
        let candidates = self
            .objects
            .get(&id)
            .map(|p| self.area_index.query(p.predicted()))
            .unwrap_or_default();
        self.refresh_candidates(id, candidates, true);
    }
    fn refresh_candidates(&mut self, id: Id, mut candidates: Vec<Id>, force: bool) {
        self.candidates += candidates.len() as u64;
        let previous = self.object_areas.get(&id);
        let mut retained = 0;
        if let Some(p) = self.objects.get(&id) {
            let predicted = p.predicted();
            // The index uses retained bounds. New observers must enter the required bounds.
            candidates.retain(|id| {
                let area = &self.areas[id];
                if area.to == Recipient::Worker(p.owner) {
                    return false;
                }
                if previous.is_some_and(|members| members.contains(id)) {
                    retained += 1;
                    return true;
                }
                area.needed().intersects(predicted)
            });
        }
        // Moving inside the same set of interest areas preserves the aggregated routes.
        // Demand, recipient and authority changes always force a fresh aggregation.
        if !force && retained == candidates.len() && retained == previous.map_or(0, BTreeSet::len) {
            return;
        }
        let old = self.object_areas.remove(&id).unwrap_or_default();
        let active_areas: BTreeSet<_> = candidates.into_iter().collect();
        let mut wanted: BTreeMap<Recipient, BTreeMap<u32, Demand>> = BTreeMap::new();
        for a in &active_areas {
            let area = &self.areas[a];
            let fields = wanted.entry(area.to).or_default();
            for (&s, &d) in &area.components {
                fields
                    .entry(s)
                    .and_modify(|r| {
                        if d.detail == Detail::Exact {
                            r.detail = Detail::Exact
                        }
                        r.period = r.period.min(d.period);
                    })
                    .or_insert(d);
            }
        }
        for a in old.difference(&active_areas) {
            if let Some(members) = self.area_objects.get_mut(a) {
                members.remove(&id);
            }
        }
        for &a in active_areas.difference(&old) {
            self.area_objects.entry(a).or_default().insert(id);
        }
        if !active_areas.is_empty() {
            self.object_areas.insert(id, active_areas);
        }
        let mut previous = self.subs.remove(&id).unwrap_or_default();
        let mut active = BTreeMap::new();
        for (to, fields) in wanted {
            if let Some(s) = previous.remove(&to) {
                if s.components == fields {
                    active.insert(to, s);
                    continue;
                }
            }
            self.clock += 1;
            let s = Subscription {
                epoch: self.clock,
                components: fields,
            };
            active.insert(to, s.clone());
            self.changes.insert(
                (to, id),
                InterestChange {
                    object: id,
                    to,
                    epoch: self.clock,
                    subscription: Some(s),
                },
            );
        }
        for (to, _) in previous {
            self.clock += 1;
            self.changes.insert(
                (to, id),
                InterestChange {
                    object: id,
                    to,
                    epoch: self.clock,
                    subscription: None,
                },
            );
        }
        if !active.is_empty() {
            self.subs.insert(id, active);
        }
    }
    pub(crate) fn edges(&self, id: Id) -> Option<&BTreeMap<Recipient, Subscription>> {
        self.subs.get(&id)
    }
    pub fn subscribers(&self, id: Id) -> impl Iterator<Item = (&Recipient, &Subscription)> {
        self.subs.get(&id).into_iter().flatten()
    }
    pub fn presence(&self, id: Id) -> Option<Presence> {
        self.objects.get(&id).copied()
    }
    pub fn changes(&mut self) -> Vec<InterestChange> {
        std::mem::take(&mut self.changes).into_values().collect()
    }
    /// Connected components of physical activity only. A shared worker is not an edge.
    pub fn islands(&self) -> Vec<Vec<Id>> {
        // A dense island can have quadratically many overlapping area pairs.
        // Only placement and diagnostics ask for its connected components. Visit
        // each discovered area once, removing it from the remaining spatial index.
        let mut remaining = Index::default();
        let mut left = BTreeSet::new();
        for a in self.areas.values().filter(|a| a.activity) {
            left.insert(a.id);
            remaining.insert(a.id, a.bounds);
        }
        let mut groups = vec![];
        while let Some(start) = left.pop_first() {
            remaining.remove(start, self.areas[&start].bounds);
            let mut q = VecDeque::from([start]);
            let mut group = vec![];
            while let Some(id) = q.pop_front() {
                group.push(id);
                let mut adjacent = remaining.query(self.areas[&id].bounds);
                adjacent.sort_unstable();
                for n in adjacent {
                    left.remove(&n);
                    remaining.remove(n, self.areas[&n].bounds);
                    q.push_back(n);
                }
            }
            groups.push(group);
        }
        groups
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub object: Id,
    pub epoch: u64,
    pub generation: u64,
    pub revision: u64,
    pub serial: u64,
    pub base: Option<u64>,
    pub fields: BTreeMap<u32, Option<Field>>,
    pub deleted: bool,
}
/// Per-recipient state. A subscription end unloads only its view. Reliable route
/// changes carry monotonically increasing epochs; data arriving before a route is retried.
#[derive(Default)]
pub struct Views {
    active: BTreeMap<Id, (u64, u64, u64, BTreeMap<u32, Field>)>,
    closed: BTreeMap<Id, u64>,
    floor: u64,
}
impl Views {
    pub fn route(&mut self, c: &InterestChange) {
        if c.epoch <= self.floor
            || self.closed.get(&c.object).copied().unwrap_or(0) >= c.epoch
            || self.active.get(&c.object).is_some_and(|v| v.0 >= c.epoch)
        {
            return;
        }
        if c.subscription.is_some() {
            self.closed.remove(&c.object);
            self.active
                .insert(c.object, (c.epoch, 0, 0, BTreeMap::new()));
        } else {
            self.active.remove(&c.object);
            self.closed.insert(c.object, c.epoch);
        }
    }
    /// The control channel has applied every route up to this watermark.
    pub fn watermark(&mut self, x: u64) {
        self.floor = self.floor.max(x);
        self.closed.retain(|_, e| *e > self.floor);
    }
    pub fn apply(&mut self, u: &Update) -> bool {
        self.apply_checked(u, |_| Ok(())).unwrap_or(false)
    }
    pub(crate) fn apply_checked(
        &mut self,
        u: &Update,
        install: impl FnOnce(Option<&BTreeMap<u32, Field>>) -> Result<()>,
    ) -> Result<bool> {
        let Some((epoch, generation, serial, fields)) = self.active.get_mut(&u.object) else {
            return Ok(false);
        };
        if *epoch != u.epoch
            || (*generation, *serial) >= (u.generation, u.serial)
            || u.base
                .is_some_and(|b| *generation != u.generation || *serial != b)
        {
            return Ok(false);
        }
        if u.deleted {
            install(None)?;
            self.closed.insert(u.object, *epoch);
            self.active.remove(&u.object);
            return Ok(true);
        }
        let mut candidate = if u.base.is_none() {
            BTreeMap::new()
        } else {
            fields.clone()
        };
        for (&id, value) in &u.fields {
            if let Some(value) = value {
                candidate.insert(id, value.clone());
            } else {
                candidate.remove(&id);
            }
        }
        install(Some(&candidate))?;
        *fields = candidate;
        *generation = u.generation;
        *serial = u.serial;
        Ok(true)
    }
    pub fn len(&self) -> usize {
        self.active.len()
    }
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
    pub fn objects(&self) -> Vec<Id> {
        self.active.keys().copied().collect()
    }
    pub fn interested(&self, id: Id, epoch: u64) -> bool {
        self.active.get(&id).is_some_and(|v| v.0 == epoch)
    }
    pub fn already_applied(&self, u: &Update) -> bool {
        self.active
            .get(&u.object)
            .is_some_and(|v| (v.0, v.1, v.2) == (u.epoch, u.generation, u.serial))
    }
    pub fn fields(&self, id: Id) -> Option<&BTreeMap<u32, Field>> {
        self.active.get(&id).map(|v| &v.3)
    }
}
