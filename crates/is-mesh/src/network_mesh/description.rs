use crate::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Description {
    pub presence: Presence,
    pub area: Option<Area>,
    pub group: Id,
    pub sight: Option<SightPoint>,
}

// Most worker areas use exactly the same bounds and motion as their presence.
// Keep the independent representation for game-defined areas that differ.
#[derive(Serialize, Deserialize)]
enum EncodedArea<Demands, Independent> {
    Physical {
        hysteresis: u32,
        components: Demands,
    },
    Independent(Independent),
}

#[derive(Serialize, Deserialize)]
enum Anchor {
    Centre(u32),
    Point(SightPoint),
}
fn centre(bounds: Box3) -> [i64; 3] {
    std::array::from_fn(|i| bounds.lo[i].midpoint(bounds.hi[i]))
}

impl Serialize for Description {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let p = self.presence;
        let area = self.area.as_ref().map(|a| {
            if a.activity
                && a.id == p.id
                && a.worker == p.owner
                && a.to == Recipient::Worker(p.owner)
                && a.bounds == p.bounds
                && a.velocity == p.velocity
                && a.ahead == p.ahead
            {
                EncodedArea::Physical {
                    hysteresis: a.hysteresis,
                    components: &a.components,
                }
            } else {
                EncodedArea::Independent(a)
            }
        });
        let anchor = self.sight.map(|point| {
            if point.mm == centre(p.bounds) {
                Anchor::Centre(point.kind)
            } else {
                Anchor::Point(point)
            }
        });
        // The object's own id is the common principal. Encode it only once.
        (
            p.id,
            p.owner,
            (p.principal != p.id).then_some(p.principal),
            p.bounds,
            p.velocity,
            p.ahead,
            area,
            self.group,
            anchor,
        )
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Description {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        type AreaData = EncodedArea<BTreeMap<u32, Demand>, Area>;
        let (id, owner, principal, bounds, velocity, ahead, area, group, anchor) =
            <(
                Id,
                Worker,
                Option<Id>,
                Box3,
                [i64; 3],
                u16,
                Option<AreaData>,
                Id,
                Option<Anchor>,
            )>::deserialize(d)?;
        let presence = Presence {
            id,
            owner,
            principal: principal.unwrap_or(id),
            bounds,
            velocity,
            ahead,
        };
        let area = area.map(|a| match a {
            EncodedArea::Physical {
                hysteresis,
                components,
            } => Area {
                id,
                worker: owner,
                to: Recipient::Worker(owner),
                bounds,
                velocity,
                ahead,
                activity: true,
                hysteresis,
                components,
            },
            EncodedArea::Independent(area) => area,
        });
        let sight = anchor.map(|anchor| match anchor {
            Anchor::Centre(kind) => SightPoint {
                kind,
                mm: centre(bounds),
            },
            Anchor::Point(point) => point,
        });
        Ok(Self {
            presence,
            area,
            group,
            sight,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_reports_preserve_physical_and_independent_areas_and_exact_anchors() {
        let id = Id::numbered(7);
        let owner = Worker {
            slot: 2,
            incarnation: 9,
        };
        let bounds = Box3::new(
            Id::numbered(17),
            [95_000, -5000, -5000],
            [105_000, 5000, 5000],
        )
        .unwrap();
        let p = Presence {
            id,
            owner,
            principal: id,
            bounds,
            velocity: [250, 0, 0],
            ahead: 5,
        };
        let a = Area {
            id,
            worker: owner,
            to: Recipient::Worker(owner),
            bounds,
            velocity: p.velocity,
            ahead: p.ahead,
            hysteresis: 1000,
            activity: true,
            components: BTreeMap::from([
                (
                    2,
                    Demand {
                        detail: Detail::Exact,
                        period: 1,
                    },
                ),
                (
                    4,
                    Demand {
                        detail: Detail::Exact,
                        period: 4,
                    },
                ),
            ]),
        };
        let mut d = Description {
            presence: p,
            area: Some(a),
            group: Id::numbered(91),
            sight: Some(SightPoint {
                kind: 1,
                mm: [100_000, 0, 0],
            }),
        };
        let legacy = encode(&(d.presence, &d.area, d.group)).unwrap();
        let bytes = encode(&d).unwrap();
        assert!(bytes.len() < legacy.len());
        assert_eq!(decode::<Description>(&bytes).unwrap(), d);
        let a = d.area.as_mut().unwrap();
        a.to = Recipient::Client(id);
        a.activity = false;
        a.bounds = Box3::new(Id::numbered(18), [1; 3], [3; 3]).unwrap();
        d.presence.principal = Id::numbered(70);
        d.presence.bounds = Box3::new(id, [i64::MIN; 3], [i64::MAX; 3]).unwrap();
        d.sight.as_mut().unwrap().mm = [i64::MIN, i64::MAX, 0];
        assert_eq!(decode::<Description>(&encode(&d).unwrap()).unwrap(), d);
        d.area = None;
        d.sight = None;
        assert_eq!(decode::<Description>(&encode(&d).unwrap()).unwrap(), d);
    }
}
