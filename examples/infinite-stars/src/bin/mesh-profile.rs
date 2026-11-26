//! Profile moving, session-bound players against the local authoritative mesh.
#[path = "mesh-profile/gameplay.rs"]
mod gameplay;
use bevy::prelude::Entity;
use infinite_stars::{movement::Drive, world, Inventory, Player, Position, PLANET_ID};
use serde_json::{json, Value};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::BTreeSet,
    time::{Duration, Instant},
};
use is_mesh::*;

struct Meter;
thread_local! {
    static ALLOCATION: Cell<(u64,u64)> = const { Cell::new((0,0)) };
}
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation(System.alloc(layout), layout.size())
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation(System.alloc_zeroed(layout), layout.size())
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation(System.realloc(ptr, layout, size), size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}
// Forward resize and zeroing to the native allocator so measuring a Vec's growth
// does not force an otherwise avoidable allocation and copy.
fn count_allocation(ptr: *mut u8, size: usize) -> *mut u8 {
    if !ptr.is_null() {
        let _ = ALLOCATION.try_with(|meter| {
            let (calls, bytes) = meter.get();
            meter.set((calls + 1, bytes + size as u64));
        });
    }
    ptr
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

const STAGES: [&str; 8] = [
    "input",
    "simulation_checkpoint",
    "spatial",
    "routes",
    "publish",
    "placement",
    "delivery_ack",
    "islands",
];
#[derive(Clone, Copy, Default)]
struct Cost {
    time: Duration,
    allocations: u64,
    bytes: u64,
}
struct Timer(Instant, u64, u64);
impl Timer {
    fn start() -> Self {
        let (calls, bytes) = ALLOCATION.with(Cell::get);
        Self(Instant::now(), calls, bytes)
    }
    fn record(&mut self, cost: &mut Cost) {
        let next = Self::start();
        cost.time += next.0 - self.0;
        cost.allocations += next.1 - self.1;
        cost.bytes += next.2 - self.2;
        *self = next;
    }
}
fn quantiles(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let q = |p: usize| values[(values.len() * p).div_ceil(100) - 1];
    json!({"p50":q(50), "p95":q(95), "p99":q(99), "max":values.last()})
}
struct Actor {
    id: Id,
    session: Id,
    owner: Worker,
    peer: Worker,
    entity: Entity,
    start: [i64; 3],
    inputs: u64,
    moving: bool,
}
#[derive(Clone)]
struct Options {
    players: usize,
    workers: usize,
    island_size: usize,
    ticks: usize,
    partitions: usize,
    cpu_budget: Option<usize>,
    active_percent: usize,
    effects: gameplay::Settings,
    require_20hz: bool,
    database: String,
    output: Option<String>,
}
impl Options {
    fn read() -> Result<Self> {
        let mut o = Self {
            players: 10_000,
            workers: 32,
            island_size: 16,
            ticks: 60,
            partitions: 1,
            cpu_budget: None,
            active_percent: 100,
            effects: gameplay::Settings {
                count: 0,
                interval: 1,
                local_only: false,
            },
            require_20hz: false,
            database: ":memory:".into(),
            output: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            if key == "--require-20hz" {
                o.require_20hz = true;
                continue;
            }
            if key == "--local-effects" {
                o.effects.local_only = true;
                continue;
            }
            let v = args
                .next()
                .ok_or_else(|| invalid(format!("missing value for {key}")))?;
            let number = || {
                v.parse::<usize>()
                    .map_err(|_| invalid(format!("invalid {key}")))
            };
            match key.as_str() {
                "--players" => o.players = number()?,
                "--workers" => o.workers = number()?,
                "--island-size" => o.island_size = number()?,
                "--ticks" => o.ticks = number()?,
                "--partitions" => o.partitions = number()?,
                "--cpu-budget" => o.cpu_budget = Some(number()?),
                "--active-percent" => o.active_percent = number()?,
                "--status-effects" => o.effects.count = number()?,
                "--effect-interval" => {
                    o.effects.interval =
                        v.parse().map_err(|_| invalid("invalid effect interval"))?
                }
                "--database" => o.database = v,
                "--output" => o.output = Some(v),
                _ => return Err(invalid(format!("unknown option {key}"))),
            }
        }
        if !(2..=65_536).contains(&o.players)
            || !(2..=128).contains(&o.island_size)
            || o.players % o.island_size != 0
            || !(2..=256).contains(&o.workers)
            || o.workers % 2 != 0
            || !(20..=10_000).contains(&o.ticks)
        {
            return Err(invalid("require 2..65536 players in complete islands of 2..128, an even worker count in 2..256, and 20..10000 ticks"));
        }
        if o.database != ":memory:" && std::path::Path::new(&o.database).exists() {
            return Err(invalid("benchmark database path already exists"));
        }
        if o.active_percent > 100 {
            return Err(invalid("active percent must be between 0 and 100"));
        }
        if !(1..=64).contains(&o.effects.interval)
            || o.effects.count > 4096
            || o.players.saturating_mul(o.effects.count) > 32_000_000
            || (o.effects.count == 0 && (o.effects.local_only || o.effects.interval != 1))
        {
            return Err(invalid("use 1..4096 status effects, up to 32 million total, with an optional interval of 1..64 ticks or --local-effects"));
        }
        if o.partitions == 0
            || o.partitions > o.workers / 2
            || o.workers % (2 * o.partitions) != 0
            || o.players / o.island_size < o.partitions
        {
            return Err(invalid(
                "each partition needs at least one island and an even share of the workers",
            ));
        }
        if o.cpu_budget.is_some_and(|n| {
            n < o.partitions || n > std::thread::available_parallelism().map_or(1, |p| p.get())
        }) {
            return Err(invalid(
                "cpu budget must cover the requested partitions and fit the available CPUs",
            ));
        }
        Ok(o)
    }
}

struct Clock {
    go: std::sync::mpsc::Receiver<()>,
    done: std::sync::mpsc::Sender<()>,
}
impl Clock {
    fn ready(&self) -> Result<()> {
        self.done
            .send(())
            .map_err(|_| invalid("benchmark controller closed"))
    }
    fn start(&self) -> Result<()> {
        self.go.recv().map_err(|_| invalid("benchmark stopped"))
    }
}
fn lane(
    o: &Options,
    db: Store,
    partition: usize,
    first_player: usize,
    clock: Option<Clock>,
    background: BulkExecutor,
) -> Result<Value> {
    let mut pool = Pool::with_bulk_executor(db.clone(), o.players * 2, background);
    // Measure stable placement. Every island straddles two authorities.
    pool.policy.every = 0;
    let mut workers = Vec::new();
    for slot in 1..=o.workers {
        let w = db.start_worker((partition * o.workers + slot) as u32)?;
        let mut app = world(&db, w);
        gameplay::install(&mut app, o.effects)?;
        pool.add(w, app)?;
        workers.push(w);
    }
    let mut actors = Vec::new();
    for i in 0..o.players {
        let site = i / o.island_size;
        let lane = (site * 2 + i % 2) % o.workers;
        let owner = workers[lane];
        let id = Id::numbered(1_000_000 + (first_player + i) as u128);
        let group =
            Id::numbered(2_000_000 + ((first_player / o.island_size + site) * 2 + i % 2) as u128);
        let start = [site as i64 * 128_000, (i % o.island_size) as i64 * 32, 0];
        let entity = pool
            .app_mut(owner)?
            .world_mut()
            .spawn((
                Managed::new(id, group),
                Player,
                Position {
                    frame: if partition == 0 {
                        PLANET_ID
                    } else {
                        Id::numbered(4_000_000 + partition as u128)
                    },
                    mm: start,
                },
                Drive::default(),
                Inventory::default(),
            ))
            .id();
        gameplay::attach(
            pool.app_mut(owner)?.world_mut(),
            entity,
            first_player + i,
            o.effects,
        );
        let session = Id::numbered(3_000_000 + (first_player + i) as u128);
        db.session(session, id)?;
        actors.push(Actor {
            id,
            session,
            owner,
            peer: workers[lane ^ 1],
            entity,
            start,
            inputs: 0,
            moving: (first_player + i) * 37 % 100 < o.active_percent,
        });
    }
    pool.step()?;
    pool.deliver_all()?;
    if let Some(c) = &clock {
        c.ready()?;
    }
    let mut samples = Vec::with_capacity(o.ticks);
    let mut tick_ms = Vec::with_capacity(o.ticks);
    let mut frames = 0;
    let mut acks = 0;
    let mut candidate_checks = 0;
    let mut deadline_misses = 0;
    let mut runtime = Duration::ZERO;
    let mut cpu_ns = 0;
    let mut cpu_cycles = 0;
    let moving = actors.iter().filter(|a| a.moving).count();
    let mut projections = 0;
    let mut warm_gameplay = (Duration::ZERO, 0);
    let mut ingress: Vec<Vec<(Id, Input)>> = (0..o.workers).map(|_| Vec::new()).collect();
    for tick in 0..o.ticks + 4 {
        let measured = tick >= 4;
        if let Some(c) = &clock {
            c.start()?;
        }
        let mut costs = [Cost::default(); STAGES.len()];
        let candidates = pool.directory.candidates;
        let mut timer = Timer::start();
        let begin = timer.0;
        let cpu_begin = (thread_cpu_ns(), thread_cpu_cycles());
        let now = 1000 + tick as u64 * 50;
        // Stagger four-tick movement leases so the mix includes inputs every tick.
        for a in actors
            .iter_mut()
            .skip(tick % 4)
            .step_by(4)
            .filter(|a| a.moving)
        {
            a.inputs += 1;
            // With stationary neighbors, turn every sixteen steps to stay in the island.
            let reverse = o.active_percent < 100 && ((a.inputs - 1) / 4) % 2 == 1;
            let input = Input {
                session: a.session,
                sequence: a.inputs,
                at_ms: now,
                axes: [0, 0, if reverse { -32767 } else { 32767 }],
            };
            let lane = (a.owner.slot as usize - 1) % o.workers;
            ingress[lane].push((a.id, input));
        }
        for (&worker, inputs) in workers.iter().zip(&mut ingress) {
            for chunk in inputs.chunks(MAX_TARGETS) {
                let replies = apply_inputs(pool.app_mut(worker)?.world_mut(), chunk, now)?;
                for ((_, input), ack) in chunk.iter().zip(replies) {
                    if ack?.sequence != input.sequence {
                        return Err(invalid("input ACK mismatch"));
                    }
                    acks += usize::from(measured);
                }
            }
            inputs.clear();
        }
        timer.record(&mut costs[0]);
        pool.step_observed(|stage| {
            let k = match stage {
                TickStage::Simulation => 1,
                TickStage::Spatial => 2,
                TickStage::Routes => 3,
                TickStage::Publish => 4,
                TickStage::Placement => 5,
            };
            timer.record(&mut costs[k]);
        })?;
        let queued = pool.messages.len();
        let applied = pool.deliver_all()?;
        timer.record(&mut costs[6]);
        let islands = pool.directory.island_groups();
        timer.record(&mut costs[7]);
        let elapsed = timer.0 - begin;
        if measured {
            cpu_ns += thread_cpu_ns().saturating_sub(cpu_begin.0);
            cpu_cycles += thread_cpu_cycles().saturating_sub(cpu_begin.1);
            if applied != moving || queued != applied {
                return Err(invalid(format!(
                    "delivered {applied}/{queued} for {moving} moving players"
                )));
            }
            frames += applied;
            candidate_checks += pool.directory.candidates - candidates;
            deadline_misses += usize::from(elapsed > Duration::from_millis(50));
            tick_ms.push(elapsed.as_secs_f64() * 1000.);
            runtime += elapsed;
            samples.push(costs);
        }
        if islands.len() != o.players / o.island_size
            || islands.iter().any(|s| s.len() != o.island_size)
        {
            return Err(invalid("physical island count or membership size mismatch"));
        }
        if tick == 3 {
            projections = pool.link_stats().0;
            warm_gameplay = gameplay::totals(&pool, &workers)?;
        }
        if let Some(c) = &clock {
            c.ready()?;
        }
    }
    // Inspect actual receiver ECS components and every routing edge outside the timer.
    let mut active_workers = BTreeSet::new();
    for (i, a) in actors.iter().enumerate() {
        active_workers.insert(a.owner);
        let source = pool.app(a.owner)?.world();
        let target = pool.app(a.peer)?.world();
        let local = source
            .get::<Position>(a.entity)
            .ok_or_else(|| invalid("local position"))?;
        let remote = target
            .resource::<MeshState>()
            .entities
            .get(&a.id)
            .copied()
            .ok_or_else(|| invalid("remote entity missing"))?;
        gameplay::verify(
            source,
            a.entity,
            target,
            remote,
            (o.ticks + 5) as u32,
            o.effects,
        )?;
        let expected_z = if a.moving {
            let steps = o.ticks + 4 - i % 4;
            let travel = if o.active_percent < 100 {
                let phase = steps % 32;
                phase.min(32 - phase)
            } else {
                steps
            };
            travel as i64 * 250
        } else {
            0
        };
        if local.mm != [a.start[0], a.start[1], expected_z]
            || target.get::<Position>(remote).map(|p| p.mm) != Some(local.mm)
            || target.get::<Replica>(remote).is_none()
            || target.get::<Inventory>(remote).is_some()
            || pool
                .directory
                .subscribers(a.id)
                .map(|(to, _)| *to)
                .ne([Recipient::Worker(a.peer)])
        {
            return Err(invalid(
                "wrong receiver position, authority, private data or subscription",
            ));
        }
    }
    let (game_time, game_updates) = gameplay::totals(&pool, &workers)?;
    let game_updates = game_updates - warm_gameplay.1;
    gameplay::verify_checkpoint(&db, actors[0].id, (o.ticks + 5) as u32, o.effects)?;
    for w in workers {
        let s = pool.app(w)?.world().resource::<MeshState>();
        if !s.diagnostics.is_empty() {
            return Err(invalid(format!("worker diagnostics: {:?}", s.diagnostics)));
        }
    }
    let mut stages = serde_json::Map::new();
    for (i, name) in STAGES.iter().enumerate() {
        let total: f64 = samples.iter().map(|s| s[i].time.as_secs_f64()).sum();
        stages.insert(name.to_string(),json!({
            "ms":quantiles(samples.iter().map(|s|s[i].time.as_secs_f64()*1000.).collect()),
            "wall_percent":100.*total/runtime.as_secs_f64(),
            "rust_allocations_per_tick":samples.iter().map(|s|s[i].allocations).sum::<u64>() as f64/o.ticks as f64,
            "rust_allocated_bytes_per_tick":samples.iter().map(|s|s[i].bytes).sum::<u64>() as f64/o.ticks as f64
        }));
    }
    Ok(
        json!({"schema":1,"workload":"session inputs, Bevy movement, worker replication, checkpoints, physical islands",
        "target_os":std::env::consts::OS,"players":o.players,"workers":o.workers,"active_workers":active_workers.len(),
        "island_size":o.island_size,"islands":o.players/o.island_size,"ticks":o.ticks,"warmup_ticks":4,
        "store":if o.database==":memory:"{"memory"}else{"file"},"lane_samples_ms":clock.is_some().then(||tick_ms.as_slice()),"tick_ms":quantiles(tick_ms.clone()),
        "equivalent_hz":o.ticks as f64/runtime.as_secs_f64(),"ticks_over_50ms":deadline_misses,
        "inputs_acked":acks,"peer_updates_applied":frames,"cross_island_subscriptions":0,
        "active_percent":o.active_percent,"moving_players":moving,"component_projections":pool.link_stats().0-projections,
        "status_effects":o.effects.count,"effect_interval":o.effects.interval,"local_effects":o.effects.local_only,
        "gameplay_player_updates":game_updates,"effect_checks":game_updates*o.effects.count as u64,
        "sum_gameplay_system_ms_per_tick":(game_time-warm_gameplay.0).as_secs_f64()*1000./o.ticks as f64,
        "worker_cpu_ms_per_tick":cpu_ns as f64/(1e6*o.ticks as f64),
        "worker_cycles_per_tick":(cpu_cycles>0).then_some(cpu_cycles/o.ticks as u64),
        "octree_candidates":candidate_checks,"stages":stages}),
    )
}
fn run(o: &Options) -> Result<Value> {
    if let Some(cores) = o.cpu_budget {
        bevy::core::TaskPoolOptions::with_num_threads(cores).create_default_pools();
    }
    let db = Store::open(&o.database)?;
    let background = BulkExecutor::new(2);
    if o.partitions == 1 {
        return lane(o, db, 0, 0, None, background);
    }
    // All reference frames use the same authority, session and checkpoint store.
    // Each Bevy App stays on its owning thread. Wait for every frame on each tick.
    let mut clocks = Vec::new();
    let mut jobs = Vec::new();
    let sites = o.players / o.island_size;
    for p in 0..o.partitions {
        let (start, go) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        clocks.push((start, finished));
        let first = (p * sites / o.partitions) * o.island_size;
        let after = ((p + 1) * sites / o.partitions) * o.island_size;
        let mut local = o.clone();
        local.workers /= o.partitions;
        local.players = after - first;
        let store = db.clone();
        let background = background.clone();
        jobs.push(std::thread::spawn(move || {
            lane(
                &local,
                store,
                p,
                first,
                Some(Clock { go, done }),
                background,
            )
        }));
    }
    let mut times = Vec::new();
    let wait =
        |clocks: &[(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)]| -> Result<()> {
            for (_, r) in clocks {
                r.recv_timeout(Duration::from_secs(90))
                    .map_err(|_| invalid("partition did not complete"))?;
            }
            Ok(())
        };
    let result = (|| -> Result<()> {
        wait(&clocks)?;
        for tick in 0..o.ticks + 4 {
            let start = Timer::start();
            for (send, _) in &clocks {
                send.send(()).map_err(|_| invalid("partition stopped"))?;
            }
            wait(&clocks)?;
            let end = Timer::start();
            if tick >= 4 {
                times.push((end.0 - start.0).as_secs_f64() * 1000.);
            }
        }
        Ok(())
    })();
    drop(clocks);
    let mut lanes = Vec::new();
    for job in jobs {
        lanes.push(job.join().map_err(|_| invalid("partition panicked"))??);
    }
    result?;
    let barrier: Vec<_> = times
        .iter()
        .enumerate()
        .map(|(i, elapsed)| {
            let slowest = lanes
                .iter()
                .filter_map(|l| l["lane_samples_ms"][i].as_f64())
                .fold(0., f64::max);
            (elapsed - slowest).max(0.)
        })
        .collect();
    for lane in &mut lanes {
        lane.as_object_mut().unwrap().remove("lane_samples_ms");
    }
    let sum = |k: &str| lanes.iter().map(|v| v[k].as_u64().unwrap()).sum::<u64>();
    let stage_sum = |key: &str| {
        lanes
            .iter()
            .flat_map(|lane| {
                STAGES
                    .iter()
                    .map(move |stage| lane["stages"][stage][key].as_f64().unwrap())
            })
            .sum::<f64>()
    };
    Ok(
        json!({"schema":1,"target_os":std::env::consts::OS,"players":o.players,"workers":o.workers,
        "partitions":o.partitions,"islands":sites,"island_size":o.island_size,"ticks":o.ticks,
        "store":if o.database==":memory:"{"shared memory SQLite"}else{"shared file SQLite"},
        "inputs_acked":sum("inputs_acked"),"peer_updates_applied":sum("peer_updates_applied"),
        "moving_players":sum("moving_players"),"active_percent":o.active_percent,"component_projections":sum("component_projections"),
        "status_effects":o.effects.count,"effect_interval":o.effects.interval,"local_effects":o.effects.local_only,
        "gameplay_player_updates":sum("gameplay_player_updates"),"effect_checks":sum("effect_checks"),
        "sum_gameplay_system_ms_per_tick":lanes.iter().map(|p|p["sum_gameplay_system_ms_per_tick"].as_f64().unwrap()).sum::<f64>(),
        "cross_island_subscriptions":sum("cross_island_subscriptions"),
        "ticks_over_50ms":times.iter().filter(|&&t|t>50.).count(),
        "equivalent_hz":o.ticks as f64*1000./times.iter().sum::<f64>(),
        "tick_ms":quantiles(times),"barrier_and_checks_ms":quantiles(barrier),"rust_allocations_per_tick":stage_sum("rust_allocations_per_tick"),
        "rust_allocated_bytes_per_tick":stage_sum("rust_allocated_bytes_per_tick"),"partition_profiles":lanes}),
    )
}
fn main() -> Result<()> {
    let options = Options::read()?;
    let report = run(&options)?;
    let result = serde_json::to_string_pretty(&report).map_err(|e| invalid(e.to_string()))?;
    if let Some(path) = options.output {
        std::fs::write(path, &result)?;
    }
    println!("{result}");
    if options.require_20hz && report["ticks_over_50ms"].as_u64() != Some(0) {
        return Err(invalid("one or more measured ticks exceeded 50 ms"));
    }
    Ok(())
}
