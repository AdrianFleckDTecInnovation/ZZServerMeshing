//! Versioned, deterministic workloads for release builds on Windows and Linux.
use infinite_stars::{host::*, *};
use serde_json::{json, Value};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    hint::black_box,
    net::UdpSocket,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, Instant},
};
use is_mesh::{wire::*, *};

struct CountAlloc;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let n = l.size() as u64;
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(n, Relaxed);
            PEAK.fetch_max(LIVE.fetch_add(n, Relaxed) + n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size() as u64, Relaxed);
    }
}
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;
fn percent(mut xs: Vec<u64>) -> Value {
    xs.sort_unstable();
    let q = |p: usize| xs[(xs.len() * p).div_ceil(100).saturating_sub(1)];
    json!({"p50":q(50),"p95":q(95),"p99":q(99),"max":xs.last()})
}
fn scene(workers: usize, islands: usize, objects: usize, ticks: usize) -> Result<Value> {
    let db = Store::open(":memory:")?;
    let mut pool = Pool::new(db.clone(), 65536);
    pool.policy.every = 0;
    let mut members = vec![vec![]; islands];
    let mut nodes = vec![];
    for slot in 0..workers {
        let w = db.start_worker(slot as u32 + 1)?;
        nodes.push(w);
        pool.add(w, world(&db, w))?;
    }
    for i in 0..objects {
        let island = i % islands;
        let index = i / islands;
        let slot = if islands == 1 {
            i % workers
        } else {
            island % workers
        };
        let w = nodes[slot];
        let obj = Id::numbered(1000 + (island * 100000 + index) as u128);
        let group = Id::numbered(100 + slot as u128);
        let p = Position {
            frame: PLANET_ID,
            mm: [
                island as i64 * 1_000_000 + (index % 12) as i64 * 800,
                (index % 9) as i64 * 800,
                (index % 13) as i64 * 800,
            ],
        };
        let mut e = pool
            .app_mut(w)?
            .world_mut()
            .spawn((Managed::new(obj, group), p));
        if index == 0 {
            e.insert(Player);
        }
        members[island].push((w, e.id()));
    }
    let client = Id::numbered(700);
    for i in 0..islands {
        let x = i as i64 * 1_000_000;
        pool.directory.register_area(Area {
            id: Id::numbered(600 + i as u128),
            worker: nodes[i % workers],
            to: Recipient::Client(if i == 0 {
                client
            } else {
                Id::numbered(700 + i as u128)
            }),
            bounds: Box3::new(
                PLANET_ID,
                [x - 20000, -20000, -20000],
                [x + 20000, 20000, 20000],
            )?,
            velocity: [0; 3],
            ahead: 2,
            hysteresis: 500,
            activity: true,
            components: BTreeMap::from([(
                POSITION,
                Demand {
                    detail: Detail::Coarse,
                    period: 1,
                },
            )]),
        })?;
    }
    for _ in 0..6 {
        pool.step()?;
        pool.deliver_all()?;
    }
    let (a0, b0, l0) = (
        ALLOCS.load(Relaxed),
        BYTES.load(Relaxed),
        LIVE.load(Relaxed),
    );
    PEAK.store(l0, Relaxed);
    let mut times = Vec::with_capacity(ticks);
    let (mut peak_queue, mut first, mut sum_payload, mut first_payload, mut retries) =
        (0, 0, 0, 0, 0);
    let candidates = pool.directory.candidates;
    let mut delivered = BTreeMap::new();
    for tick in 0..ticks {
        let start = Instant::now();
        for entries in &members {
            for j in 0..4 {
                let (w, e) = entries[(tick * 4 + j) % entries.len()];
                let mut p = pool.app_mut(w)?.world_mut().get_mut::<Position>(e).unwrap();
                p.mm[2] += if tick / entries.len() % 2 == 0 {
                    100
                } else {
                    -100
                };
            }
        }
        pool.step()?;
        peak_queue = peak_queue.max(pool.messages.len());
        while let Some(m) = pool.messages.pop_front() {
            // Count typed payload separately from the direct path's zero socket bytes.
            let size = m
                .update
                .fields
                .values()
                .flatten()
                .map(|f| f.bytes.len())
                .sum::<usize>();
            sum_payload += size;
            if m.to == Recipient::Client(client) {
                first += 1;
                first_payload += size
            }
            let k = (m.to, m.update.object, m.update.epoch);
            if delivered.insert(k, m.update.serial) == Some(m.update.serial) {
                retries += 1;
            }
            // Deterministically drop first-island ACKs, independent of worker count.
            let ack = (tick + usize::from(m.update.object.0[15])) % 11 != 0;
            pool.deliver_with_ack(m, ack)?;
        }
        times.push(start.elapsed().as_nanos() as u64);
    }
    let allocated = ALLOCS.load(Relaxed) - a0;
    let bytes = BYTES.load(Relaxed) - b0;
    let peak = PEAK.load(Relaxed).saturating_sub(l0);
    let (subs, peers) = pool.routing();
    let (encodes, scheduled) = pool.link_stats();
    Ok(
        json!({"workers":workers,"physical_islands":pool.directory.islands().len(),"objects":objects,"ticks":ticks,
        "loop_ns":percent(times),"socket_bytes":0,"field_payload_bytes":sum_payload,"first_island_frames":first,"first_island_payload":first_payload,
        "repeated_frames":retries,"exact_spatial_candidates":pool.directory.candidates-candidates,"active_subscriptions":subs,"directed_peer_pairs":peers,
        "projected_fields_since_start":encodes,"scheduled_edge_peak_sum":scheduled,"delivery_queue_peak":peak_queue,"allocations":allocated,
        "allocated_bytes":bytes,"additional_live_heap_peak":peak}),
    )
}
fn codecs() -> Result<Value> {
    let base = vec![0u8; 4096];
    let mut cases = vec![];
    for stride in [4095, 256, 16, 4, 1] {
        let mut next = base.clone();
        for i in (0..4096).step_by(stride) {
            next[i] = (1 + (i / 16) % 7) as u8;
        }
        let mut formats = vec![];
        for v in VoxelWrites::alternatives(&base, &next)? {
            let name = match &v {
                VoxelWrites::Indices(_) => "sorted",
                VoxelWrites::Runs(_) => "ranges",
                _ => "palette_and_mask",
            };
            let bytes = encode(&v)?;
            let text = serde_json::to_vec(&v).unwrap();
            let iterations = 512;
            let mut rows = vec![];
            for binary in [true, false] {
                let encoded = if binary { &bytes } else { &text };
                let t = Instant::now();
                let c = thread_cpu_ns();
                for _ in 0..iterations {
                    if binary {
                        black_box(encode(black_box(&v))?);
                    } else {
                        black_box(serde_json::to_vec(black_box(&v)).unwrap());
                    }
                }
                let enc_cpu = thread_cpu_ns() - c;
                let enc = t.elapsed();
                let d = Instant::now();
                let c = thread_cpu_ns();
                for _ in 0..iterations {
                    let v: VoxelWrites = if binary {
                        decode(black_box(encoded))?
                    } else {
                        serde_json::from_slice(black_box(encoded)).unwrap()
                    };
                    black_box(v);
                }
                rows.push(json!({"codec":if binary{"bincode-le-varint"}else{"json"},"bytes":encoded.len(),"iterations":iterations,
                    "encode_thread_cpu_ns":enc_cpu,"decode_thread_cpu_ns":thread_cpu_ns()-c,"encode_elapsed_ns":enc.as_nanos(),"decode_elapsed_ns":d.elapsed().as_nanos()}));
            }
            formats.push(json!({"format":name,"codecs":rows}));
        }
        cases.push(json!({"changed":next.iter().filter(|x|**x!=0).count(),"formats":formats}));
    }
    Ok(json!(cases))
}
fn migration(path: &Path) -> Result<Value> {
    let db = Store::open(path.join("migration.sqlite"))?;
    let (a, src) = start_node(&db, 1)?;
    let b = db.start_worker(2)?;
    let mut pool = Pool::new(db.clone(), 1024);
    pool.add(a, src)?;
    pool.add(b, world(&db, b))?;
    pool.step()?;
    let mut rows = vec![];
    for to in [b, a, b, a, b, a] {
        let c = pool.transfer(GROUP, to, 16384)?;
        rows.push(json!({"target":to.slot,"bulk_payload":c.payload,"preparation_ns":c.prepare.as_nanos(),"pause_ns":c.pause.as_nanos(),
            "cache_hits":pool.app(to)?.world().resource::<BlobCache>().hits}));
    }
    Ok(json!(rows))
}
fn transports(dir: &Path) -> Result<Value> {
    init_tls(dir)?;
    let db = Store::open(dir.join("transport.sqlite"))?;
    let (w, app) = start_node(&db, 1)?;
    let owner = db.owner(GROUP)?;
    let Lookup::Present(mut actor) = db.lookup(ACTOR)? else {
        return Err(Error::Missing("actor".into()));
    };
    actor.put(INVENTORY, 1, &Inventory(BTreeMap::from([(STONE, 10000)])))?;
    db.checkpoint(GROUP, owner, db.snapshot(GROUP)?.revision, vec![actor])?;
    drop(app);
    let service = service(&db, w);
    let role = Role::Player(ACTOR);
    provision_worker(dir, &db, w)?;
    let peer = identity(dir, &worker_name(w))?;
    let player = identity(dir, "player")?;
    let ca = std::fs::read(dir.join("ca.der"))?;
    let listener = Listener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &ca,
        &identity(dir, "server")?,
        BTreeMap::from([
            (digest(&player.cert), role),
            (digest(&peer.cert), Role::Worker(w)),
        ]),
    )?;
    let addr = listener.address()?;
    let server_traffic = listener.traffic.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (s, sv) = (stop.clone(), service.clone());
    let listener = std::thread::spawn(move || listener.serve(sv, s, 8));
    let mut client = Client::connect(addr, "localhost", &ca, &player)?;
    let mut modes = vec![];
    for network in [false, true] {
        let session = Id::new();
        let mut times = vec![];
        let mut copies = 0;
        for batch in 0..24 {
            let make = |seq| {
                Intent::typed(
                    ActionKey { session, seq },
                    GROUP,
                    &DropItems(vec![
                        Stack {
                            kind: STONE,
                            count: 1
                        };
                        4
                    ]),
                )
            };
            let items = (1..=8)
                .map(|n| make(batch * 8 + n))
                .collect::<Result<Vec<_>>>()?;
            let req = Request::Actions(items);
            let repeat = if batch % 8 == 0 { 2 } else { 1 };
            for _ in 0..repeat {
                let t = Instant::now();
                let response = if network {
                    client.call(&req)?
                } else {
                    direct(service.as_ref(), role, req.clone())?
                };
                let Response::Actions(replies) = response else {
                    return Err(invalid("benchmark action failed"));
                };
                if replies.iter().any(|r| {
                    !matches!(
                        r,
                        Reply::Decided(Receipt {
                            outcome: Outcome::Committed(_),
                            ..
                        })
                    )
                }) {
                    return Err(invalid("benchmark actions rejected"));
                }
                times.push(t.elapsed().as_nanos() as u64);
                copies += 1;
            }
        }
        modes.push(json!({"path":if network{"tcp_mtls"}else{"direct"},"rpc_calls":copies,"distinct_actions":192,"targets":768,
            "roundtrip_ns":percent(times),"socket_tx":client.traffic.sent.load(Relaxed),"socket_rx":client.traffic.received.load(Relaxed),
            "encoded_requests":client.traffic.payload_out.load(Relaxed),"encoded_responses":client.traffic.payload_in.load(Relaxed),
            "client_encode_cycles":client.traffic.encode_cycles.load(Relaxed),"client_decode_cycles":client.traffic.decode_cycles.load(Relaxed),
            "server_encode_cycles":server_traffic.encode_cycles.load(Relaxed),"server_decode_cycles":server_traffic.decode_cycles.load(Relaxed),
            "client_encode_cpu_ns":client.traffic.encode_ns.load(Relaxed),"client_decode_cpu_ns":client.traffic.decode_ns.load(Relaxed),
            "server_encode_cpu_ns":server_traffic.encode_ns.load(Relaxed),"server_decode_cpu_ns":server_traffic.decode_ns.load(Relaxed),
            "framing_buffer_bytes":client.traffic.payload_out.load(Relaxed)+client.traffic.payload_in.load(Relaxed)}));
    }
    drop(client);
    let mut wire = Client::connect(addr, "localhost", &ca, &peer)?;
    let request = Request::Inspect(GROUP);
    let mut tls_times = vec![];
    for _ in 0..512 {
        let t = Instant::now();
        let Response::Owner(a) = wire.call(&request)? else {
            return Err(Error::Fenced);
        };
        if a != owner {
            return Err(Error::Fenced);
        }
        tls_times.push(t.elapsed().as_nanos() as u64);
    }
    let tls = json!({"tx":wire.traffic.sent.load(Relaxed),"rx":wire.traffic.received.load(Relaxed),"roundtrip_ns":percent(tls_times)});
    drop(wire);
    stop.store(true, Relaxed);
    listener.join().map_err(|_| invalid("server thread"))??;
    let recv = UdpSocket::bind("127.0.0.1:0")?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    sender.connect(recv.local_addr()?)?;
    recv.connect(sender.local_addr()?)?;
    recv.set_read_timeout(Some(Duration::from_secs(2)))?;
    sender.set_read_timeout(Some(Duration::from_secs(2)))?;
    let server = std::thread::spawn(move || -> Result<()> {
        let mut buffer = [0u8; 2048];
        for _ in 0..512 {
            let n = recv.recv(&mut buffer)?;
            let reply = service.dispatch(Role::Worker(w), decode(&buffer[..n])?)?;
            recv.send(&encode(&reply)?)?;
        }
        Ok(())
    });
    let (mut tx, mut rx) = (0, 0);
    let mut times = vec![];
    let mut bytes = [0; 2048];
    for _ in 0..512 {
        let start = Instant::now();
        tx += sender.send(&encode(&request)?)?;
        let n = sender.recv(&mut bytes)?;
        rx += n;
        if !matches!(decode::<Response>(&bytes[..n])?,Response::Owner(a) if a==owner) {
            return Err(Error::Fenced);
        }
        times.push(start.elapsed().as_nanos() as u64);
    }
    server
        .join()
        .map_err(|_| invalid("UDP measurement thread"))??;
    Ok(
        json!({"durable_action_paths":modes,"same_owner_lookup":{"calls":512,"tcp_mtls":tls,
        "udp_loopback":{"tx":tx,"rx":rx,"roundtrip_ns":percent(times)}}}),
    )
}
struct BenchChild(std::process::Child);
impl Drop for BenchChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn bench_worker(dir: &Path, slot: u32) -> Result<(BenchChild, std::net::SocketAddr)> {
    let ready = dir.join(format!("ready-{slot}"));
    let mut child = BenchChild(
        std::process::Command::new(std::env::current_exe()?)
            .arg("--worker")
            .arg(dir)
            .arg(slot.to_string())
            .arg(&ready)
            .stdout(std::process::Stdio::null())
            .spawn()?,
    );
    let start = Instant::now();
    loop {
        if let Ok(addr) = std::fs::read_to_string(&ready) {
            if let Ok(addr) = addr.trim().parse() {
                return Ok((child, addr));
            }
        }
        if child.0.try_wait()?.is_some() || start.elapsed() > Duration::from_secs(15) {
            return Err(invalid("benchmark worker failed to start"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn network_migration(dir: &Path) -> Result<Value> {
    let dir = dir.join("network");
    init_tls(&dir)?;
    let (_a, aa) = bench_worker(&dir, 1)?;
    let (_b, ba) = bench_worker(&dir, 2)?;
    let db = Store::open(dir.join("world.sqlite"))?;
    let ca = std::fs::read(dir.join("ca.der"))?;
    let mut rows = vec![];
    for (slot, from, to) in [(2, aa, ba), (1, ba, aa), (2, aa, ba)] {
        let source = db.owner(GROUP)?;
        let target = db.worker(slot)?;
        let t = Handoff {
            id: Id::new(),
            group: GROUP,
            source,
            target,
        };
        let id = identity(&dir, &worker_name(source.worker))?;
        let mut origin = Client::connect(from, "localhost", &ca, &id)?;
        let mut dest = Client::connect(to, "localhost", &ca, &id)?;
        let cost = is_mesh::remote::migrate(&mut origin, &mut dest, t, 16384)?;
        let tx = origin.traffic.sent.load(Relaxed) + dest.traffic.sent.load(Relaxed);
        let rx = origin.traffic.received.load(Relaxed) + dest.traffic.received.load(Relaxed);
        rows.push(json!({"target":slot,"bulk_payload":cost.payload,"prepare_ns":cost.prepare.as_nanos(),"pause_ns":cost.pause.as_nanos(),"socket_tx":tx,"socket_rx":rx}));
    }
    Ok(json!(rows))
}
fn cpu_model() -> String {
    if let Ok(cpu) = std::env::var("PROCESSOR_IDENTIFIER") {
        return cpu;
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| value.trim().to_owned())
            })
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}
fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--worker") {
        let mut args = std::env::args_os().skip(2);
        let d = PathBuf::from(
            args.next()
                .ok_or_else(|| invalid("benchmark worker directory"))?,
        );
        let slot = args
            .next()
            .and_then(|s| s.into_string().ok())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| invalid("benchmark worker slot"))?;
        return listen(
            d,
            slot,
            "127.0.0.1:0".parse().unwrap(),
            args.next().map(PathBuf::from),
        );
    }
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/infinite-stars-benchmark.json".into());
    let root = std::env::temp_dir().join(format!("zz-stars-bench-{}", Id::new()));
    std::fs::create_dir_all(&root)?;
    let mut scenes = vec![];
    for workers in [1, 2, 4] {
        scenes.push(scene(workers, 1, 256, 160)?);
    }
    for islands in [4, 16] {
        scenes.push(scene(islands, islands, 256 * islands, 160)?);
    }
    let frames = scenes[0]["first_island_frames"].clone();
    let bytes = scenes[0]["first_island_payload"].clone();
    if scenes[3..]
        .iter()
        .any(|s| s["first_island_frames"] != frames || s["first_island_payload"] != bytes)
    {
        return Err(invalid(
            "unrequested remote traffic changed first-island workload",
        ));
    }
    let data = json!({"scenario_version":1,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "cpu":cpu_model(),"logical_threads":std::thread::available_parallelism()?.get(),
        "rustc":String::from_utf8_lossy(&std::process::Command::new("rustc").arg("--version").output()?.stdout).trim(),
        "profile":if cfg!(debug_assertions){"debug"}else{"release"},"scenes":scenes,"migration":migration(&root)?,"transport":transports(&root)?,"network_migration":network_migration(&root)?,"voxel_codecs":codecs()?});
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&data).unwrap())?;
    std::fs::remove_dir_all(root)?;
    println!("Wrote {}", out.display());
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
