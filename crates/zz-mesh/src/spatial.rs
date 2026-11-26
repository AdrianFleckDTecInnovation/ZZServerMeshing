use crate::octree::{Boundaries, Index};
use crate::regions::MovingIndex;
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

/// Inclusive integer millimetres in a named local reference frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Box3 {
    pub frame: Id,
    pub lo: [i64; 3],
    pub hi: [i64; 3],
}
impl Box3 {
    fn padded(mut self, padding: u32) -> Self {
        let d = i64::from(padding);
        self.lo = self.lo.map(|x| x.saturating_sub(d));
        self.hi = self.hi.map(|x| x.saturating_add(d));
        self
    }
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
        if ticks == 0 || velocity == [0; 3] {
            return if padding == 0 {
                self
            } else {
                self.padded(padding)
            };
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    areas: HashMap<Id, Area>,
    objects: HashMap<Id, Presence>,
    area_index: MovingIndex,
    object_index: MovingIndex,
    area_faces: Boundaries,
    object_faces: Boundaries,
    activity_faces: Boundaries,
    islands: OnceLock<Vec<Vec<Id>>>,
    area_objects: HashMap<Id, HashSet<Id>>,
    object_areas: HashMap<Id, HashSet<Id>>,
    subs: BTreeMap<Id, BTreeMap<Recipient, Subscription>>,
    clock: u64,
    changes: BTreeMap<(Recipient, Id), InterestChange>,
    area_recipients: BTreeMap<Recipient, usize>,
    pending: Option<HashMap<Id, bool>>,
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
        self.pending = Some(HashMap::new());
        let result = edit(self);
        self.flush_pending();
        self.pending = None;
        result
    }
    fn flush_pending(&mut self) {
        self.reserved = 0;
        let mut ids: Vec<_> = self
            .pending
            .as_mut()
            .expect("active directory batch")
            .drain()
            .collect();
        ids.sort_unstable_by_key(|(id, _)| *id);
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
        let old = self.areas.get(&id);
        if old == Some(&a) {
            return Ok(());
        }
        let needed = a.needed();
        let retained = needed.padded(a.hysteresis);
        let old_bounds = old.map(|area| {
            let needed = area.needed();
            (needed, needed.padded(area.hysteresis))
        });
        let previous = self.area_objects.get(&id);
        let demand_changed =
            old.is_some_and(|old| old.to != a.to || old.components != a.components);
        if !demand_changed
            && (old_bounds.is_some_and(|(required, kept)| {
                self.object_faces.stable(required, needed)
                    && self.object_faces.stable(kept, retained)
            }) || (previous.is_none_or(HashSet::is_empty)
                && self.object_index.excludes_others(needed, a.to)))
        {
            self.index_area(a, needed, retained, old_bounds);
            return Ok(());
        }
        let candidates = self.object_index.candidates(needed, a.to);
        // A jump into a disjoint volume can change most edges. In a batch the
        // final exact test already runs per object, once all observers have moved.
        // Only use this conservative union if its whole reservation fits.
        if self.pending.is_some()
            && !demand_changed
            && old_bounds.is_some_and(|(_, old)| !old.intersects(retained))
        {
            let count = previous.map_or(0, HashSet::len) + candidates.len();
            let cost = 2 * count;
            let available = self
                .limit
                .unwrap_or(1_000_000)
                .saturating_sub(self.changes.len() + self.reserved);
            if cost <= available {
                self.reserve(cost, count, false)?;
                let pending = self.pending.as_mut().expect("active directory batch");
                for object in self
                    .area_objects
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .copied()
                    .chain(candidates)
                {
                    pending.entry(object).or_default();
                }
                self.index_area(a, needed, retained, old_bounds);
                return Ok(());
            }
        }
        // Smaller movements usually preserve membership. Old members and newly
        // intersecting non-members are disjoint, and each candidate occurs once.
        let mut touched = Vec::new();
        for &object in previous.into_iter().flatten() {
            let p = &self.objects[&object];
            if demand_changed
                || a.to == Recipient::Worker(p.owner)
                || !retained.intersects(p.predicted())
            {
                touched.push(object);
            }
        }
        for object in candidates {
            if previous.is_some_and(|members| members.contains(&object)) {
                continue;
            }
            let p = self.objects[&object];
            if needed.intersects(p.predicted()) {
                touched.push(object);
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
        self.index_area(a, needed, retained, old_bounds);
        if self.pending.is_none() {
            touched.sort_unstable();
        }
        for object in touched {
            self.refresh(object);
        }
        Ok(())
    }
    fn index_area(
        &mut self,
        area: Area,
        needed: Box3,
        retained: Box3,
        old_bounds: Option<(Box3, Box3)>,
    ) {
        let id = area.id;
        let to = area.to;
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
        let old_retained = old_bounds.map(|(_, kept)| kept);
        self.area_faces
            .replace(old_bounds.map(|(needed, _)| needed), Some(needed));
        self.area_faces.replace(old_retained, Some(retained));
        self.area_index.replace(
            id,
            old_retained.zip(old.as_ref().map(|a| a.to)),
            retained,
            to,
        );
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
            self.area_index.remove(id, a.retained(), a.to);
        }
        let mut objects: Vec<_> = self
            .area_objects
            .remove(&id)
            .into_iter()
            .flatten()
            .collect();
        // An unbatched edit assigns route epochs in object order.
        if self.pending.is_none() {
            objects.sort_unstable();
        }
        for object in objects {
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
        let old_bounds = previous.map(Presence::predicted);
        let identity_changed =
            previous.is_none_or(|old| old.owner != p.owner || old.principal != p.principal);
        if !identity_changed
            && (old_bounds.is_some_and(|old| self.area_faces.stable(old, predicted))
                || (self.object_areas.get(&p.id).is_none_or(HashSet::is_empty)
                    && self
                        .area_index
                        .excludes_others(predicted, Recipient::Worker(p.owner))))
        {
            self.index_object(old_bounds, p, predicted);
            return Ok(());
        }
        // During a batch, final membership is queried after all areas have moved.
        // Each old recipient can lose one route and each current one can gain one.
        let upper = self.subs.get(&p.id).map_or(0, BTreeMap::len) + self.area_recipients.len();
        let available = self
            .limit
            .unwrap_or(1_000_000)
            .saturating_sub(self.changes.len() + self.reserved);
        let candidates = if self.pending.is_some() && upper <= available {
            if self.reserve(upper, 1, false)? {
                return self.upsert(p);
            }
            Vec::new()
        } else {
            let mut hits = self
                .area_index
                .candidates(predicted, Recipient::Worker(p.owner));
            hits.retain(|id| self.areas[id].retained().intersects(predicted));
            let cost = hits.len() + self.object_areas.get(&p.id).map_or(0, HashSet::len);
            if self.reserve(cost, 1, false)? {
                return self.upsert(p);
            }
            hits
        };
        self.index_object(old_bounds, p, predicted);
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
    fn index_object(&mut self, old: Option<Box3>, p: Presence, new: Box3) {
        if old != Some(new) {
            self.object_faces.replace(old, Some(new));
        }
        let previous = self.objects.insert(p.id, p);
        self.object_index.replace(
            p.id,
            old.zip(previous.map(|p| Recipient::Worker(p.owner))),
            new,
            Recipient::Worker(p.owner),
        );
    }
    pub fn remove(&mut self, id: Id) {
        if let Some(p) = self.objects.remove(&id) {
            self.object_faces.replace(Some(p.predicted()), None);
            self.object_index
                .remove(id, p.predicted(), Recipient::Worker(p.owner));
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
            .map(|p| {
                self.area_index
                    .candidates(p.predicted(), Recipient::Worker(p.owner))
            })
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
                if previous.is_some_and(|members| members.contains(id)) {
                    let keep = area.retained().intersects(predicted);
                    retained += usize::from(keep);
                    return keep;
                }
                area.needed().intersects(predicted)
            });
        }
        // Moving inside the same set of interest areas preserves the aggregated routes.
        // Demand, recipient and authority changes always force a fresh aggregation.
        if !force && retained == candidates.len() && retained == previous.map_or(0, HashSet::len) {
            return;
        }
        let mut departing = self.object_areas.remove(&id).unwrap_or_default();
        let mut wanted: BTreeMap<Recipient, BTreeMap<u32, Demand>> = BTreeMap::new();
        for a in &candidates {
            if !departing.remove(a) {
                self.area_objects.entry(*a).or_default().insert(id);
            }
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
        for a in departing {
            if let Some(members) = self.area_objects.get_mut(&a) {
                members.remove(&id);
            }
        }
        if !candidates.is_empty() {
            self.object_areas
                .insert(id, candidates.into_iter().collect());
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
        let mut left: BTreeSet<_> = self
            .areas
            .values()
            .filter(|a| a.activity)
            .map(|a| a.id)
            .collect();
        for &id in &left {
            remaining.insert(id, self.areas[&id].bounds);
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    Installed,
    Duplicate,
    Rejected,
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
        matches!(
            self.apply_checked(std::borrow::Cow::Borrowed(u), |_, _| Ok(())),
            Ok(ApplyOutcome::Installed)
        )
    }
    pub(crate) fn apply_checked(
        &mut self,
        u: std::borrow::Cow<'_, Update>,
        install: impl FnOnce(&Update, &BTreeMap<u32, Field>) -> Result<()>,
    ) -> Result<ApplyOutcome> {
        let Some((epoch, generation, serial, fields)) = self.active.get_mut(&u.object) else {
            return Ok(ApplyOutcome::Rejected);
        };
        if (*epoch, *generation, *serial) == (u.epoch, u.generation, u.serial) {
            return Ok(ApplyOutcome::Duplicate);
        }
        if *epoch != u.epoch
            || (*generation, *serial) >= (u.generation, u.serial)
            || u.base
                .is_some_and(|b| *generation != u.generation || *serial != b)
        {
            return Ok(ApplyOutcome::Rejected);
        }
        // Commit the receipt and mutate its baseline only after codec installation
        // succeeds. An invalid delta can be retried against exactly the old base.
        install(&u, fields)?;
        if u.deleted {
            self.closed.insert(u.object, *epoch);
            self.active.remove(&u.object);
            return Ok(ApplyOutcome::Installed);
        }
        if u.base.is_none() {
            fields.retain(|id, _| u.fields.contains_key(id));
        }
        *generation = u.generation;
        *serial = u.serial;
        let mut patch = |id, value| {
            if let Some(field) = value {
                fields.insert(id, field);
            } else {
                fields.remove(&id);
            }
        };
        match u {
            std::borrow::Cow::Borrowed(u) => {
                for (&id, value) in &u.fields {
                    patch(id, value.clone());
                }
            }
            std::borrow::Cow::Owned(u) => {
                for (id, value) in u.fields {
                    patch(id, value);
                }
            }
        }
        Ok(ApplyOutcome::Installed)
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
    #[test]
    fn disjoint_group_jump_keeps_only_existing_members_inside_the_buffer() {
        let mut d = Directory::default();
        let kept = pair(1, 0).0;
        let left = pair(2, 0).0;
        let newcomer = pair(3, 100_005).0;
        d.register_area(pair(0, 0).1).unwrap();
        for p in [kept, left, newcomer] {
            d.upsert(p).unwrap();
        }
        let epoch = d.subscribers(kept.id).next().unwrap().1.epoch;
        d.changes();
        d.batch(|d| {
            d.register_area(pair(0, 100_000).1)?;
            d.upsert(pair(1, 100_005).0)
        })
        .unwrap();
        assert_eq!(d.subscribers(kept.id).next().unwrap().1.epoch, epoch);
        assert_eq!(d.subscribers(newcomer.id).count(), 0);
        assert_eq!(d.subscribers(left.id).count(), 0);
        let changes = d.changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].object, left.id);
        assert!(changes[0].subscription.is_none());
    }

    #[test]
    fn loose_covers_respect_exact_contact_retention_and_frame_switches() {
        let mut d = Directory::default();
        let (mut p, _) = pair(1, 0);
        let (_, mut a) = pair(0, 0);
        let frame = a.bounds.frame;
        a.bounds = Box3::new(frame, [-9000; 3], [-4000; 3]).unwrap();
        a.hysteresis = 1;
        d.register_area(a).unwrap();
        for (x, subscribed) in [(-3999, false), (-4000, true), (-3999, true), (-3998, false)] {
            p.bounds = Box3::new(frame, [x; 3], [x + 5000; 3]).unwrap();
            d.upsert(p).unwrap();
            assert_eq!(d.subscribers(p.id).count(), usize::from(subscribed));
        }
        for edge in [i64::MIN, i64::MAX] {
            let other_frame = Id::new();
            let old = Box3::new(
                frame,
                [edge.saturating_sub(5000); 3],
                [edge.saturating_add(5000); 3],
            )
            .unwrap();
            let new = Box3 {
                frame: other_frame,
                ..old
            };
            let mut index = MovingIndex::default();
            let tag = Recipient::Worker(p.owner);
            let other = Recipient::Client(p.id);
            index.replace(p.id, None, old, tag);
            index.replace(p.id, Some((old, tag)), new, tag);
            assert!(index.candidates(old, other).is_empty());
            assert_eq!(index.candidates(new, other), [p.id]);
        }
    }
}
