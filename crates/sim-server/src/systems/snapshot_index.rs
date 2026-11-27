use glam::Vec3;
use shared::entity::{EntityId, EntityState};
use shared::spatial::{in_aoi, AOI_RADIUS, AOI_RADIUS_SQ};
use std::collections::HashMap;
use std::ops::Range;

type Cell = (i32, i32);
fn cell(p: Vec3) -> Cell {
    (
        (p.x / AOI_RADIUS).floor() as i32,
        (p.z / AOI_RADIUS).floor() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::ServerId;
    #[test]
    fn grid_matches_inclusive_circle_at_cell_edges_and_in_isolated_groups() {
        let mut entities = Vec::new();
        for (x, z) in [
            (-500., 0.),
            (0., 0.),
            (0., 499.99),
            (300., 400.),
            (300., 400.1),
            (-500.01, 0.),
            (4000., 4000.),
            (4010., 4010.),
        ] {
            entities.push(EntityState::new_player(Vec3::new(x, 0., z), ServerId(1)));
        }
        let index = SnapshotIndex::build(&mut entities);
        let mut scratch = Vec::new();
        for e in &entities {
            let (cohort, visible) = index.visible(e.position, e.id, &entities, &mut scratch);
            let mut got: Vec<_> = visible.iter().map(|v| v.id.0).collect();
            let mut expected: Vec<_> = entities
                .iter()
                .filter(|v| in_aoi(e.position, v.position))
                .map(|v| v.id.0)
                .collect();
            got.sort_unstable();
            expected.sort_unstable();
            assert_eq!(got, expected);
            assert_eq!(cohort.is_some(), e.position.x >= 4000.);
        }
    }
}
fn neighbors((x, z): Cell) -> impl Iterator<Item = Cell> {
    (x.saturating_sub(1)..=x.saturating_add(1))
        .flat_map(move |i| (z.saturating_sub(1)..=z.saturating_add(1)).map(move |j| (i, j)))
}
struct Bucket {
    range: Range<usize>,
    lo: Vec3,
    hi: Vec3,
    isolated: bool,
}
pub(super) struct SnapshotIndex(HashMap<Cell, Bucket>);
impl SnapshotIndex {
    pub(super) fn build(entities: &mut [EntityState]) -> Self {
        entities.sort_by_key(|e| cell(e.position));
        let mut buckets = HashMap::<Cell, Bucket>::new();
        for (i, e) in entities.iter().enumerate() {
            let p = e.position;
            let b = buckets.entry(cell(p)).or_insert(Bucket {
                range: i..i,
                lo: p,
                hi: p,
                isolated: false,
            });
            b.range.end = i + 1;
            b.lo = b.lo.min(p);
            b.hi = b.hi.max(p);
        }
        let isolated: Vec<_> = buckets
            .iter()
            .filter_map(|(&key, b)| {
                if !in_aoi(b.lo, b.hi) {
                    return None;
                }
                neighbors(key)
                    .filter(|n| *n != key)
                    .all(|n| {
                        buckets.get(&n).is_none_or(|other| {
                            let dx = (other.lo.x - b.hi.x).max(b.lo.x - other.hi.x).max(0.);
                            let dz = (other.lo.z - b.hi.z).max(b.lo.z - other.hi.z).max(0.);
                            dx * dx + dz * dz > AOI_RADIUS_SQ
                        })
                    })
                    .then_some(key)
            })
            .collect();
        for key in isolated {
            buckets.get_mut(&key).unwrap().isolated = true;
        }
        Self(buckets)
    }
    /// An isolated bucket has precisely the same visible IDs for every member.
    /// On its boundary filter the nine possible buckets with the exact XZ test.
    pub(super) fn visible<'a>(
        &self,
        p: Vec3,
        id: EntityId,
        entities: &'a [EntityState],
        scratch: &'a mut Vec<EntityState>,
    ) -> (Option<Cell>, &'a [EntityState]) {
        let key = cell(p);
        if let Some(b) = self.0.get(&key).filter(|b| b.isolated) {
            return (Some(key), &entities[b.range.clone()]);
        }
        scratch.clear();
        for b in neighbors(key).filter_map(|n| self.0.get(&n)) {
            scratch.extend(
                entities[b.range.clone()]
                    .iter()
                    .filter(|e| e.id == id || in_aoi(e.position, p))
                    .cloned(),
            );
        }
        (None, scratch)
    }
}
