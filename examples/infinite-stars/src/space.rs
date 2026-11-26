use crate::{movement::Drive, Chunk, Player, Position, Ship, CHUNK, PLAYER, POSITION, SHIP};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::ErrorKind, path::Path};
use is_mesh::{
    invalid, Box3, Demand, Detail, Managed, Result, SightPoint, SightRanges, SightRule,
    SpatialProfile, SpatialRule,
};

/// The configuration file uses metres. The runtime uses integer millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Resource, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpaceSettings {
    #[serde(rename = "player_sight_m", with = "metres")]
    pub player_sight_mm: u64,
    #[serde(rename = "ship_sight_m", with = "metres")]
    pub ship_sight_mm: u64,
    #[serde(rename = "player_half_extent_m", with = "metres")]
    pub player_half_extent_mm: u32,
    #[serde(rename = "chunk_half_extent_m", with = "metres")]
    pub chunk_half_extent_mm: u32,
    #[serde(rename = "object_half_extent_m", with = "metres")]
    pub object_half_extent_mm: u32,
    #[serde(rename = "ship_half_extent_m", with = "metres")]
    pub ship_half_extent_mm: u32,
    #[serde(rename = "contact_buffer_m", with = "metres")]
    pub contact_buffer_mm: u32,
    pub forecast_ticks: u16,
}
impl Default for SpaceSettings {
    fn default() -> Self {
        Self {
            player_sight_mm: 100_000,
            ship_sight_mm: 500_000,
            player_half_extent_mm: 5000,
            chunk_half_extent_mm: 16000,
            object_half_extent_mm: 500,
            ship_half_extent_mm: 500,
            contact_buffer_mm: 1000,
            forecast_ticks: 5,
        }
    }
}
impl SpaceSettings {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("space.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn install(self, app: &mut App) {
        app.insert_resource(self)
            .insert_resource(SightRanges::millimetres(
                PLAYER,
                [(PLAYER, self.player_sight_mm), (SHIP, self.ship_sight_mm)],
            ))
            .insert_resource(SightRule(|w, e| {
                let kind = if w.get::<Player>(e).is_some() {
                    PLAYER
                } else if w.get::<Ship>(e).is_some() {
                    SHIP
                } else {
                    return None;
                };
                let p = w.get::<Position>(e)?;
                Some((p.frame, SightPoint { kind, mm: p.mm }))
            }))
            .insert_resource(SpatialRule(physical))
            .insert_resource(is_mesh::SpatialDependencies(
                [PLAYER, SHIP, POSITION, CHUNK, crate::movement::DRIVE]
                    .into_iter()
                    .collect(),
            ));
    }
}
mod metres {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer, T: Copy + Into<u64>>(mm: &T, s: S) -> Result<S::Ok, S::Error> {
        let mm = (*mm).into();
        if mm % 1000 == 0 {
            s.serialize_u64(mm / 1000)
        } else {
            s.serialize_f64(mm as f64 / 1000.)
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: TryFrom<u64>>(d: D) -> Result<T, D::Error> {
        let metres = serde_json::Number::deserialize(d)?;
        let mm = if let Some(whole) = metres.as_u64() {
            whole.checked_mul(1000)
        } else {
            metres.as_f64().and_then(|v| {
                let mm = v * 1000.;
                (0.0..=(1u64 << 53) as f64)
                    .contains(&mm)
                    .then(|| mm.round() as u64)
            })
        };
        mm.and_then(|v| T::try_from(v).ok()).ok_or_else(|| {
            D::Error::custom("distance must be nonnegative and fit in integer millimetres")
        })
    }
}
fn physical(w: &World, e: Entity) -> Option<SpatialProfile> {
    let settings = w.resource::<SpaceSettings>();
    let p = w.get::<Position>(e)?;
    let id = w.get::<Managed>(e)?.id;
    let player = w.get::<Player>(e).is_some();
    let extent = i64::from(if player {
        settings.player_half_extent_mm
    } else if w.get::<Chunk>(e).is_some() {
        settings.chunk_half_extent_mm
    } else if w.get::<Ship>(e).is_some() {
        settings.ship_half_extent_mm
    } else {
        settings.object_half_extent_mm
    });
    Some(SpatialProfile {
        bounds: Box3::new(
            p.frame,
            p.mm.map(|v| v.saturating_sub(extent)),
            p.mm.map(|v| v.saturating_add(extent)),
        )
        .ok()?,
        velocity: w
            .get::<Drive>(e)
            .map_or([0; 3], |d| d.axes.map(|v| i64::from(v) * 250 / 32767)),
        ahead: settings.forecast_ticks,
        retention: settings.contact_buffer_mm,
        principal: id,
        active: player,
        needs: if player {
            BTreeMap::from([
                (
                    POSITION,
                    Demand {
                        detail: Detail::Exact,
                        period: 1,
                    },
                ),
                (
                    CHUNK,
                    Demand {
                        detail: Detail::Exact,
                        period: 4,
                    },
                ),
            ])
        } else {
            BTreeMap::new()
        },
    })
}
