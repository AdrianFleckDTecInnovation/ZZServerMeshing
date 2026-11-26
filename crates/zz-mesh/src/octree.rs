//! One sparse loose octree per reference frame. A box lives in exactly one octant.
use crate::{Box3, Id};
use std::collections::BTreeMap;

// Below one metre, exact integer comparisons in the leaf are cheaper than more nodes.
const LEAF_MM: i128 = 1024;

#[derive(Clone, Copy)]
struct Cube {
    lo: [i128; 3],
    width: i128,
}

fn center(b: Box3) -> [i128; 3] {
    std::array::from_fn(|k| (i128::from(b.lo[k]) + i128::from(b.hi[k])).div_euclid(2))
}

impl Cube {
    fn around(b: Box3) -> Self {
        let side = (0..3)
            .map(|k| i128::from(b.hi[k]) - i128::from(b.lo[k]) + 1)
            .max()
            .unwrap();
        let width = (side.max(LEAF_MM) as u128).next_power_of_two() as i128;
        Self {
            lo: center(b).map(|p| p - width / 2),
            width,
        }
    }
    fn octant(self, p: [i128; 3]) -> usize {
        let mut child = 0;
        for (k, coord) in p.iter().enumerate() {
            child |= usize::from(*coord >= self.lo[k] + self.width / 2) << k;
        }
        child
    }
    fn child(self, i: usize) -> Self {
        let width = self.width / 2;
        let lo = std::array::from_fn(|k| self.lo[k] + if i & (1 << k) != 0 { width } else { 0 });
        Self { lo, width }
    }
    fn fits(self, b: Box3) -> bool {
        (0..3).all(|k| {
            i128::from(b.lo[k]) >= self.lo[k] - self.width / 2
                && i128::from(b.hi[k]) < self.lo[k] + self.width + self.width / 2
        })
    }
    fn contains_center(self, p: [i128; 3]) -> bool {
        (0..3).all(|k| (self.lo[k]..self.lo[k] + self.width).contains(&p[k]))
    }
    fn loose_bounds(self, frame: Id) -> Box3 {
        let clamp = |v: i128| v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        Box3 {
            frame,
            lo: self.lo.map(|x| clamp(x - self.width / 2)),
            hi: self.lo.map(|x| clamp(x + self.width + self.width / 2 - 1)),
        }
    }
}

struct Node<K> {
    cube: Cube,
    loose: Box3,
    entries: BTreeMap<K, Box3>,
    children: [Option<Box<Node<K>>>; 8],
}
impl<K: Copy + Ord> Node<K> {
    fn new(cube: Cube, frame: Id) -> Self {
        Self {
            cube,
            loose: cube.loose_bounds(frame),
            entries: BTreeMap::new(),
            children: Default::default(),
        }
    }
    fn insert(&mut self, id: K, b: Box3, p: [i128; 3]) {
        if self.cube.width > LEAF_MM {
            let i = self.cube.octant(p);
            let child = self.cube.child(i);
            if child.fits(b) {
                self.children[i]
                    .get_or_insert_with(|| Box::new(Self::new(child, b.frame)))
                    .insert(id, b, p);
                return;
            }
        }
        self.entries.insert(id, b);
    }
    fn remove(&mut self, id: K, p: [i128; 3]) {
        // An old root can become a child when the tree grows around a distant cluster.
        if self.entries.remove(&id).is_none() {
            let child = &mut self.children[self.cube.octant(p)];
            if let Some(node) = child {
                node.remove(id, p);
                if node.empty() {
                    *child = None;
                }
            }
        }
    }
    fn relocate(&mut self, id: K, old_center: [i128; 3], new_center: [i128; 3], b: Box3) -> bool {
        if !self.cube.fits(b) || !self.cube.contains_center(new_center) {
            return false;
        }
        if let Some(entry) = self.entries.get_mut(&id) {
            *entry = b;
            return true;
        }
        let index = self.cube.octant(old_center);
        index == self.cube.octant(new_center)
            && self.children[index]
                .as_mut()
                .is_some_and(|c| c.relocate(id, old_center, new_center, b))
    }
    fn empty(&self) -> bool {
        self.entries.is_empty() && self.children.iter().all(Option::is_none)
    }
    fn query(&self, b: Box3, out: &mut Vec<K>) {
        if !self.loose.intersects(b) {
            return;
        }
        out.extend(
            self.entries
                .iter()
                .filter(|(_, e)| e.intersects(b))
                .map(|(&id, _)| id),
        );
        for child in self.children.iter().flatten() {
            child.query(b, out);
        }
    }
}

#[derive(Default)]
pub(crate) struct Index<K = Id>(BTreeMap<Id, Node<K>>);
impl<K: Copy + Ord> Index<K> {
    pub fn has_frame(&self, frame: Id) -> bool {
        self.0.contains_key(&frame)
    }
    pub fn replace(&mut self, id: K, old: Option<Box3>, new: Box3) {
        if let Some(old) = old {
            if old == new
                || (old.frame == new.frame
                    && self
                        .0
                        .get_mut(&old.frame)
                        .is_some_and(|r| r.relocate(id, center(old), center(new), new)))
            {
                return;
            }
            self.remove(id, old);
        }
        self.insert(id, new);
    }
    pub fn insert(&mut self, id: K, b: Box3) {
        let p = center(b);
        let root = self
            .0
            .entry(b.frame)
            .or_insert_with(|| Node::new(Cube::around(b), b.frame));
        while !root.cube.contains_center(p) || !root.cube.fits(b) {
            let old = root.cube;
            let lo = std::array::from_fn(|k| {
                if p[k] < old.lo[k] + old.width / 2 {
                    old.lo[k] - old.width
                } else {
                    old.lo[k]
                }
            });
            let parent = Cube {
                lo,
                width: old.width * 2,
            };
            let i = parent.octant(old.lo);
            let child = std::mem::replace(root, Node::new(parent, b.frame));
            root.children[i] = Some(Box::new(child));
        }
        root.insert(id, b, p);
    }
    pub fn remove(&mut self, id: K, b: Box3) {
        if let Some(root) = self.0.get_mut(&b.frame) {
            root.remove(id, center(b));
            while root.entries.is_empty() {
                let mut children = root.children.iter_mut().flatten();
                match (children.next(), children.next()) {
                    (None, _) => {
                        self.0.remove(&b.frame);
                        break;
                    }
                    (Some(_), None) => {
                        *root = *root.children.iter_mut().find_map(Option::take).unwrap();
                    }
                    _ => break,
                }
            }
        }
    }
    pub fn query(&self, b: Box3) -> Vec<K> {
        let mut out = Vec::new();
        self.query_into(b, &mut out);
        out
    }
    pub fn query_into(&self, b: Box3, out: &mut Vec<K>) {
        if let Some(root) = self.0.get(&b.frame) {
            root.query(b, out);
        }
    }
}

// Moving an inclusive AABB can only change membership if one face crosses an
// opposing face. Counts allow observers to share coordinates and move independently.
// A crossing is conservative: the octree still checks all three axes exactly.
#[derive(Default)]
struct Faces {
    low: [BTreeMap<i64, usize>; 3],
    high: [BTreeMap<i64, usize>; 3],
}
#[derive(Default)]
pub(crate) struct Boundaries(BTreeMap<Id, Faces>);
impl Boundaries {
    pub fn replace(&mut self, old: Option<Box3>, new: Option<Box3>) {
        if old == new {
            return;
        }
        if old.zip(new).is_some_and(|(a, b)| a.frame != b.frame) {
            self.replace(old, None);
            self.replace(None, new);
            return;
        }
        let frame = new.or(old).unwrap().frame;
        let faces = self.0.entry(frame).or_default();
        for k in 0..3 {
            adjust(
                &mut faces.low[k],
                old.map(|b| b.lo[k]),
                new.map(|b| b.lo[k]),
            );
            adjust(
                &mut faces.high[k],
                old.map(|b| b.hi[k]),
                new.map(|b| b.hi[k]),
            );
        }
        if faces.low[0].is_empty() {
            self.0.remove(&frame);
        }
    }
    pub fn stable(&self, old: Box3, new: Box3) -> bool {
        use std::ops::Bound::{Excluded, Included};
        if old.frame != new.frame {
            return false;
        }
        let Some(faces) = self.0.get(&old.frame) else {
            return true;
        };
        (0..3).all(|k| {
            let (l, r) = (old.lo[k].min(new.lo[k]), old.lo[k].max(new.lo[k]));
            let (b, t) = (old.hi[k].min(new.hi[k]), old.hi[k].max(new.hi[k]));
            (l == r || faces.high[k].range(l..r).next().is_none())
                && (b == t
                    || faces.low[k]
                        .range((Excluded(b), Included(t)))
                        .next()
                        .is_none())
        })
    }
}
fn adjust(counts: &mut BTreeMap<i64, usize>, old: Option<i64>, new: Option<i64>) {
    if old == new {
        return;
    }
    if let Some(value) = old {
        let count = counts.get_mut(&value).expect("removing registered face");
        *count -= 1;
        if *count == 0 {
            counts.remove(&value);
        }
    }
    if let Some(value) = new {
        *counts.entry(value).or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn growing_pruning_octants_match_exhaustive_closed_boxes() {
        let frames = [Id::numbered(1), Id::numbered(2)];
        let mut seed = 3949u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed
        };
        let mut items = BTreeMap::new();
        let mut tree = Index::default();
        for turn in 0..3000 {
            let id = Id::numbered((random() % 120) as u128);
            let old = items.remove(&id);
            let frame = frames[(random() >> 63) as usize];
            let p: [i64; 3] = std::array::from_fn(|_| match random() % 7 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => (1i64 << 59) + (random() % 11) as i64,
                _ => (random() % 40_000) as i64 - 20_000,
            });
            let r = (random() % 2000) as i64;
            let b = Box3::new(
                frame,
                p.map(|x| x.saturating_sub(r)),
                p.map(|x| x.saturating_add(r)),
            )
            .unwrap();
            if turn % 7 != 0 {
                tree.replace(id, old, b);
                items.insert(id, b);
            } else if let Some(old) = old {
                tree.remove(id, old);
            }
            for q in [
                b,
                Box3::new(frame, b.hi, b.hi).unwrap(),
                Box3::new(frame, [i64::MIN; 3], [i64::MAX; 3]).unwrap(),
            ] {
                let mut actual = tree.query(q);
                actual.sort_unstable();
                assert_eq!(
                    actual,
                    items
                        .iter()
                        .filter(|(_, e)| e.intersects(q))
                        .map(|(&id, _)| id)
                        .collect::<Vec<_>>()
                );
            }
        }
        for (id, b) in items {
            tree.remove(id, b);
        }
        assert!(tree.0.is_empty());
        let start = Box3::new(frames[0], [i64::MIN; 3], [i64::MIN; 3]).unwrap();
        assert_eq!(start.sweep([i64::MAX; 3], 2, 0).hi, [i64::MAX - 1; 3]);
    }
}
