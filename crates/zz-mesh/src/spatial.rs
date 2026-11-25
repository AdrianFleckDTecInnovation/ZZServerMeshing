use crate::octree::{Boundaries, Index};
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::OnceLock;

/// Inclusive integer millimetres in a named local reference frame.
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
        if padding == 0 && (ticks == 0 || velocity == [0; 3]) {
            return self;
        }
        let mut b = self;
        for (i, v) in velocity.iter().enumerate() {
            let d = i128::from(*v) * i128::from(ticks);
            let lo = i128::from(b.lo[i]) + d.min(0) - i128::from(padding);
            let hi = i128::from(b.hi[i]) + d.max(0) + i128::from(padding);
            b.lo[i] = lo.max(i128::from(i64::MIN)) as i64;
            b.hi[i] = hi.min(i128::from(i64::MAX)) as i64;
        }
        b
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
    area_faces: Boundaries,
    object_faces: Boundaries,
    activity_faces: Boundaries,
    islands: OnceLock<Vec<Vec<Id>>>,
    area_objects: BTreeMap<Id, BTreeSet<Id>>,
    object_areas: BTreeMap<Id, BTreeSet<Id>>,
    subs: BTreeMap<Id, BTreeMap<Recipient, Subscription>>,
    clock: u64,
    changes: BTreeMap<(Recipient, Id), InterestChange>,
    area_recipients: BTreeMap<Recipient, usize>,
    pending: Option<BTreeMap<Id, bool>>,
    initial_recipients: usize,
    reserved: usize,
    pub candidates: u64,
    limit: Option<usize>,
}
impl Directory {
    /// A simulation tick or directory report describes one coherent set of positions.
    /// Re-evaluate each affected object once after all its observers have moved.
    pub(crate) fn batch(&mut self, edit: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        debug_assert!(self.pending.is_none());
        self.initial_recipients = self.area_recipients.len();
        self.pending = Some(BTreeMap::new());
        let result = edit(self);
        self.flush_pending();
        self.pending = None;
        result
    }
    fn flush_pending(&mut self) {
        self.reserved = 0;
        let ids = std::mem::take(self.pending.as_mut().expect("active directory batch"));
        for (id, force) in ids {
            self.refresh_now(id, force);
        }
        self.initial_recipients = self.area_recipients.len();
    }
    // A flush can change retained membership. The caller recomputes its operation
    // before applying it when a conservative reservation reaches the queue bound.
    fn reserve(&mut self, cost: usize, affected: usize, new_recipient: bool) -> Result<bool> {
        let limit = self.limit.unwrap_or(1_000_000);
        let mut required = self.reserved.saturating_add(cost);
        if let Some(pending) = &self.pending {
            // An object can lose each original recipient and gain each final recipient
            // at most once. Many overlapping areas of one worker share a single route.
            let recipients =
                self.initial_recipients + self.area_recipients.len() + usize::from(new_recipient);
            let route_bound = pending
                .len()
                .saturating_add(affected)
                .saturating_mul(recipients);
            required = required.min(route_bound);
        }
        if self.changes.len().saturating_add(required) > limit {
            if self.reserved > 0 {
                self.flush_pending();
                return Ok(true);
            }
            return Err(Error::Busy("drain pending interest changes"));
        }
        if self.pending.is_some() {
            self.reserved = required;
        }
        Ok(false)
    }
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
        if !demand_changed
            && self.areas.get(&id).is_some_and(|old| {
                self.object_faces.stable(old.needed(), needed)
                    && self.object_faces.stable(old.retained(), retained)
            })
        {
            self.index_area(a);
            return Ok(());
        }
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
        if self.reserve(
            touched.len().saturating_mul(2),
            touched.len(),
            !self.area_recipients.contains_key(&a.to),
        )? {
            return self.register_area(a);
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
        self.index_area(a);
        for object in touched {
            self.refresh(object);
        }
        Ok(())
    }
    fn index_area(&mut self, area: Area) {
        let id = area.id;
        let to = area.to;
        let needed = area.needed();
        let retained = area.retained();
        let physical = area.activity.then_some(area.bounds);
        let old = self.areas.insert(id, area);
        self.update_activity(
            old.as_ref().filter(|a| a.activity).map(|a| a.bounds),
            physical,
        );
        if old.as_ref().map(|a| a.to) != Some(to) {
            if let Some(previous) = &old {
                self.remove_recipient_area(previous.to);
            }
            *self.area_recipients.entry(to).or_default() += 1;
        }
        let old_retained = old.as_ref().map(Area::retained);
        self.area_faces
            .replace(old.as_ref().map(Area::needed), Some(needed));
        self.area_faces.replace(old_retained, Some(retained));
        self.area_index.replace(id, old_retained, retained);
    }
    fn update_activity(&mut self, old: Option<Box3>, new: Option<Box3>) {
        if old == new {
            return;
        }
        if self.islands.get().is_some()
            && !old
                .zip(new)
                .is_some_and(|(a, b)| self.activity_faces.stable(a, b))
        {
            self.islands.take();
        }
        self.activity_faces.replace(old, new);
    }
    fn remove_recipient_area(&mut self, to: Recipient) {
        let count = self
            .area_recipients
            .get_mut(&to)
            .expect("indexed recipient");
        *count -= 1;
        if *count == 0 {
            self.area_recipients.remove(&to);
        }
    }
    pub fn remove_area(&mut self, id: Id) {
        if let Some(a) = self.areas.remove(&id) {
            self.update_activity(a.activity.then_some(a.bounds), None);
            self.remove_recipient_area(a.to);
            self.area_faces.replace(Some(a.needed()), None);
            self.area_faces.replace(Some(a.retained()), None);
            self.area_index.remove(id, a.retained());
        }
        for object in self.area_objects.remove(&id).unwrap_or_default() {
            if let Some(areas) = self.object_areas.get_mut(&object) {
                areas.remove(&id);
            }
            self.refresh(object);
        }
    }
    pub fn upsert(&mut self, p: Presence) -> Result<()> {
        Box3::new(p.bounds.frame, p.bounds.lo, p.bounds.hi)?;
        if self.objects.get(&p.id) == Some(&p) {
            return Ok(());
        }
        let predicted = p.predicted();
        let previous = self.objects.get(&p.id).copied();
        let identity_changed =
            previous.is_none_or(|old| old.owner != p.owner || old.principal != p.principal);
        if !identity_changed
            && previous.is_some_and(|old| self.area_faces.stable(old.predicted(), predicted))
        {
            self.index_object(previous, p);
            return Ok(());
        }
        let candidates = self.area_index.query(predicted);
        if self.reserve(
            candidates
                .len()
                .saturating_add(self.object_areas.get(&p.id).map_or(0, BTreeSet::len)),
            1,
            false,
        )? {
            return self.upsert(p);
        }
        self.index_object(previous, p);
        if previous.is_some() && identity_changed {
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
        if let Some(pending) = &mut self.pending {
            *pending.entry(p.id).or_default() |= identity_changed;
        } else {
            self.refresh_candidates(p.id, candidates, identity_changed);
        }
        Ok(())
    }
    fn index_object(&mut self, previous: Option<Presence>, p: Presence) {
        let old = previous.map(Presence::predicted);
        let new = p.predicted();
        if old != Some(new) {
            self.object_faces.replace(old, Some(new));
            self.object_index.replace(p.id, old, new);
        }
        self.objects.insert(p.id, p);
    }
    pub fn remove(&mut self, id: Id) {
        if let Some(p) = self.objects.remove(&id) {
            self.object_faces.replace(Some(p.predicted()), None);
            self.object_index.remove(id, p.predicted());
        }
        self.refresh(id);
    }
    fn refresh(&mut self, id: Id) {
        if let Some(pending) = &mut self.pending {
            pending.insert(id, true);
        } else {
            self.refresh_now(id, true);
        }
    }
    fn refresh_now(&mut self, id: Id, force: bool) {
        let candidates = self
            .objects
            .get(&id)
            .map(|p| self.area_index.query(p.predicted()))
            .unwrap_or_default();
        self.refresh_candidates(id, candidates, force);
    }
    pub(crate) fn remove_owned(&mut self, worker: Worker, id: Id) {
        if self.presence(id).is_some_and(|p| p.owner == worker) {
            self.remove_area(id);
            self.remove(id);
        }
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
        self.island_groups().to_vec()
    }
    /// Borrow physical connected components until the next edit to the directory.
    pub fn island_groups(&self) -> &[Vec<Id>] {
        self.islands.get_or_init(|| self.build_islands())
    }
    fn build_islands(&self) -> Vec<Vec<Id>> {
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
        let mut candidate = BTreeMap::new();
        if u.base.is_some() {
            // Keep only untouched fields from the acknowledged base. Updates replace
            // their old bytes, and omissions in a full frame clear the previous view.
            for (&id, value) in fields.iter() {
                if !u.fields.contains_key(&id) {
                    candidate.insert(id, value.clone());
                }
            }
        }
        for (&id, value) in &u.fields {
            if let Some(value) = value {
                candidate.insert(id, value.clone());
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

#[cfg(test)]
mod batch_tests {
    use super::*;
    fn pair(i: u32, x: i64) -> (Presence, Area) {
        let owner = Worker {
            slot: i % 4 + 1,
            incarnation: 1,
        };
        let id = Id::numbered(i.into());
        let bounds = Box3::new(Id::numbered(99), [x - 2, -2, -2], [x + 2, 2, 2]).unwrap();
        (
            Presence {
                id,
                owner,
                principal: id,
                bounds,
                velocity: [0; 3],
                ahead: 0,
            },
            Area {
                id,
                worker: owner,
                to: Recipient::Worker(owner),
                bounds,
                velocity: [0; 3],
                ahead: 0,
                hysteresis: 1,
                activity: true,
                components: BTreeMap::from([(
                    1,
                    Demand {
                        detail: Detail::Exact,
                        period: 1,
                    },
                )]),
            },
        )
    }
    #[test]
    fn dense_edits_reaggregate_each_object_once_and_keep_stable_epochs() {
        // Area/object overlap pairs exceed 600 while all distinct routes fit comfortably.
        let mut d = Directory::bounded(600);
        for (together, x) in [(false, 0), (true, 0), (true, 10000), (false, 10000)] {
            let before = d.candidates;
            d.batch(|d| {
                for i in 0..32 {
                    let (p, a) = pair(i, x + if together { 0 } else { 100 * i64::from(i) });
                    d.upsert(p)?;
                    d.register_area(a)?;
                }
                Ok(())
            })
            .unwrap();
            assert!(d.candidates - before <= 32 * 32);
            assert_eq!(d.islands().len(), if together { 1 } else { 32 });
            for i in 0..32 {
                assert_eq!(
                    d.subscribers(Id::numbered(i)).count(),
                    if together { 3 } else { 0 }
                );
            }
            let changes = d.changes();
            if together && x != 0 {
                assert!(changes.is_empty(), "common translation changed routes");
            }
        }
    }
    #[test]
    fn partial_report_at_quota_keeps_accepted_memberships_and_can_retry() {
        let mut d = Directory::bounded(3);
        let (p, _) = pair(0, 0);
        d.upsert(p).unwrap();
        let result = d.batch(|d| {
            for i in 1..4 {
                d.register_area(pair(i, 0).1)?;
            }
            d.register_area(pair(7, 2).1)
        });
        assert!(matches!(result, Err(Error::Busy(_))));
        assert!(!d.changes().is_empty());
        for i in 1..4 {
            d.register_area(pair(i, 0).1).unwrap();
            d.changes();
        }
        assert_eq!(d.subscribers(p.id).count(), 3);
    }
}
