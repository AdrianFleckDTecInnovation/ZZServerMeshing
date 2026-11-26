//! Interest broad phase. Nearby small objects share an octree entry. Within it a
//! query can skip every member belonging to its own worker in one operation.
use crate::{octree::Index, Box3, Id, Recipient, Worker};
use std::collections::{BTreeMap, HashMap};

const CELL: i64 = 32_768;
const RADIUS: i64 = CELL / 4;
type Cell = [i64; 3];
type Buckets = BTreeMap<Option<Worker>, Members>;

// Bound linear scans when one owner packs many small objects into one cell.
// Discard the local octree when the last member leaves.
enum Members {
    Small(BTreeMap<Id, Box3>),
    Dense { index: Index, len: usize },
}

impl Default for Members {
    fn default() -> Self {
        Self::Small(BTreeMap::new())
    }
}

impl Members {
    fn replace(&mut self, id: Id, old: Option<Box3>, new: Box3) {
        match self {
            Self::Small(items) => {
                items.insert(id, new);
                if items.len() > 128 {
                    let mut index = Index::default();
                    for (&id, &b) in items.iter() {
                        index.insert(id, b);
                    }
                    *self = Self::Dense {
                        index,
                        len: items.len(),
                    };
                }
            }
            Self::Dense { index, len } => {
                index.replace(id, old, new);
                *len += usize::from(old.is_none());
            }
        }
    }

    fn remove(&mut self, id: Id, b: Box3) -> bool {
        match self {
            Self::Small(items) => {
                items.remove(&id);
                items.is_empty()
            }
            Self::Dense { index, len } => {
                index.remove(id, b);
                *len -= 1;
                *len == 0
            }
        }
    }

    fn query(&self, b: Box3, hits: &mut Vec<Id>) {
        match self {
            Self::Small(items) => hits.extend(
                items
                    .iter()
                    .filter(|(_, bounds)| bounds.intersects(b))
                    .map(|(&id, _)| id),
            ),
            Self::Dense { index, .. } => index.query_into(b, hits),
        }
    }
}

fn worker(to: Recipient) -> Option<Worker> {
    match to {
        Recipient::Worker(w) => Some(w),
        Recipient::Client(_) => None,
    }
}

fn cover(mut b: Box3) -> Box3 {
    const STEP: i64 = 2048;
    for k in 0..3 {
        if b.hi[k].saturating_sub(b.lo[k]) >= 2 * STEP {
            b.lo[k] &= !(STEP - 1);
            b.hi[k] |= STEP - 1;
        }
    }
    b
}

fn cell_at(v: i64) -> i64 {
    v.div_euclid(CELL) + i64::from(v.rem_euclid(CELL) >= CELL / 2)
}

fn home(b: Box3) -> Option<Cell> {
    if (0..3).any(|k| b.hi[k].saturating_sub(b.lo[k]) > 2 * RADIUS) {
        return None;
    }
    Some(std::array::from_fn(|k| {
        cell_at(b.lo[k] + (b.hi[k] - b.lo[k]) / 2)
    }))
}

fn envelope(frame: Id, cell: Cell) -> Box3 {
    // A small box may extend RADIUS past its centre's cell, including on a shared face.
    let bound = |x: i64, offset: i64| {
        (i128::from(x) * i128::from(CELL) + i128::from(offset))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    };
    Box3 {
        frame,
        lo: cell.map(|x| bound(x, -CELL / 2 - RADIUS)),
        hi: cell.map(|x| bound(x, CELL / 2 - 1 + RADIUS)),
    }
}

#[derive(Default)]
pub(crate) struct MovingIndex {
    regions: Index<Cell>,
    members: HashMap<(Id, Cell), Buckets>,
    large: Index,
    large_owners: HashMap<Id, Option<Worker>>,
}

impl MovingIndex {
    pub fn replace(&mut self, id: Id, old: Option<(Box3, Recipient)>, new: Box3, to: Recipient) {
        let new = cover(new);
        let owner = worker(to);
        let old = old.map(|(b, to)| (cover(b), worker(to)));
        if old == Some((new, owner)) {
            return;
        }
        let cell = home(new);
        if let Some((previous, previous_owner)) = old {
            let previous_cell = home(previous);
            if cell.is_none() && previous_cell.is_none() {
                self.large.replace(id, Some(previous), new);
                self.large_owners.insert(id, owner);
                return;
            }
            if let Some(home_cell) = cell.filter(|c| {
                Some(*c) == previous_cell && previous.frame == new.frame && owner == previous_owner
            }) {
                let members = self
                    .members
                    .get_mut(&(new.frame, home_cell))
                    .expect("registered region");
                members
                    .get_mut(&owner)
                    .expect("registered owner")
                    .replace(id, Some(previous), new);
                return;
            }
            self.remove_covered(id, previous, previous_owner, previous_cell);
        }
        if let Some(cell) = cell {
            let buckets = self.members.entry((new.frame, cell)).or_insert_with(|| {
                self.regions.insert(cell, envelope(new.frame, cell));
                Buckets::new()
            });
            buckets.entry(owner).or_default().replace(id, None, new);
        } else {
            self.large.insert(id, new);
            self.large_owners.insert(id, owner);
        }
    }

    pub fn remove(&mut self, id: Id, b: Box3, to: Recipient) {
        let b = cover(b);
        self.remove_covered(id, b, worker(to), home(b));
    }

    fn remove_covered(&mut self, id: Id, b: Box3, owner: Option<Worker>, cell: Option<Cell>) {
        if let Some(cell) = cell {
            let key = (b.frame, cell);
            let buckets = self.members.get_mut(&key).expect("registered region");
            let members = buckets.get_mut(&owner).expect("registered owner");
            if members.remove(id, b) {
                buckets.remove(&owner);
            }
            if buckets.is_empty() {
                self.members.remove(&key);
                self.regions.remove(cell, envelope(b.frame, cell));
            }
        } else {
            self.large.remove(id, b);
            self.large_owners.remove(&id);
        }
    }

    /// A small query can only reach centres inside this expanded range. An unchanged
    /// empty membership needs no narrow phase if all nearby buckets belong to its writer.
    pub fn excludes_others(&self, b: Box3, to: Recipient) -> bool {
        if self.large.has_frame(b.frame) {
            return false;
        }
        let first = b.lo.map(|x| cell_at(x.saturating_sub(RADIUS)));
        let last = b.hi.map(|x| cell_at(x.saturating_add(RADIUS)));
        if (0..3).any(|k| last[k] - first[k] > 1) {
            return false;
        }
        let owner = worker(to);
        for x in first[0]..=last[0] {
            for y in first[1]..=last[1] {
                for z in first[2]..=last[2] {
                    if let Some(buckets) = self.members.get(&(b.frame, [x, y, z])) {
                        if owner.is_none() || buckets.len() != 1 || !buckets.contains_key(&owner) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    pub fn candidates(&self, b: Box3, exclude: Recipient) -> Vec<Id> {
        let exclude = worker(exclude);
        let mut hits = self.large.query(b);
        if exclude.is_some() {
            hits.retain(|id| self.large_owners[id] != exclude);
        }
        let mut visit = |cell| {
            if let Some(buckets) = self.members.get(&(b.frame, cell)) {
                for (&owner, members) in buckets {
                    if exclude.is_some() && owner == exclude {
                        continue;
                    }
                    members.query(b, &mut hits);
                }
            }
        };
        let first = b.lo.map(|x| cell_at(x.saturating_sub(RADIUS)));
        let last = b.hi.map(|x| cell_at(x.saturating_add(RADIUS)));
        if first == last {
            visit(first);
        } else {
            self.regions.visit(b, visit);
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_match_exhaustive_queries_across_owner_frame_and_size_changes() {
        let frames = [Id::numbered(1), Id::numbered(2)];
        let tags = [
            Recipient::Worker(Worker {
                slot: 1,
                incarnation: 1,
            }),
            Recipient::Worker(Worker {
                slot: 1,
                incarnation: 2,
            }),
            Recipient::Client(Id::numbered(1)),
        ];
        let mut index = MovingIndex::default();
        let mut objects = BTreeMap::new();
        let check = |index: &MovingIndex,
                     objects: &BTreeMap<Id, (Box3, Recipient)>,
                     query: Box3,
                     to: Recipient| {
            let expected: Vec<_> = objects
                .iter()
                .filter(|(_, (bounds, tag))| {
                    (worker(to).is_none() || worker(to) != worker(*tag))
                        && cover(*bounds).intersects(query)
                })
                .map(|(&id, _)| id)
                .collect();
            let mut actual = index.candidates(query, to);
            actual.sort_unstable();
            assert_eq!(actual, expected);
            assert!(!index.excludes_others(query, to) || expected.is_empty());
        };
        // Promote one owner to its local octree, then move and remove members from it.
        for i in 0..160 {
            let id = Id::numbered(i);
            let b = Box3::new(frames[0], [i as i64; 3], [i as i64; 3]).unwrap();
            index.replace(id, None, b, tags[0]);
            objects.insert(id, (b, tags[0]));
        }
        let mut state = 981_421_u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state >> 19
        };
        for turn in 0..1800 {
            let id = Id::numbered(u128::from(next() % 200));
            let frame = frames[(next() % 2) as usize];
            let tag = tags[(next() % 3) as usize];
            let p: [i64; 3] = std::array::from_fn(|_| match next() % 8 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => -CELL / 2,
                3 => CELL / 2 - 1,
                _ => (next() % 150_000) as i64 - 75_000,
            });
            let radius = [0, 50, 5000, RADIUS, CELL * 4][(next() % 5) as usize];
            let b = Box3::new(
                frame,
                p.map(|v| v.saturating_sub(radius)),
                p.map(|v| v.saturating_add(radius)),
            )
            .unwrap();
            let old = objects.remove(&id);
            if turn % 7 == 0 {
                if let Some((old, to)) = old {
                    index.remove(id, old, to);
                }
            } else {
                index.replace(id, old, b, tag);
                objects.insert(id, (b, tag));
            }
            for to in tags {
                check(&index, &objects, b, to);
                check(&index, &objects, Box3::new(frame, b.hi, b.hi).unwrap(), to);
            }
        }
        for (id, (b, to)) in objects {
            index.remove(id, b, to);
        }
        assert!(index.members.is_empty() && index.large_owners.is_empty());
        assert!(frames
            .into_iter()
            .all(|f| !index.regions.has_frame(f) && !index.large.has_frame(f)));
    }
}
