//! Optional authoritative Bevy workload for the profiler.
use bevy::prelude::*;
use infinite_stars::Player;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use is_mesh::{
    invalid, Id, Lookup, MeshApp, Pool, Result, Rules, Simulated, Store, Visibility, Worker,
};

const VITALS: u32 = 60_001;

#[derive(Clone, Copy)]
pub struct Settings {
    pub count: usize,
    pub interval: u32,
    pub local_only: bool,
}

struct Effect {
    period: u32,
    offset: u32,
    strength: i32,
}

#[derive(Component)]
struct Loadout {
    phase: u32,
    effects: Box<[Effect]>,
}

#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Vitals {
    through: u32,
    updates: u32,
    damage: u64,
    healing: u64,
}

#[derive(Resource, Default)]
struct Meter {
    tick: u32,
    interval: u32,
    busy: Duration,
    updates: u64,
}

pub fn install(app: &mut App, s: Settings) -> Result<()> {
    if s.count == 0 {
        return Ok(());
    }
    if !s.local_only {
        app.mesh_component::<Vitals>(
            VITALS,
            1,
            Rules {
                visibility: Visibility::Private,
                migrate: true,
                persist: true,
            },
            Default::default(),
        )?;
    }
    app.insert_resource(Meter {
        interval: s.interval,
        ..Default::default()
    })
    .add_systems(Update, evaluate);
    Ok(())
}

pub fn attach(world: &mut World, entity: Entity, player: usize, s: Settings) {
    if s.count == 0 {
        return;
    }
    // Different players have different cooldowns and damage/healing profiles.
    let mut rng = (player as u64).wrapping_add(1);
    let mut next = || {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        (rng >> 32) as u32
    };
    let effects = (0..s.count)
        .map(|_| {
            let period = 2 + next() % 31;
            let offset = next() % period;
            let strength = (next() % 65) as i32 - 32;
            Effect {
                period,
                offset,
                strength,
            }
        })
        .collect();
    world.entity_mut(entity).insert((
        Vitals::default(),
        Loadout {
            phase: player as u32 % s.interval,
            effects,
        },
    ));
}

fn evaluate(
    mut meter: ResMut<Meter>,
    mut actors: Query<(&Loadout, &mut Vitals), (With<Player>, With<Simulated>)>,
) {
    let start = Instant::now();
    meter.tick += 1;
    let now = meter.tick;
    let phase = (now - 1) % meter.interval;
    for (loadout, mut vitals) in &mut actors {
        if loadout.phase != phase {
            continue;
        }
        let mut damage = 0;
        let mut healing = 0;
        for e in &loadout.effects {
            let pulses = (now + e.offset) / e.period - (vitals.through + e.offset) / e.period;
            let points = u64::from(pulses) * u64::from(e.strength.unsigned_abs());
            if e.strength < 0 {
                damage += points;
            } else {
                healing += points;
            }
        }
        vitals.through = now;
        vitals.updates += 1;
        vitals.damage += damage;
        vitals.healing += healing;
        meter.updates += 1;
    }
    meter.busy += start.elapsed();
}

pub fn totals(pool: &Pool, workers: &[Worker]) -> Result<(Duration, u64)> {
    let mut busy = Duration::ZERO;
    let mut updates = 0;
    for &worker in workers {
        if let Some(meter) = pool.app(worker)?.world().get_resource::<Meter>() {
            busy += meter.busy;
            updates += meter.updates;
        }
    }
    Ok((busy, updates))
}

pub fn verify(
    source: &World,
    own: Entity,
    target: &World,
    replica: Entity,
    ticks: u32,
    s: Settings,
) -> Result<()> {
    if s.count == 0 {
        return Ok(());
    }
    let loadout = source
        .get::<Loadout>(own)
        .ok_or_else(|| invalid("missing effect loadout"))?;
    let updates = (ticks + s.interval - 1 - loadout.phase) / s.interval;
    let through = if updates == 0 {
        0
    } else {
        1 + loadout.phase + (updates - 1) * s.interval
    };
    // Independently derive the lifetime totals without relying on the measured tick schedule.
    let mut expected = Vitals {
        updates,
        through,
        ..Default::default()
    };
    for e in &loadout.effects {
        let pulses = (through + e.offset) / e.period;
        let points = u64::from(pulses) * u64::from(e.strength.unsigned_abs());
        if e.strength < 0 {
            expected.damage += points;
        } else {
            expected.healing += points;
        }
    }
    if source.get::<Vitals>(own) != Some(&expected) || target.get::<Vitals>(replica).is_some() {
        return Err(invalid(
            "effect totals, cadence or private replica mismatch",
        ));
    }
    Ok(())
}

pub fn verify_checkpoint(db: &Store, actor: Id, ticks: u32, s: Settings) -> Result<()> {
    if s.count == 0 {
        return Ok(());
    }
    let Lookup::Present(record) = db.lookup(actor)? else {
        return Err(invalid("missing checkpoint actor"));
    };
    if s.local_only {
        if record.fields.contains_key(&VITALS) {
            return Err(invalid("transient effects persisted"));
        }
    } else {
        let stored: Vitals = record.get(VITALS, 1)?;
        if stored.through > ticks || (ticks >= 20 + s.interval && stored.updates == 0) {
            return Err(invalid("missing or invalid effects checkpoint"));
        }
    }
    Ok(())
}
