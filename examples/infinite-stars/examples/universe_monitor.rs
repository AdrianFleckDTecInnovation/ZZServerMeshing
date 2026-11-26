//! Opt-in worker for the local live monitor. Each OS process runs one game world.
use bevy::prelude::{Entity, World};
use infinite_stars::{
    host::{identity, init_tls, provision_worker, service, worker_name},
    world, Player, Position, PLANET_ID,
};
use is_mesh::{
    wire::{Client, Listener, Role},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    net::{Ipv4Addr, UdpSocket},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const WORKERS: u32 = 4;
const HOMES: [[f64; 2]; 6] = [
    [-43., -17.],
    [-8., -29.],
    [36., -23.],
    [-38., 22.],
    [0., 18.],
    [45., 16.],
];
// The western pair joins W1 and W2. The eastern pair joins W2, W3 and W4.
const OWNERS: [[u32; 2]; 6] = [[1, 1], [2, 2], [1, 3], [4, 4], [2, 3], [3, 4]];
const STEP: Duration = Duration::from_millis(50);
const SAMPLE: Duration = Duration::from_millis(250);

#[derive(Deserialize)]
struct Config {
    ports: Vec<u16>,
    cycle_seconds: u32,
}

fn progress(t: f64) -> (usize, f64) {
    if t < 0.20 {
        (0, 0.)
    } else if t < 0.46 {
        (1, (t - 0.20) / 0.26)
    } else if t < 0.65 {
        (2, 1.)
    } else if t < 0.83 {
        (3, 1. - (t - 0.65) / 0.18)
    } else {
        (4, 0.)
    }
}

fn position(i: usize, players: usize, seconds: f64, approach: f64, cycle: f64) -> [i64; 3] {
    let home = i % HOMES.len();
    let [x, z] = HOMES[home];
    let (target, factor) = match home {
        // One W4 player visits the W2/W3 island and returns with its authority intact.
        3 if i == players - 3 && cycle >= 0.83 => {
            let away = ((cycle - 0.83) / 0.055)
                .min((1. - cycle) / 0.055)
                .clamp(0., 1.);
            (HOMES[4], away)
        }
        0 | 1 => ([-25., -22.], (approach * 1.24).min(1.)),
        4 | 5 => ([23., 17.], ((approach - 0.17) / 0.83).clamp(0., 1.)),
        _ => ([x, z], 0.),
    };
    let eased = factor * factor * (3. - 2. * factor);
    let rank = i / HOMES.len();
    let members = (players / HOMES.len()) as f64;
    let spread = (members / 12.).sqrt().min(1.);
    let radius = spread * (1.4 + 7.2 * ((rank as f64 + 0.5) / members).sqrt());
    let angle = rank as f64 * 2.39996 + home as f64 * 0.71 + seconds * 0.095;
    let x = x + (target[0] - x) * eased + radius * angle.cos();
    let z = z + (target[1] - z) * eased + radius * 0.79 * angle.sin();
    [(x * 1000.).round() as i64, 0, (z * 1000.).round() as i64]
}

fn period(id: u32) -> u32 {
    if id % 4 == 0 {
        2
    } else {
        1
    }
}

fn number(id: Id) -> u32 {
    u128::from_be_bytes(id.0) as u32
}

#[derive(Serialize)]
struct Member {
    id: u32,
    worker: u32,
    cluster: u32,
    pos: [f64; 3],
    half: f64,
    period: u32,
}

#[derive(Serialize)]
struct Topology {
    players: Vec<Member>,
    routes: BTreeSet<(u32, u32)>,
}

// Copy just the values needed for the chart while the broker is idle. JSON is
// written only after releasing the broker lock. Pooling by worker would join
// distant islands incorrectly. Use the exact connected components of the broker.
fn topology(index: &Directory) -> Topology {
    let mut players = Vec::new();
    let mut routes = BTreeSet::new();
    for group in index.island_groups() {
        let Some(cluster) = group.iter().copied().map(number).min() else {
            continue;
        };
        for &object in group {
            let Some(p) = index.presence(object) else {
                continue;
            };
            for (&to, _) in index.subscribers(object) {
                if let Recipient::Worker(target) = to {
                    routes.insert((p.owner.slot, target.slot));
                }
            }
            let id = number(object);
            players.push(Member {
                id,
                worker: p.owner.slot,
                cluster,
                period: period(id),
                pos: [0, 1, 2].map(|k| (p.bounds.lo[k] + p.bounds.hi[k]) as f64 / 2000.),
                half: (p.bounds.hi[0] - p.bounds.lo[0]) as f64 / 2000.,
            });
        }
    }
    Topology { players, routes }
}

struct Actor {
    id: u32,
    entity: Entity,
    port: u16,
}

// The observer datagram contains only public positions already present in this
// worker's world. A remote player can appear only after authenticated peer delivery.
struct Feeds {
    socket: UdpSocket,
    visible: Vec<(u32, Box3)>,
    packet: Vec<u8>,
    drops: u64,
}

impl Feeds {
    fn new() -> Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            visible: vec![],
            packet: vec![],
            drops: 0,
        })
    }

    fn send(&mut self, w: &World, actors: &[Actor], tick: u64) -> Result<()> {
        self.visible.clear();
        for (&id, &e) in &w.resource::<MeshState>().entities {
            if w.get::<Simulated>(e).is_none() && w.get::<Replica>(e).is_none() {
                continue;
            }
            if let Some(pos) = w.get::<Position>(e) {
                self.visible
                    .push((number(id), Box3::new(pos.frame, pos.mm, pos.mm)?));
            }
        }
        let spatial = w.resource::<SpatialRule>();
        for actor in actors {
            if tick % u64::from(period(actor.id)) != 0 {
                continue;
            }
            let view = spatial.0(w, actor.entity).ok_or_else(|| invalid("missing player area"))?;
            let bytes = &mut self.packet;
            bytes.clear();
            bytes.extend_from_slice(b"MSO1");
            bytes.extend_from_slice(&tick.to_le_bytes());
            bytes.extend_from_slice(&(actor.id as u16).to_le_bytes());
            bytes.extend_from_slice(&[0, 0]);
            for &(object, point) in &self.visible {
                if view.bounds.intersects(point) {
                    bytes.extend_from_slice(&object.to_le_bytes());
                    for v in point.lo {
                        bytes.extend_from_slice(&(v as i32).to_le_bytes());
                    }
                }
            }
            let records = ((bytes.len() - 16) / 16) as u16;
            bytes[14..16].copy_from_slice(&records.to_le_bytes());
            if records == 0 {
                continue;
            }
            match self
                .socket
                .send_to(bytes, (Ipv4Addr::LOCALHOST, actor.port))
            {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.drops += 1,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

fn emit(value: serde_json::Value) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()?;
    Ok(())
}

// The current owner requests the standard fenced handoff over two authenticated
// connections. Its event loop stays available to serve freeze and activation.
fn transfer(dir: &Path, db: &Store, here: Worker, group: Id, target: u32) -> Result<()> {
    let source = db.owner(group)?;
    if source.worker != here {
        return Err(Error::Fenced);
    }
    let target = db.worker(target)?;
    let ca = fs::read(dir.join("ca.der"))?;
    let credentials = identity(dir, &worker_name(here))?;
    let mut from = Client::connect(db.address(here)?, "localhost", &ca, &credentials)?;
    let mut to = Client::connect(db.address(target)?, "localhost", &ca, &credentials)?;
    let ticket = Handoff {
        id: Id::new(),
        group,
        source,
        target,
    };
    remote::migrate(&mut from, &mut to, ticket, 16_384)?;
    Ok(())
}

fn worker(dir: &Path, slot: u32) -> Result<()> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(());
    }
    let cfg: Config = serde_json::from_str(&line).map_err(|e| invalid(e.to_string()))?;
    let count = cfg.ports.len();
    if !(12..=120).contains(&count)
        || count % 6 != 0
        || cfg.ports.contains(&0)
        || !(8..=240).contains(&cfg.cycle_seconds)
    {
        return Err(invalid("invalid observer workload"));
    }
    let db = Store::open(dir.join("world.sqlite"))?;
    let here = db.worker(slot)?;
    let mut app = world(&db, here);
    let mut actors = Vec::new();
    for (i, &port) in cfg.ports.iter().enumerate() {
        if OWNERS[i % 6][(i / 6) % 2] != slot {
            continue;
        }
        let id = Id::numbered((i + 1) as u128);
        let entity = app
            .world_mut()
            .spawn((
                Managed::new(id, id),
                Player,
                Position {
                    frame: PLANET_ID,
                    mm: position(i, count, 0., 0., 0.),
                },
            ))
            .id();
        actors.push(Actor {
            id: number(id),
            entity,
            port,
        });
    }
    let registry = db.clone();
    let ca = fs::read(dir.join("ca.der"))?;
    let cred = identity(dir, &worker_name(here))?;
    let listener = Listener::bind_resolved(
        (Ipv4Addr::LOCALHOST, 0).into(),
        &ca,
        &identity(dir, "server")?,
        move |cert| registry.authenticated_worker(cert).map(Role::Worker),
    )?;
    db.advertise(here, listener.address()?)?;
    let (endpoint, mut pump) = remote::worker(
        service(&db, here),
        app.world().resource::<BlobCache>().clone(),
    );
    let registry = db.clone();
    pump.peers = Some(network_mesh::PeerMesh::new(db.clone(), here, move |to| {
        Client::connect(registry.address(to)?, "localhost", &ca, &cred)
    }));
    let serving = endpoint.clone();
    let listener_thread = thread::spawn(move || listener.serve(serving, Default::default(), 16));
    let mut feeds = Feeds::new()?;
    emit(
        json!({"type":"ready","port":feeds.socket.local_addr()?.port(),
        "clients":actors.iter().map(|a| a.id).collect::<Vec<_>>() }),
    )?;
    // The launcher releases all four workers after every listener is bound.
    line.clear();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(());
    }
    if line.trim() != "start" {
        return Err(invalid("expected start"));
    }
    let running = Arc::new(AtomicBool::new(true));
    let control = running.clone();
    thread::spawn(move || {
        let _ = io::stdin().read_line(&mut String::new());
        control.store(false, Ordering::Release);
    });
    let start = Instant::now();
    let mut next = start;
    let mut sampled = start;
    let mut tick = 0;
    let mut steps = 0;
    let mut compute = Duration::ZERO;
    let moving_authority = Id::numbered((count - 5) as u128);
    let mut target_slot = 1;
    let mut handoff: Option<thread::JoinHandle<Result<()>>> = None;
    let mut pending_sample: Option<serde_json::Value> = None;
    app.update();
    while running.load(Ordering::Acquire) {
        if listener_thread.is_finished() {
            return Err(invalid("peer listener exited"));
        }
        pump.pump(app.world_mut(), 16, Duration::from_millis(3));
        // Directory reports arrive close to a tick boundary. Retry an outstanding
        // sample between ticks to avoid an entire lost sample on lock contention.
        if let Some(frame) = pending_sample.as_mut() {
            if let Some(state) = endpoint.try_inspect_directory(topology) {
                frame["topology"] = json!(state);
                emit(pending_sample.take().unwrap())?;
            }
        }
        let now = Instant::now();
        if now < next {
            thread::sleep((next - now).min(Duration::from_millis(2)));
            continue;
        }
        tick += 1;
        let seconds = now.duration_since(start).as_secs_f64();
        let cycle = (seconds / f64::from(cfg.cycle_seconds)).fract();
        let (phase, approach) = progress(cycle);
        if handoff.as_ref().is_some_and(|h| h.is_finished()) {
            handoff
                .take()
                .unwrap()
                .join()
                .map_err(|_| invalid("handoff thread exited"))??;
        }
        let requested = if (0.49..0.76).contains(&cycle) { 2 } else { 1 };
        if requested != target_slot {
            if slot == target_slot && handoff.is_none() {
                let db = db.clone();
                let dir = dir.to_owned();
                handoff = Some(thread::spawn(move || {
                    transfer(&dir, &db, here, moving_authority, requested)
                }));
            }
            target_slot = requested;
        }
        // Frozen and staged entities must never run gameplay. A promoted replica
        // appears here on its new owner with the same persistent player ID.
        actors.clear();
        let w = app.world();
        for (&id, &entity) in &w.resource::<MeshState>().entities {
            if w.get::<Simulated>(entity).is_some() {
                let id = number(id);
                actors.push(Actor {
                    id,
                    entity,
                    port: cfg.ports[id as usize - 1],
                });
            }
        }
        for actor in &actors {
            app.world_mut()
                .get_mut::<Position>(actor.entity)
                .ok_or_else(|| invalid("missing player"))?
                .mm = position((actor.id - 1) as usize, count, seconds, approach, cycle);
        }
        app.update();
        pump.peers.as_mut().unwrap().step(app.world_mut())?;
        let used = now.elapsed();
        pump.record_tick(used);
        steps += 1;
        compute += used;
        feeds.send(app.world(), &actors, tick)?;
        if now.duration_since(sampled) >= SAMPLE || tick == 1 {
            let w = app.world();
            let entities = &w.resource::<MeshState>().entities;
            let owned = entities
                .values()
                .filter(|&&e| w.get::<Simulated>(e).is_some())
                .count();
            let replicas = entities
                .values()
                .filter(|&&e| w.get::<Replica>(e).is_some())
                .count();
            let p = pump.peers.as_ref().unwrap();
            let frame = json!({
                "type": "sample", "id": slot, "pid": std::process::id(),
                "owned": owned, "replicas": replicas, "tick": tick, "seconds": seconds,
                "phase": phase, "cycle": cycle, "sent": p.frames_out, "applied": p.frames_in,
                "stepMs": compute.as_secs_f64() * 1000. / f64::from(steps),
                "drops": feeds.drops, "traveler": count-2, "handoffPlayer": number(moving_authority),
            });
            if slot == 1 {
                pending_sample = Some(frame);
            } else {
                emit(frame)?;
            }
            steps = 0;
            compute = Duration::ZERO;
            sampled = now;
        }
        next = (next + STEP).max(Instant::now());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag, dir] if flag == "--init" => {
            let dir = Path::new(dir);
            init_tls(dir)?;
            let db = Store::open(dir.join("world.sqlite"))?;
            for slot in 1..=WORKERS {
                provision_worker(dir, &db, db.start_worker(slot)?)?;
            }
            Ok(())
        }
        [dir, slot] => {
            let slot: u32 = slot
                .to_str()
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=WORKERS).contains(n))
                .ok_or_else(|| invalid("invalid worker slot"))?;
            worker(Path::new(dir), slot)
        }
        _ => Err(invalid("run npm run observe from the repository root")),
    }
}
