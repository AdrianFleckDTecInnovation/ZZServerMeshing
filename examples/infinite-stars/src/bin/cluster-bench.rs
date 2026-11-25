//! Moving, mutually interested players on the actual Bevy/Directory/Publisher path.
use bevy::prelude::Entity;
use infinite_stars::{world, Inventory, Player, Position, PLANET_ID, POSITION, STONE};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Instant};
use zz_mesh::*;

const PER_SITE: usize = 16;
const TRAVEL: usize = 32;
const HOLD: usize = 48;
const DISTANCE: i64 = 64_000;
const QUEUE: usize = 8192;

fn site(i: usize) -> [i64; 3] {
    match i {
        0 => [0, 0, 0],
        1 => [DISTANCE, 0, 0],
        2 => [0, DISTANCE, 0],
        3 => [0, 0, DISTANCE],
        n => [n as i64 * DISTANCE, 0, 0],
    }
}

struct Member {
    origin: usize,
    index: usize,
    owner: Worker,
    id: Id,
    entity: Entity,
    mm: [i64; 3],
}
struct Scene {
    pool: Pool,
    members: Vec<Member>,
    workers: Vec<Worker>,
    per_site: usize,
}
#[derive(Default)]
struct Sample {
    ns: Vec<u64>,
    simulate_publish_ns: u64,
    delivery_ns: u64,
    topology_ns: Vec<u64>,
    with_topology_ns: Vec<u64>,
    queue: usize,
    fields: usize,
    frames: usize,
    baselines: usize,
    observer: usize,
    foreign_origin: usize,
    candidates: u64,
    peak_pairs: usize,
    peak_islands: usize,
}
impl Scene {
    fn new(sites: usize, per_site: usize, workers: usize) -> Result<Self> {
        let db = Store::open(":memory:")?;
        let mut pool = Pool::new(db.clone(), QUEUE);
        // Hold writer placement fixed to isolate the cost of changing spatial interest.
        pool.policy.every = 0;
        let mut nodes = vec![];
        for slot in 1..=workers {
            let w = db.start_worker(slot as u32)?;
            pool.add(w, world(&db, w))?;
            nodes.push(w);
        }
        let mut members = vec![];
        for origin in 0..sites {
            for index in 0..per_site {
                let owner = nodes[(origin * 2 + index % 2) % workers];
                let id = Id::numbered(1000 + members.len() as u128);
                let mm = site(origin);
                let entity = pool
                    .app_mut(owner)?
                    .world_mut()
                    .spawn((
                        Managed::new(id, id),
                        Player,
                        Position {
                            frame: PLANET_ID,
                            mm,
                        },
                        Inventory(BTreeMap::from([(STONE, 9)])),
                    ))
                    .id();
                members.push(Member {
                    origin,
                    index,
                    owner,
                    id,
                    entity,
                    mm,
                });
            }
        }
        let mut scene = Self {
            pool,
            workers: nodes,
            members,
            per_site,
        };
        for _ in 0..6 {
            scene.tick(&mut Sample::default(), |m| site(m.origin))?;
        }
        Ok(scene)
    }

    fn tick(&mut self, stats: &mut Sample, center: impl Fn(&Member) -> [i64; 3]) -> Result<()> {
        let start = Instant::now();
        let phase = (self.pool.tick % 4) as i64;
        for m in &mut self.members {
            let [x, y, z] = center(m);
            // All players move even on a plateau. Sampling identical positions would hide replication.
            m.mm = [
                x + (m.index % 4) as i64 * 100,
                y + (m.index / 4) as i64 * 100,
                z + phase * 25,
            ];
            self.pool
                .app_mut(m.owner)?
                .world_mut()
                .get_mut::<Position>(m.entity)
                .unwrap()
                .mm = m.mm;
        }
        let candidates = self.pool.directory.candidates;
        self.pool.step()?;
        let step = start.elapsed();
        stats.candidates += self.pool.directory.candidates - candidates;
        stats.queue = stats.queue.max(self.pool.messages.len());
        // Inspect typed payload outside the measured step and delivery intervals.
        for msg in &self.pool.messages {
            let Recipient::Worker(to) = msg.to else {
                return Err(invalid("unexpected client route"));
            };
            let object = &self.members[(u128::from_be_bytes(msg.update.object.0) - 1000) as usize];
            assert_eq!(msg.from, object.owner);
            assert_ne!(msg.from, to);
            for (schema, field) in &msg.update.fields {
                if let Some(field) = field {
                    assert_eq!(*schema, POSITION, "unexpected disclosed field");
                    stats.fields += field.bytes.len();
                }
            }
            stats.frames += 1;
            stats.baselines += usize::from(msg.update.base.is_none());
            stats.observer += usize::from(to == self.workers[0]);
            stats.foreign_origin += usize::from((to.slot as usize - 1) / 2 != object.origin);
        }
        let delivery = Instant::now();
        let queued = self.pool.messages.len();
        assert_eq!(self.pool.deliver_all()?, queued, "frame rejected");
        let deliver_ns = delivery.elapsed().as_nanos() as u64;
        let step_ns = step.as_nanos() as u64 + deliver_ns;
        stats.simulate_publish_ns += step.as_nanos() as u64;
        stats.delivery_ns += deliver_ns;
        stats.ns.push(step_ns);
        let topology_start = Instant::now();
        let islands = self.pool.directory.islands();
        let topology_ns = topology_start.elapsed().as_nanos() as u64;
        stats.topology_ns.push(topology_ns);
        stats.with_topology_ns.push(step_ns + topology_ns);
        stats.peak_pairs = stats.peak_pairs.max(self.pool.routing().1);
        stats.peak_islands = stats.peak_islands.max(islands.len());
        self.check_islands(islands);
        self.check_views()
    }

    fn check_islands(&self, mut actual: Vec<Vec<Id>>) {
        let mut seen = vec![false; self.members.len()];
        let mut expected = Vec::new();
        for start in 0..seen.len() {
            if seen[start] {
                continue;
            }
            seen[start] = true;
            let mut pending = vec![start];
            let mut group = Vec::new();
            while let Some(i) = pending.pop() {
                let a = &self.members[i];
                group.push(a.id);
                for (j, b) in self.members.iter().enumerate() {
                    if !seen[j] && (0..3).all(|k| a.mm[k].abs_diff(b.mm[k]) <= 10_000) {
                        seen[j] = true;
                        pending.push(j);
                    }
                }
            }
            group.sort_unstable();
            expected.push(group);
        }
        for group in &mut actual {
            group.sort_unstable();
        }
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected, "incorrect physical island membership");
    }

    // Independent exhaustive search, outside timings. Boundaries come from the game plugin:
    // two player boxes of 5 m half-width and 1 m retention. Check installed ECS data as well.
    fn check_views(&self) -> Result<()> {
        for &w in &self.workers {
            let world = self.pool.app(w)?.world();
            let state = world.resource::<MeshState>();
            assert!(state.diagnostics.is_empty(), "{:?}", state.diagnostics);
            let local: Vec<_> = self.members.iter().filter(|p| p.owner == w).collect();
            for m in &self.members {
                let e = state.entities.get(&m.id).copied();
                if m.owner == w {
                    assert_eq!(e, Some(m.entity));
                    assert!(world.get::<Simulated>(m.entity).is_some());
                    assert_eq!(world.get::<Position>(m.entity).unwrap().mm, m.mm);
                    let inventory = &world.get::<Inventory>(m.entity).unwrap().0;
                    assert_eq!(inventory.len(), 1);
                    assert_eq!(inventory.get(&STONE), Some(&9));
                    assert_eq!(state.authority(m.id).unwrap().generation, 1);
                    continue;
                }
                let distance = local
                    .iter()
                    .map(|p| (0..3).map(|k| p.mm[k].abs_diff(m.mm[k])).max().unwrap())
                    .min()
                    .unwrap();
                let subscribed = self
                    .pool
                    .directory
                    .subscribers(m.id)
                    .any(|(&r, _)| r == Recipient::Worker(w));
                assert_eq!(
                    subscribed,
                    e.is_some(),
                    "subscription disagrees with installed replica"
                );
                if distance <= 10_000 {
                    assert!(subscribed, "near player missing");
                }
                if distance > 11_000 {
                    assert!(!subscribed, "distant player leaked");
                }
                if let Some(e) = e {
                    assert!(
                        world.get::<Replica>(e).is_some() && world.get::<Simulated>(e).is_none()
                    );
                    assert_eq!(world.get::<Position>(e).unwrap().mm, m.mm);
                    assert!(world.get::<Inventory>(e).is_none());
                }
            }
        }
        Ok(())
    }

    fn segment(
        &mut self,
        name: &str,
        ticks: usize,
        path: impl Fn(usize, &Member) -> [i64; 3],
    ) -> Result<Value> {
        let mut s = Sample::default();
        for t in 1..=ticks {
            self.tick(&mut s, |m| path(t, m))?;
        }
        let percentiles = |samples: &mut [u64]| {
            samples.sort_unstable();
            let ms = |p: usize| samples[(ticks * p).div_ceil(100) - 1] as f64 / 1e6;
            json!({"p50":ms(50),"p95":ms(95),"p99":ms(99),"max":ms(100)})
        };
        let (subs, pairs) = self.pool.routing();
        // A worker serving several origins has no single destination island for this counter.
        let sites = self.members.len() / self.per_site;
        let foreign_home_frames = (self.workers.len() == 2 * sites).then_some(s.foreign_origin);
        let mean_ms = |ns: u64| ns as f64 / ticks as f64 / 1e6;
        let topology_mean = mean_ms(s.topology_ns.iter().sum());
        Ok(
            json!({"phase":name,"ticks":ticks,"players":self.members.len(),"players_per_origin":self.per_site,
            "workers":self.workers.len(),"end_islands":self.pool.directory.islands().len(),"peak_islands":s.peak_islands,
            "end_pairs":pairs,"peak_pairs":s.peak_pairs,"end_subscriptions":subs,"queue_peak":s.queue,
            "step_ms":percentiles(&mut s.ns),"topology_query_ms":percentiles(&mut s.topology_ns),
            "step_with_topology_ms":percentiles(&mut s.with_topology_ns),
            "phase_mean_ms":{"simulate_publish":mean_ms(s.simulate_publish_ns),
                "delivery_ack":mean_ms(s.delivery_ns),"topology":topology_mean},
            "frames":s.frames,"new_baselines":s.baselines,"first_worker_frames":s.observer,
            "foreign_home_frames":foreign_home_frames,"field_bytes":s.fields,"spatial_candidates":s.candidates}),
        )
    }
}

fn travel(a: [i64; 3], b: [i64; 3], t: usize) -> [i64; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t as i64 / TRAVEL as i64)
}

// Same actors, writers, update rates and worker worlds. Only the positions change.
fn compare_layouts(ticks: usize) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for players in [16, 32, 64, 128, 256, 384, 512, 768, 1024] {
        let mut scene = Scene::new(4, players / 4, 8)?;
        let separate = scene.segment("separated", ticks, |_, m| site(m.origin))?;
        for _ in 0..6 {
            scene.tick(&mut Sample::default(), |_| site(0))?;
        }
        let dense = scene.segment("dense", ticks, |_, _| site(0))?;
        assert_eq!(separate["end_islands"], 4);
        assert_eq!(dense["end_islands"], 1);
        assert_eq!(separate["foreign_home_frames"], 0);
        assert_eq!(separate["frames"], players * ticks);
        assert_eq!(dense["frames"], 7 * players * ticks);
        let over_budget = dense["step_with_topology_ms"]["p95"].as_f64().unwrap() >= 50.0;
        rows.push(json!({"players":players, "separate":separate, "dense":dense}));
        if players >= 256 && over_budget {
            break;
        }
    }
    Ok(rows)
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| invalid("supply output JSON path"))?;
    let quick = match args.next() {
        None => false,
        Some(arg) if arg == "--quick" => true,
        _ => return Err(invalid("expected an optional --quick")),
    };
    if args.next().is_some() {
        return Err(invalid("unexpected benchmark argument"));
    }
    let separated_ticks = if quick { 32 } else { 120 };
    let density_ticks = if quick { 32 } else { 100 };
    let mut independent = vec![];
    let mut first_worker_frames = None;
    for count in [1, 2, 4, 8] {
        let mut scene = Scene::new(count, PER_SITE, count * 2)?;
        let r = scene.segment("separated", separated_ticks, |_, m| site(m.origin))?;
        assert_eq!(r["end_islands"], count);
        assert_eq!(r["end_pairs"], count * 2);
        assert_eq!(r["foreign_home_frames"], 0);
        assert_eq!(r["new_baselines"], 0);
        let first = r["first_worker_frames"].as_u64().unwrap();
        assert!(first > 0);
        assert_eq!(*first_worker_frames.get_or_insert(first), first);
        independent.push(r);
    }
    // Strong scaling at fixed total load, with workers shared by distant islands.
    let mut shared = vec![];
    for workers in [1, 2, 4, 8] {
        let mut scene = Scene::new(4, PER_SITE, workers)?;
        let row = scene.segment("shared-workers", separated_ticks, |_, m| site(m.origin))?;
        assert_eq!(row["end_islands"], 4);
        shared.push(row);
    }
    let mut density = vec![];
    for players in [16, 32, 64, 128, 256] {
        let mut scene = Scene::new(1, players, 2)?;
        let row = scene.segment("dense", density_ticks, |_, _| site(0))?;
        assert_eq!(row["end_islands"], 1);
        assert_eq!(row["end_subscriptions"], players);
        assert_eq!(row["new_baselines"], 0);
        density.push(row);
    }
    let mut merge = vec![];
    let mut scene = Scene::new(4, PER_SITE, 8)?;
    let mut centers = (0..4).map(site).collect::<Vec<_>>();
    for (i, joins) in [
        (0, false),
        (1, true),
        (2, true),
        (3, true),
        (3, false),
        (2, false),
        (1, false),
    ] {
        if i != 0 {
            let from = centers[i];
            let to = if joins { site(0) } else { site(i) };
            merge.push(scene.segment(
                if joins { "approach" } else { "depart" },
                TRAVEL,
                |t, m| {
                    if m.origin == i {
                        travel(from, to, t)
                    } else {
                        centers[m.origin]
                    }
                },
            )?);
            centers[i] = to;
        }
        let count = centers
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let plateau = scene.segment("hold", HOLD, |_, m| centers[m.origin])?;
        assert_eq!(plateau["end_islands"], count);
        assert_eq!(plateau["new_baselines"], 0);
        merge.push(plateau);
    }
    let mut switches = vec![];
    let mut scene = Scene::new(2, 32, 4)?;
    for travelers in [2, 4, 8, 16, 32] {
        for outbound in [true, false] {
            let mut row = scene.segment(
                if outbound { "exchange" } else { "return" },
                TRAVEL,
                |t, m| {
                    if m.index >= travelers {
                        return site(m.origin);
                    }
                    let (a, b) = (site(m.origin), site(1 - m.origin));
                    if outbound {
                        travel(a, b, t)
                    } else {
                        travel(b, a, t)
                    }
                },
            )?;
            assert_eq!(row["end_islands"], 2);
            row["players_switching"] = json!(travelers * 2);
            switches.push(row);
        }
    }
    let mut report = json!({"version":1,"os":std::env::consts::OS,
        "arch":std::env::consts::ARCH,"per_site":PER_SITE,"separation_mm":DISTANCE,
        "warmup_ticks":6,"delivery_queue_limit":QUEUE,"independent":independent,"shared_workers":shared,"density":density,
        "merge_split":merge,"exchange":switches});
    if quick {
        report["matched_layouts"] = json!(compare_layouts(separated_ticks)?);
        let mut rows = Vec::new();
        for sites in [1, 4, 16, 32] {
            let mut scene = Scene::new(sites, 32, sites * 2)?;
            let row = scene.segment("separated", 16, |_, m| site(m.origin))?;
            assert_eq!(row["end_islands"], sites);
            assert_eq!(row["end_pairs"], sites * 2);
            assert_eq!(row["frames"], sites * 32 * 16);
            assert_eq!(row["first_worker_frames"], 16 * 16);
            assert_eq!(row["foreign_home_frames"], 0);
            assert_eq!(row["new_baselines"], 0);
            rows.push(row);
        }
        report["island_scaling"] = json!(rows);
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&report).map_err(|e| invalid(e.to_string()))?,
    )?;
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
