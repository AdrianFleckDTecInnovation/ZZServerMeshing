//! Barrier-driven convergence across separate processes using the game's authenticated TCP adapter.
//! Stdin carries benchmark positions and queries. Replicas and directory reports only use mTLS.
use infinite_stars::{
    deployment,
    host::{identity, init_tls, provision_worker, service, worker_name},
    world, Inventory, Player, Position, PLANET_ID, STONE,
};
use is_mesh::{
    wire::{Client, Endpoint, Listener, Request, Response, Role},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{atomic::Ordering::Relaxed, mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
const MEMBERS: usize = 8;
type Point = [i64; 3];
fn object(slot: usize, i: usize) -> Id {
    Id::numbered((1000 + slot * MEMBERS + i) as u128)
}
fn site(n: usize) -> Point {
    match n {
        0 => [0, 0, 0],
        1 => [64000, 0, 0],
        2 => [0, 64000, 0],
        3 => [0, 0, 64000],
        _ => unreachable!(),
    }
}

struct CountPush {
    inner: Arc<dyn Endpoint>,
    by_source: Arc<Mutex<BTreeMap<u32, u64>>>,
}
impl Endpoint for CountPush {
    fn dispatch(&self, role: Role, request: Request) -> Result<Response> {
        if let (Role::Worker(w), Request::Push(frames)) = (role, &request) {
            *self.by_source.lock().unwrap().entry(w.slot).or_default() += frames.len() as u64;
        }
        self.inner.dispatch(role, request)
    }
}
#[derive(Serialize, Deserialize)]
enum Control {
    Set { step: u32, positions: Vec<Point> },
    Read,
}
#[derive(Serialize, Deserialize)]
struct Status {
    step: u32,
    replicas: Vec<(Id, Point)>,
    incoming: BTreeMap<u32, u64>,
    tls_tx: u64,
    tls_rx: u64,
    store_tx: u64,
    store_rx: u64,
    frames_out: u64,
    applied: u64,
}
fn worker(dir: &Path, slot: u32, authority: Option<SocketAddr>) -> Result<()> {
    let (db, w, cred) = if let Some(addr) = authority {
        deployment::connect_worker(dir, addr)?
    } else {
        let db = Store::open(dir.join("world.sqlite"))?;
        let w = db.start_worker(slot)?;
        provision_worker(dir, &db, w)?;
        (db, w, identity(dir, &worker_name(w))?)
    };
    assert_eq!(w.slot, slot);
    let store_traffic = db.network_traffic().unwrap_or_default();
    let mut app = world(&db, w);
    let mut entities = vec![];
    for i in 0..MEMBERS {
        let id = object(slot as usize, i);
        entities.push(
            app.world_mut()
                .spawn((
                    Managed::new(id, id),
                    Player,
                    Position {
                        frame: PLANET_ID,
                        mm: site((slot as usize - 1) / 2),
                    },
                    Inventory(BTreeMap::from([(STONE, 11)])),
                ))
                .id(),
        );
    }
    let registry = db.clone();
    let ca = fs::read(dir.join("ca.der"))?;
    let listener = Listener::bind_resolved(
        "127.0.0.1:0".parse().unwrap(),
        &ca,
        &identity(dir, "server")?,
        move |cert| registry.authenticated_worker(cert).map(Role::Worker),
    )?;
    db.advertise(w, listener.address()?)?;
    let traffic = listener.traffic.clone();
    let (endpoint, mut pump) =
        remote::worker(service(&db, w), app.world().resource::<BlobCache>().clone());
    let registry = db.clone();
    pump.peers = Some(network_mesh::PeerMesh::new(db, w, move |to| {
        let name = if authority.is_some() {
            deployment::worker_dns(to.slot)
        } else {
            "localhost".into()
        };
        Client::connect(registry.address(to)?, &name, &ca, &cred)
    }));
    let incoming = Arc::new(Mutex::new(BTreeMap::new()));
    let counted = Arc::new(CountPush {
        inner: endpoint,
        by_source: incoming.clone(),
    });
    thread::spawn(move || listener.serve(counted, Default::default(), 16));
    let (tx, rx) = mpsc::sync_channel(8);
    thread::spawn(move || {
        for line in std::io::stdin()
            .lock()
            .lines()
            .map_while(std::result::Result::ok)
        {
            let Ok(command) = serde_json::from_str::<Control>(&line) else {
                break;
            };
            if tx.send(command).is_err() {
                break;
            }
        }
    });
    println!("ready");
    let mut next = Instant::now();
    let (mut requested, mut applied) = (0, 0);
    loop {
        match rx.try_recv() {
            Ok(Control::Set { step, positions }) => {
                assert_eq!(positions.len(), MEMBERS);
                for (&e, p) in entities.iter().zip(positions) {
                    app.world_mut().get_mut::<Position>(e).unwrap().mm = p;
                }
                requested = step;
            }
            Ok(Control::Read) => {
                let world = app.world();
                let mut replicas = vec![];
                for (&id, &e) in &world.resource::<MeshState>().entities {
                    if world.get::<Replica>(e).is_some() {
                        assert!(
                            world.get::<Simulated>(e).is_none()
                                && world.get::<Inventory>(e).is_none()
                        );
                        replicas.push((id, world.get::<Position>(e).unwrap().mm));
                    }
                }
                assert!(
                    world.resource::<MeshState>().diagnostics.is_empty(),
                    "{:?}",
                    world.resource::<MeshState>().diagnostics
                );
                let p = pump.peers.as_ref().unwrap();
                assert!(p.last_error.is_none(), "{:?}", p.last_error);
                let status = Status {
                    step: applied,
                    replicas,
                    incoming: incoming.lock().unwrap().clone(),
                    tls_rx: traffic.received.load(Relaxed),
                    tls_tx: traffic.sent.load(Relaxed),
                    store_rx: store_traffic.received.load(Relaxed),
                    store_tx: store_traffic.sent.load(Relaxed),
                    frames_out: p.frames_out,
                    applied: p.frames_in,
                };
                println!("{}", serde_json::to_string(&status).unwrap());
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= next {
            app.update();
            pump.peers.as_mut().unwrap().step(app.world_mut())?;
            applied = requested;
            next = Instant::now() + Duration::from_millis(50);
        }
        pump.pump(app.world_mut(), 16, Duration::from_millis(3));
        thread::sleep(Duration::from_millis(1));
    }
}
struct Node {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<String>,
}
impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Node {
    fn start(dir: &Path, slot: usize, authority: Option<SocketAddr>) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--worker")
            .arg(dir)
            .arg(slot.to_string())
            .args(authority.map(|a| a.to_string()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::sync_channel(4);
        thread::spawn(move || {
            for line in BufReader::new(output)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let node = Self {
            child,
            input,
            replies,
        };
        if node.line()? != "ready" {
            return Err(invalid("worker startup failed"));
        }
        Ok(node)
    }
    fn line(&self) -> Result<String> {
        self.replies
            .recv_timeout(Duration::from_secs(12))
            .map_err(|e| invalid(format!("worker reply: {e}")))
    }
    fn send(&mut self, c: &Control) -> Result<()> {
        serde_json::to_writer(&mut self.input, c).map_err(|e| invalid(e.to_string()))?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }
}
fn read(nodes: &mut [Node]) -> Result<Vec<Status>> {
    for n in &mut *nodes {
        n.send(&Control::Read)?;
    }
    nodes
        .iter()
        .map(|n| serde_json::from_str(&n.line()?).map_err(|e| invalid(e.to_string())))
        .collect()
}
fn totals(states: &[Status]) -> [u64; 7] {
    let mut sum = [0; 7];
    for (to, s) in states.iter().enumerate() {
        sum[0] += s.tls_tx;
        sum[1] += s.tls_rx;
        sum[2] += s.frames_out;
        sum[3] += s.applied;
        sum[5] += s.store_tx;
        sum[6] += s.store_rx;
        sum[4] += s
            .incoming
            .iter()
            .filter(|(slot, _)| ((**slot as usize) - 1) / 2 != to / 2)
            .map(|(_, n)| *n)
            .sum::<u64>();
    }
    sum
}
// A complete per-ID oracle for the network scenes. Step distances avoid the 10-11 m retention band.
fn expected(positions: &[Vec<Point>]) -> Vec<Vec<(Id, Point)>> {
    positions
        .iter()
        .enumerate()
        .map(|(to, local)| {
            let mut wanted = vec![];
            for (from, remote) in positions.iter().enumerate().filter(|(i, _)| *i != to) {
                for (i, &p) in remote.iter().enumerate() {
                    let distance = local
                        .iter()
                        .map(|q| (0..3).map(|axis| p[axis].abs_diff(q[axis])).max().unwrap())
                        .min()
                        .unwrap();
                    assert!(distance <= 10_000 || distance > 11_000);
                    if distance <= 10_000 {
                        wanted.push((object(from + 1, i), p));
                    }
                }
            }
            wanted
        })
        .collect()
}
fn measure(
    nodes: &mut [Node],
    step: u32,
    center: impl Fn(usize, usize) -> Point,
) -> Result<(Vec<Status>, f64)> {
    let positions: Vec<Vec<Point>> = (0..nodes.len())
        .map(|slot| {
            (0..MEMBERS)
                .map(|i| {
                    let [x, y, z] = center(slot / 2, i);
                    [
                        x + (i % 4) as i64 * 50,
                        y + (i / 4) as i64 * 50,
                        z + (step % 2) as i64 * 25,
                    ]
                })
                .collect()
        })
        .collect();
    let wanted = expected(&positions);
    let start = Instant::now();
    for (n, positions) in nodes.iter_mut().zip(positions) {
        n.send(&Control::Set { step, positions })?;
    }
    loop {
        let states = read(nodes)?;
        if states
            .iter()
            .zip(&wanted)
            .all(|(s, w)| s.step == step && &s.replicas == w)
        {
            return Ok((states, start.elapsed().as_secs_f64() * 1000.0));
        }
        if start.elapsed() > Duration::from_secs(12) {
            return Err(invalid(format!(
                "step {step} convergence timeout: {:?}",
                states
                    .iter()
                    .zip(&wanted)
                    .map(|(s, w)| (s.step, s.replicas.len(), w.len()))
                    .collect::<Vec<_>>()
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn segment(
    nodes: &mut [Node],
    step: &mut u32,
    name: &str,
    count: usize,
    poses: impl Fn(usize, usize, usize) -> Point,
) -> Result<Value> {
    let before = totals(&read(nodes)?);
    let mut times = vec![];
    let mut after = before;
    for i in 1..=count {
        *step += 1;
        let (states, ms) = measure(nodes, *step, |s, m| poses(i, s, m))?;
        times.push(ms);
        after = totals(&states);
    }
    let diff = std::array::from_fn::<_, 7, _>(|i| after[i] - before[i]);
    if name == "isolated" {
        assert_eq!(diff[4], 0);
        assert!(diff[2] > 0 && diff[3] > 0);
    }
    let mut order = times.clone();
    order.sort_by(f64::total_cmp);
    Ok(
        json!({"phase":name,"barriers":count,"convergence_ms":times,"p50_ms":order[count.div_ceil(2)-1],
        "p95_ms":order[(count*95).div_ceil(100)-1],"tls_tx":diff[0],"tls_rx":diff[1],
        "queued_frames":diff[2],"applied_frames":diff[3],"incoming_foreign_home_frames":diff[4],
        "authority_tls_tx":diff[5],"authority_tls_rx":diff[6]}),
    )
}
fn lerp(a: Point, b: Point, n: usize) -> Point {
    std::array::from_fn(|k| a[k] + (b[k] - a[k]) * n as i64 / 8)
}
struct AuthorityProcess(Child);
impl Drop for AuthorityProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn scene(dir: &Path, sites: usize, distributed: bool) -> Result<Value> {
    let control = dir.join("authority");
    let _authority;
    let address = if distributed {
        for slot in 1..=sites * 2 {
            deployment::provision(&control, &dir.join(slot.to_string()), Some(slot as u32))?;
        }
        let ready = dir.join("authority-ready");
        let child = Command::new(std::env::current_exe()?)
            .arg("--authority")
            .arg(&control)
            .arg(&ready)
            .stdout(Stdio::null())
            .spawn()?;
        _authority = AuthorityProcess(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(addr) = fs::read_to_string(&ready).ok().and_then(|s| s.parse().ok()) {
                break Some(addr);
            }
            if Instant::now() >= deadline {
                return Err(invalid("authority startup timed out"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    } else {
        init_tls(dir)?;
        None
    };
    let mut nodes = (1..=sites * 2)
        .map(|slot| {
            let home = if distributed {
                dir.join(slot.to_string())
            } else {
                dir.to_path_buf()
            };
            Node::start(&home, slot, address)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut step = 1;
    measure(&mut nodes, step, |s, _| site(s))?;
    let mut rows = vec![segment(
        &mut nodes,
        &mut step,
        "isolated",
        12,
        |_, s, _| site(s),
    )?];
    if sites == 4 {
        // Drain any outstanding ACK, then exceed the listener's five-second idle timeout.
        // Active directory polls keep running. The next pose must reconnect and converge.
        thread::sleep(Duration::from_millis(550));
        let idle_before = totals(&read(&mut nodes)?);
        let start = Instant::now();
        thread::sleep(Duration::from_millis(5500));
        let idle_after = totals(&read(&mut nodes)?);
        assert_eq!(&idle_after[2..5], &idle_before[2..5]);
        let (tx, rx) = (
            idle_after[0] - idle_before[0],
            idle_after[1] - idle_before[1],
        );
        assert!(tx > 0 && rx > 0);
        rows.push(json!({"phase":"idle","seconds":start.elapsed().as_secs_f64(),
            "tls_tx":tx,"tls_rx":rx,"applied_frames":0,"queued_frames":0,"incoming_foreign_home_frames":0,
            "authority_tls_tx":idle_after[5]-idle_before[5],"authority_tls_rx":idle_after[6]-idle_before[6]}));
        let mut centers = (0..4).map(site).collect::<Vec<_>>();
        for (group, join) in [
            (1, true),
            (2, true),
            (3, true),
            (3, false),
            (2, false),
            (1, false),
        ] {
            let a = centers[group];
            let b = if join { site(0) } else { site(group) };
            rows.push(segment(
                &mut nodes,
                &mut step,
                if join { "approach" } else { "depart" },
                8,
                |t, s, _| {
                    if s == group {
                        lerp(a, b, t)
                    } else {
                        centers[s]
                    }
                },
            )?);
            centers[group] = b;
            rows.push(segment(&mut nodes, &mut step, "hold", 4, |_, s, _| {
                centers[s]
            })?);
        }
        for travelers in [1, 4, 8] {
            for back in [false, true] {
                let mut row = segment(&mut nodes, &mut step, "exchange", 8, |t, s, i| {
                    if s > 1 || i >= travelers {
                        return site(s);
                    }
                    let (a, b) = (site(s), site(1 - s));
                    if back {
                        lerp(b, a, t)
                    } else {
                        lerp(a, b, t)
                    }
                })?;
                row["players_switching"] = json!(travelers * 4);
                row["return"] = json!(back);
                rows.push(row);
            }
        }
    }
    Ok(json!({"processes":nodes.len(),"players":nodes.len()*MEMBERS,"sites":sites,"segments":rows}))
}
fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let out = PathBuf::from(
        args.next()
            .ok_or_else(|| invalid("supply output JSON path"))?,
    );
    if out == Path::new("--worker") {
        let dir = PathBuf::from(args.next().ok_or_else(|| invalid("worker directory"))?);
        let slot = args
            .next()
            .and_then(|s| s.to_str()?.parse().ok())
            .ok_or_else(|| invalid("worker slot"))?;
        let authority = args
            .next()
            .map(|s| {
                s.to_string_lossy()
                    .parse()
                    .map_err(|_| invalid("authority address"))
            })
            .transpose()?;
        return worker(&dir, slot, authority);
    }
    if out == Path::new("--authority") {
        let dir = PathBuf::from(args.next().ok_or_else(|| invalid("authority directory"))?);
        let ready = PathBuf::from(args.next().ok_or_else(|| invalid("authority ready file"))?);
        return deployment::serve(&dir, "127.0.0.1:0".parse().unwrap(), Some(ready));
    }
    let distributed = match args.next() {
        None => false,
        Some(x) if x == "--separate-volumes" => true,
        _ => return Err(invalid("expected --separate-volumes")),
    };
    let temp = tempfile::tempdir()?;
    let mut scenes = vec![];
    for sites in [1, 2, 4] {
        scenes.push(scene(
            &temp.path().join(format!("sites-{sites}")),
            sites,
            distributed,
        )?);
    }
    fs::write(out,serde_json::to_vec_pretty(&json!({"version":2,"separate_volumes":distributed,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"scenes":scenes})).map_err(|e|invalid(e.to_string()))?)?;
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1)
    }
}
