use infinite_stars::{host::*, *};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use zz_mesh::{wire::*, *};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait(path: &Path, child: &mut OwnedChild) -> String {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            if !s.trim().is_empty() {
                return s;
            }
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before ready"
        );
        assert!(Instant::now() < end, "child timeout");
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// Run only in the child test process. It stops at the durable commit boundary so
/// the parent can kill its own process before any answer or ECS update is sent.
#[test]
fn child_at_commit() {
    let Some(dir) = std::env::var_os("ZZ_STARS_CRASH_DIR").map(PathBuf::from) else {
        return;
    };
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let (worker, _app) = start_node(&db, 1).unwrap();
    struct Crash {
        service: Arc<Service>,
        dir: PathBuf,
    }
    impl Endpoint for Crash {
        fn dispatch(&self, role: Role, r: Request) -> zz_mesh::Result<Response> {
            let reply = self.service.dispatch(role, r)?;
            if matches!(&reply,Response::Actions(v) if matches!(v.first(),Some(Reply::Decided(Receipt{outcome:Outcome::Committed(_),..}))))
            {
                fs::write(self.dir.join("committed"), "durable")?;
                loop {
                    std::thread::park();
                }
            }
            Ok(reply)
        }
    }
    let l = Listener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &fs::read(dir.join("ca.der")).unwrap(),
        &identity(&dir, "server").unwrap(),
        BTreeMap::from([(
            digest(&fs::read(dir.join("player.der")).unwrap()),
            Role::Player(ACTOR),
        )]),
    )
    .unwrap();
    fs::write(dir.join("crash-ready"), l.address().unwrap().to_string()).unwrap();
    l.serve(
        Arc::new(Crash {
            service: service(&db, worker),
            dir,
        }),
        Arc::new(AtomicBool::new(false)),
        8,
    )
    .unwrap();
}
fn server(dir: &Path, slot: u32, ready: &str) -> (OwnedChild, std::net::SocketAddr) {
    let file = dir.join(ready);
    let child = Command::new(env!("CARGO_BIN_EXE_infinite-stars"))
        .arg("serve")
        .arg(dir)
        .arg(slot.to_string())
        .arg("127.0.0.1:0")
        .arg(&file)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut owned = OwnedChild(child);
    let addr = wait(&file, &mut owned).parse().unwrap();
    (owned, addr)
}
fn tls(dir: &Path, addr: std::net::SocketAddr, name: &str) -> Client {
    Client::connect(
        addr,
        "localhost",
        &fs::read(dir.join("ca.der")).unwrap(),
        &identity(dir, name).unwrap(),
    )
    .unwrap()
}
#[test]
fn two_writers_crash_before_response_retry_and_process_restart() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    init_tls(dir).unwrap();
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let (w, app) = start_node(&db, 1).unwrap();
    drop(app);
    let other = Id::new();
    let remote = Id::new();
    let (b_server, b_addr) = server(dir, 2, "b-ready");
    let b = db.start_worker(8).unwrap();
    let b_owner = db.create_group(remote, b).unwrap();
    let mut merchant = Record::new(other, remote);
    merchant
        .put(
            INVENTORY,
            1,
            &Inventory(BTreeMap::from([(STONE, 8), (COIN, 0)])),
        )
        .unwrap();
    merchant
        .put(
            ORDER,
            1,
            &TradeOrder {
                buyer: ACTOR,
                give: STONE,
                want: COIN,
                limit: 7,
            },
        )
        .unwrap();
    db.checkpoint(remote, b_owner, 0, vec![merchant]).unwrap();
    // Discover the real second process' incarnation using an independent control read.
    let worker2 = Worker {
        slot: 2,
        incarnation: 1,
    };
    let ticket = Handoff {
        id: Id::new(),
        group: remote,
        source: b_owner,
        target: worker2,
    };
    db.begin_handoff(ticket).unwrap();
    let snap = db.snapshot(remote).unwrap();
    db.freeze(ticket, snap.revision, &snap.records).unwrap();
    let mut peer = world(&db, worker2);
    let staged = prepare_world(peer.world_mut(), &snap).unwrap();
    db.acknowledge(ticket, worker2, &staged).unwrap();
    db.switch(ticket).unwrap();
    let session = Id::new();
    let action = Intent::typed(
        ActionKey { session, seq: 1 },
        GROUP,
        &Trade {
            counterparty: other,
            quantity: 4,
        },
    )
    .unwrap();
    let approval = Request::Approval {
        group: remote,
        intent: action.clone(),
    };
    let mut denied = tls(dir, b_addr, "player");
    assert!(matches!(
        denied.call(&approval).unwrap(),
        Response::Error(_)
    ));
    let mut remote_worker = tls(dir, b_addr, &worker_name(worker2));
    assert!(matches!(
        remote_worker.call(&approval).unwrap(),
        Response::Approved
    ));
    let mut crash = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_at_commit", "--nocapture"])
            .env("ZZ_STARS_CRASH_DIR", dir)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let addr = wait(&dir.join("crash-ready"), &mut crash).parse().unwrap();
    let mut first = tls(dir, addr, "player");
    let original = action.clone();
    let thread = std::thread::spawn(move || first.call(&Request::Actions(vec![original])));
    wait(&dir.join("committed"), &mut crash);
    crash.0.kill().unwrap();
    crash.0.wait().unwrap();
    assert!(thread.join().unwrap().is_err());
    drop(b_server);
    let (_new, addr) = server(dir, 1, "a-ready");
    let mut client = tls(dir, addr, "player");
    let request = Request::Actions(vec![action.clone()]);
    let Response::Actions(network) = client.call(&request).unwrap() else {
        panic!("no retry result")
    };
    let owner = db.owner(GROUP).unwrap();
    assert_ne!(owner.worker, w);
    let service = service(&db, owner.worker);
    let Response::Actions(local) = direct(service.as_ref(), Role::Player(ACTOR), request).unwrap()
    else {
        panic!()
    };
    assert_eq!(local, network);
    assert!(matches!(
        &local[0],
        Reply::Decided(Receipt {
            outcome: Outcome::Committed(_),
            ..
        })
    ));
    let inv = |id| {
        let Lookup::Present(r) = db.lookup(id).unwrap() else {
            panic!()
        };
        r.get::<Inventory>(INVENTORY, 1).unwrap()
    };
    assert_eq!((inv(ACTOR).0[&STONE], inv(other).0[&STONE]), (14, 4));
    assert_eq!((inv(ACTOR).0[&COIN], inv(other).0[&COIN]), (6, 4));
    assert!(
        client
            .traffic
            .received
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
}

#[test]
fn live_workers_prepare_switch_fence_and_resume_over_tls() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    init_tls(dir).unwrap();
    let (_a, aa) = server(dir, 1, "a");
    let (_b, ba) = server(dir, 2, "b");
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let old = db.owner(GROUP).unwrap();
    let target = db.worker(2).unwrap();
    let t = Handoff {
        id: Id::new(),
        group: GROUP,
        source: old,
        target,
    };
    let mut source = tls(dir, aa, &worker_name(old.worker));
    let mut dest = tls(dir, ba, &worker_name(old.worker));
    let cost = zz_mesh::remote::migrate(&mut source, &mut dest, t, 9000).unwrap();
    assert_eq!(cost.authority, db.owner(GROUP).unwrap());
    assert_eq!(cost.payload, 4096 + 16);
    let replay = zz_mesh::remote::migrate(&mut source, &mut dest, t, 0).unwrap();
    assert_eq!(replay.payload, 0);
    let key = ActionKey {
        session: Id::new(),
        seq: 1,
    };
    let req = Request::Actions(vec![Intent::typed(
        key,
        GROUP,
        &DropItems(vec![Stack {
            kind: STONE,
            count: 2,
        }]),
    )
    .unwrap()]);
    assert!(matches!(
        tls(dir, aa, "player").call(&req).unwrap(),
        Response::Error(_)
    ));
    let Response::Actions(r) = tls(dir, ba, "player").call(&req).unwrap() else {
        panic!()
    };
    assert!(matches!(
        r[0],
        Reply::Decided(Receipt {
            outcome: Outcome::Committed(_),
            ..
        })
    ));
    let back = Handoff {
        id: Id::new(),
        group: GROUP,
        source: cost.authority,
        target: old.worker,
    };
    let mut origin = tls(dir, ba, &worker_name(target));
    let mut dest = tls(dir, aa, &worker_name(target));
    // Allow the published durable action to reach the authoritative Bevy tick.
    std::thread::sleep(Duration::from_millis(110));
    let warm = zz_mesh::remote::migrate(&mut origin, &mut dest, back, 0).unwrap();
    assert_eq!(warm.payload, 0);
}

#[test]
fn distributed_directory_delivers_only_to_an_interested_world() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    init_tls(dir).unwrap();
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    for (slot, x) in [(2, 3000), (3, 10_000_000)] {
        let w = db.start_worker(slot).unwrap();
        let mut app = world(&db, w);
        app.world_mut().spawn((
            Managed::default(),
            Player,
            Position {
                frame: PLANET_ID,
                mm: [x, 0, 0],
            },
        ));
        app.update();
    }
    let (_a, aa) = server(dir, 1, "directory");
    let (_b, ba) = server(dir, 2, "near");
    let (_c, ca) = server(dir, 3, "far");
    let reader = worker_name(db.worker(1).unwrap());
    let mut a = tls(dir, aa, &reader);
    let mut b = tls(dir, ba, &reader);
    let mut c = tls(dir, ca, &reader);
    let counts = |c: &mut Client| match c.call(&Request::Diagnostics).unwrap() {
        Response::Diagnostics {
            views,
            sent,
            received,
            ..
        } => (views, sent, received),
        other => panic!("{other:?}"),
    };
    let end = Instant::now() + Duration::from_secs(6);
    loop {
        let source = counts(&mut a);
        let near = counts(&mut b);
        if source.2 > 0 && near.2 > 0 {
            assert!(source.0 >= 1 && near.0 >= 1);
            break;
        }
        assert!(
            Instant::now() < end,
            "no directed exchange: {source:?} {near:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(counts(&mut c), (0, 0, 0));
}

#[test]
fn persistent_cli_outbox_routes_through_the_bootstrap_after_migration() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let (_a, aa) = server(dir, 1, "a");
    let (_b, ba) = server(dir, 2, "b");
    let run = |verb: &str, args: Vec<String>| {
        let result = Command::new(env!("CARGO_BIN_EXE_infinite-stars"))
            .arg(verb)
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).into_owned()
    };
    assert!(run("client", vec![aa.to_string()]).contains("sequence 1"));
    run(
        "migrate",
        vec![aa.to_string(), ba.to_string(), "2".to_owned()],
    );
    assert!(run("client", vec![aa.to_string()]).contains("sequence 2"));
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let Lookup::Present(r) = db.lookup(ACTOR).unwrap() else {
        panic!()
    };
    assert_eq!(r.get::<Inventory>(INVENTORY, 1).unwrap().0[&STONE], 8);
}
