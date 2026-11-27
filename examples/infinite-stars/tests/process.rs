use infinite_stars::{host::*, *};
use is_mesh::{wire::*, *};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

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
        fn dispatch(&self, role: Role, r: Request) -> is_mesh::Result<Response> {
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
    let cost = is_mesh::remote::migrate(&mut source, &mut dest, t, 9000).unwrap();
    assert_eq!(cost.authority, db.owner(GROUP).unwrap());
    assert_eq!(cost.payload, 4096 + 16);
    let replay = is_mesh::remote::migrate(&mut source, &mut dest, t, 0).unwrap();
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
    let warm = is_mesh::remote::migrate(&mut origin, &mut dest, back, 0).unwrap();
    assert_eq!(warm.payload, 0);
}

#[test]
fn external_adapter_routes_input_and_resumes_scoped_streams_across_worker_processes() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    init_tls(dir).unwrap();
    let (_a, aa) = server(dir, 1, "first");
    let (_b, ba) = server(dir, 2, "second");
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let a = db.worker(1).unwrap();
    let b = db.worker(2).unwrap();
    let neighbor = Id::new();
    let authority = db.create_group(neighbor, b).unwrap();
    let mut record = Record::new(neighbor, neighbor);
    record.put(PLAYER, 1, &Player).unwrap();
    record
        .put(
            POSITION,
            1,
            &Position {
                frame: PLANET_ID,
                mm: [50_000, 0, 0],
            },
        )
        .unwrap();
    record.put(INVENTORY, 1, &Inventory::default()).unwrap();
    db.checkpoint(neighbor, authority, 0, vec![record]).unwrap();
    let mut origin = tls(dir, aa, &worker_name(b));
    let mut remote = tls(dir, ba, &worker_name(a));
    let mut player = tls(dir, aa, "player");
    let directory = SessionDirectory::persistent(db.clone());
    let p = PrincipalId(Id::new());
    let s = SessionId(Id::new());
    let subscriber = SubscriberId(Id::new());
    let grant = |epoch| SessionGrant {
        principal: p,
        session: s,
        object: ACTOR,
        control_epoch: epoch,
        expires_at: SystemTime::now() + Duration::from_secs(120),
    };
    let first = directory.admit(grant(1), ConnectionId(Id::new())).unwrap();
    directory.subscribe(first, Some(subscriber)).unwrap();
    let input = SessionInput {
        binding: first,
        sequence: 7,
        at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        axes: [3, 0, 0],
    };
    assert!(matches!(
        player.call(&Request::SessionInputs(vec![input])).unwrap(),
        Response::Error(_)
    ));
    assert!(matches!(
        origin.call(&Request::SessionInputs(vec![input])).unwrap(),
        Response::Approved
    ));
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let ready = matches!(
            origin.call(&Request::SessionProgress(first)).unwrap(),
            Response::Input(InputAck { sequence: 7, .. })
        );
        let both = matches!(origin.call(&Request::SubscriberSources(first,subscriber)).unwrap(),Response::SightSources(map) if map.contains_key(&a)&&map.contains_key(&b));
        if ready && both {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "routed input and sight discovery"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
    let mut near = WorkerSubscription::default();
    let mut far = WorkerSubscription::default();
    let one = near.poll(&mut origin, first, subscriber).unwrap();
    let frame = one.frame.unwrap();
    assert!(frame.events.iter().any(|e|matches!(e,SubscriberEvent::Enter{object,fields,..} if *object==ACTOR&&fields.iter().any(|(k,_)|*k==INVENTORY))));
    near.applied(one.stream, frame.sequence).unwrap();
    let two = far.poll(&mut remote, first, subscriber).unwrap();
    let frame = two.frame.unwrap();
    assert!(frame.events.iter().any(|e|matches!(e,SubscriberEvent::Enter{object,fields,..} if *object==neighbor&&!fields.iter().any(|(k,_)|*k==INVENTORY))));
    far.applied(two.stream, frame.sequence).unwrap();
    assert!(far
        .poll(&mut remote, first, subscriber)
        .unwrap()
        .frame
        .is_none());
    let next = directory.admit(grant(2), ConnectionId(Id::new())).unwrap();
    directory.subscribe(next, Some(subscriber)).unwrap();
    assert!(directory.suspend(first).is_err());
    assert!(far.poll(&mut remote, first, subscriber).is_err());
    assert!(far
        .poll(&mut remote, next, subscriber)
        .unwrap()
        .frame
        .is_none());
    let ticket = Handoff {
        id: Id::new(),
        group: GROUP,
        source: db.owner(GROUP).unwrap(),
        target: b,
    };
    let mut source = tls(dir, aa, &worker_name(a));
    let mut destination = tls(dir, ba, &worker_name(b));
    is_mesh::remote::migrate(&mut source, &mut destination, ticket, 8 << 20).unwrap();
    assert!(matches!(
        origin.call(&Request::SessionInputs(vec![input])).unwrap(),
        Response::Error(_)
    ));
    let current = SessionInput {
        binding: next,
        sequence: 1,
        at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        axes: [0, 1, 0],
    };
    assert!(matches!(
        remote.call(&Request::SessionInputs(vec![current])).unwrap(),
        Response::Approved
    ));
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if matches!(
            remote.call(&Request::SessionProgress(next)).unwrap(),
            Response::Input(InputAck { sequence: 1, .. })
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    directory.close(next).unwrap();
    assert!(far.poll(&mut remote, next, subscriber).is_err());
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
fn five_authenticated_players_observe_only_their_own_neighbours_over_tls() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    init_tls(dir).unwrap();
    fs::write(
        dir.join("space.json"),
        br#"{"player_sight_m":120,"ship_sight_m":750}"#,
    )
    .unwrap();
    let db = Store::open(dir.join("world.sqlite")).unwrap();
    let w = db.start_worker(1).unwrap();
    let remote = db.start_worker(2).unwrap();
    let mut a = world(&db, w);
    let mut b = world(&db, remote);
    let group = Id::numbered(4000);
    let watchers: Vec<_> = (1..=5).map(Id::numbered).collect();
    let base = 1_000_000_000_000i64;
    let pos = |x| Position {
        frame: PLANET_ID,
        mm: [x, 0, 0],
    };
    let mut credentials = vec![];
    for (i, &id) in watchers.iter().enumerate() {
        a.world_mut().spawn((
            Managed::new(id, group),
            Player,
            pos(base * (i as i64 + 1)),
            movement::Drive::default(),
            Inventory::default(),
        ));
        let dest = dir.join(format!("client-{i}"));
        deployment::provision_player(dir, &dest, id).unwrap();
        credentials.push(dest);
    }
    let local = Id::numbered(80);
    let ship = Id::numbered(81);
    let local_credentials = dir.join("local-player");
    deployment::provision_player(dir, &local_credentials, local).unwrap();
    a.world_mut().spawn((
        Managed::new(local, group),
        Player,
        pos(base + 120001),
        Inventory::default(),
    ));
    b.world_mut()
        .spawn((Managed::new(ship, ship), Ship, pos(base + 750001)));
    a.update();
    b.update();
    checkpoint_group(a.world_mut(), group).unwrap();
    checkpoint_group(b.world_mut(), ship).unwrap();
    drop(a);
    drop(b);
    let (_one, first) = server(dir, 1, "first.ready");
    let (_two, second) = server(dir, 2, "second.ready");
    let session = Id::numbered(707);
    db.session(session, watchers[0]).unwrap();
    let mut readers: Vec<_> = credentials
        .iter()
        .enumerate()
        .map(|(i, creds)| tls(creds, if i == 0 { first } else { second }, "player"))
        .collect();
    let mut views: Vec<_> = (0..5).map(|_| PlayerSightClient::default()).collect();
    let render = |i: usize, view: &mut PlayerSightClient, reader: &mut Client, expected: &[Id]| {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let result = view.refresh(reader, |_, addr| {
                Client::connect(
                    addr,
                    "localhost",
                    &fs::read(dir.join("ca.der"))?,
                    &identity(&credentials[i], "player")?,
                )
            });
            if result.is_ok() && view.records().keys().copied().eq(expected.iter().copied()) {
                break;
            }
            assert!(
                Instant::now() < until,
                "player {i}, expected {expected:?}, result {result:?}, visible {:?}",
                view.records().keys()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    for i in 0..5 {
        render(i, &mut views[i], &mut readers[i], &[]);
    }
    // A known remote server address also filters by the authenticated character.
    let mut other = tls(&credentials[1], second, "player");
    assert!(
        matches!(other.call(&Request::SightPage{after:None, known:None}).unwrap(),Response::SightPage(p) if p.records.is_empty())
    );
    assert!(matches!(
        readers[1]
            .call(&Request::SightPlan {
                viewer: watchers[0],
                after: None
            })
            .unwrap(),
        Response::Error(_)
    ));
    let move_player = |reader: &mut Client, seq, axes| {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(matches!(
            reader
                .call(&Request::Input(Input {
                    session,
                    sequence: seq,
                    at_ms: ms,
                    axes
                }))
                .unwrap(),
            Response::Input(_)
        ));
    };
    move_player(&mut readers[0], 1, [32767, 0, 0]);
    render(0, &mut views[0], &mut readers[0], &[local, ship]);
    assert!(!views[0].records()[&local].fields.contains_key(&INVENTORY));
    assert!(views[0].records()[&local].fields.contains_key(&PLAYER));
    assert!(views[0].records()[&ship].fields.contains_key(&SHIP));
    let reused = views[0]
        .refresh(&mut readers[0], |_, _| {
            Err(invalid("expected live connection"))
        })
        .unwrap();
    assert_eq!((reused.changed_pages, reused.unchanged_pages), (0, 2));
    let mut source = tls(&credentials[0], second, "player");
    let Response::SightPage(page) = source
        .call(&Request::SightPage {
            after: None,
            known: None,
        })
        .unwrap()
    else {
        panic!("expected public ship page")
    };
    assert_eq!(
        page.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        [ship]
    );
    let rx = source
        .traffic
        .payload_in
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        source
            .call(&Request::SightPage {
                after: None,
                known: Some(page.content_digest())
            })
            .unwrap(),
        Response::SightUnchanged { next: None }
    ));
    assert!(
        source
            .traffic
            .payload_in
            .load(std::sync::atomic::Ordering::Relaxed)
            - rx
            < rx
    );
    let cli = Command::new(env!("CARGO_BIN_EXE_infinite-stars"))
        .args([
            "sight",
            credentials[0].to_str().unwrap(),
            &second.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(String::from_utf8_lossy(&cli.stdout).contains("2 visible"));
    let mut neighbour = tls(&local_credentials, first, "player");
    let Response::SightPage(initial) = neighbour
        .call(&Request::SightPage {
            after: None,
            known: None,
        })
        .unwrap()
    else {
        panic!("expected the moving neighbour's public record");
    };
    assert_eq!(initial.records[0].id, watchers[0]);
    let known = Some(initial.content_digest());
    move_player(&mut readers[0], 2, [0, 32767, 0]);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Response::SightPage(p) = neighbour
            .call(&Request::SightPage { after: None, known })
            .unwrap()
        {
            if p.records.first().is_some_and(|r| {
                r.id == watchers[0] && r.get::<Position>(POSITION, 1).unwrap().mm[1] > 0
            }) {
                break;
            }
        }
        assert!(
            Instant::now() < until,
            "a changed position must replace the cached page"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for i in 1..5 {
        render(i, &mut views[i], &mut readers[i], &[]);
    }
    move_player(&mut readers[0], 3, [-32767, 0, 0]);
    render(0, &mut views[0], &mut readers[0], &[]);
    assert_eq!(db.owner(group).unwrap().worker, db.worker(1).unwrap());
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

#[test]
fn separate_volumes_resume_after_authority_restart_handoff_and_worker_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let authority = root.join("authority");
    let first = root.join("worker-a");
    let second = root.join("worker-b");
    let player = root.join("player");
    let exe = env!("CARGO_BIN_EXE_infinite-stars");
    let run = |verb: &str, dir: &Path, args: &[&str]| {
        let p = Command::new(exe)
            .arg(verb)
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            p.status.success(),
            "{verb}: {}",
            String::from_utf8_lossy(&p.stderr)
        );
        String::from_utf8(p.stdout).unwrap()
    };
    let start = |verb: &str, dir: &Path, args: &[&str], label: &str| {
        let marker = root.join(label);
        let mut p = OwnedChild(
            Command::new(exe)
                .arg(verb)
                .arg(dir)
                .args(args)
                .arg(&marker)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let addr = wait(&marker, &mut p);
        (p, addr)
    };
    run("provision", &authority, &["1", first.to_str().unwrap()]);
    run("provision", &authority, &["2", second.to_str().unwrap()]);
    run("provision-client", &authority, &[player.to_str().unwrap()]);
    let any = "127.0.0.1:0";
    let (control, store_addr) = start("authority", &authority, &[any], "store1");
    let (_a, aa) = start("worker", &first, &[&store_addr, any], "a");
    let (_b, ba) = start("worker", &second, &[&store_addr, any], "b");
    let db = Store::open(authority.join("world.sqlite")).unwrap();
    let observer = root.join("observer");
    run("provision", &authority, &["3", observer.to_str().unwrap()]);
    let (unrelated, _, _) =
        deployment::connect_worker(&observer, store_addr.parse().unwrap()).unwrap();
    assert!(matches!(
        unrelated.lookup(ACTOR),
        Err(is_mesh::Error::Denied)
    ));
    assert!(matches!(
        unrelated.snapshot(GROUP),
        Err(is_mesh::Error::Denied)
    ));
    assert!(run("client", &player, &[&aa]).contains("sequence 1"));
    let session = Id::new();
    let action = Request::Actions(vec![Intent::typed(
        ActionKey { session, seq: 1 },
        GROUP,
        &DropItems(vec![Stack {
            kind: STONE,
            count: 1,
        }]),
    )
    .unwrap()]);
    let connect_player = |addr: &str| {
        Client::connect(
            addr.parse().unwrap(),
            deployment::GAME_DNS,
            &fs::read(player.join("ca.der")).unwrap(),
            &identity(&player, "player").unwrap(),
        )
        .unwrap()
    };
    let mut client = connect_player(&aa);
    // Sync creates the actor-bound logical session. It survives every process below.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let answer = client
            .call(&Request::Sync {
                session,
                group: GROUP,
            })
            .unwrap();
        match answer {
            Response::Confirmed { .. } => break,
            Response::Error(ref msg)
                if msg.contains("waiting for confirmed world revision")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => panic!("initial sync: {answer:?}"),
        }
    }
    let Response::Actions(receipt) = client.call(&action).unwrap() else {
        panic!("drop refused")
    };
    assert!(matches!(
        receipt.as_slice(),
        [Reply::Decided(Receipt {
            outcome: Outcome::Committed(_),
            ..
        })]
    ));
    assert!(matches!(
        client
            .call(&Request::Storage(
                is_mesh::authority_store::Operation::Snapshot(GROUP)
            ))
            .unwrap(),
        Response::Error(_)
    ));
    let ca = fs::read(player.join("ca.der")).unwrap();
    let mut bad = Client::connect(
        store_addr.parse().unwrap(),
        "authority.mesh.internal",
        &ca,
        &identity(&player, "player").unwrap(),
    )
    .unwrap();
    assert!(bad
        .call(&Request::Storage(
            is_mesh::authority_store::Operation::Snapshot(GROUP)
        ))
        .is_err());
    drop(control);
    let (_control, _) = start("authority", &authority, &[&store_addr], "store2");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Response::Actions(v)) = client.call(&action) {
            assert_eq!(v, receipt);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "durable action could not be recovered"
        );
        client = connect_player(&aa);
        std::thread::sleep(Duration::from_millis(20));
    }
    run("migrate", &authority, &[&aa, &ba, "2"]);
    let before = db.owner(GROUP).unwrap();
    assert_eq!(before.worker.slot, 2);
    fs::write(
        second.join("capacity.json"),
        r#"{"tick_budget_us":1,"memory_bytes":268435456}"#,
    )
    .unwrap();
    let (_replacement, newer) = start("worker", &second, &[&store_addr, any], "b2");
    let after = db.owner(GROUP).unwrap();
    assert!(
        after.generation > before.generation
            && after.worker.incarnation > before.worker.incarnation
    );
    assert!(!matches!(
        connect_player(&ba).call(&action),
        Ok(Response::Actions(_))
    ));
    assert!(
        matches!(connect_player(&newer).call(&action), Ok(Response::Actions(v)) if v == receipt)
    );
    assert!(run("client", &player, &[&aa]).contains("sequence 2"));
    let Lookup::Present(r) = db.lookup(ACTOR).unwrap() else {
        panic!("actor missing")
    };
    assert_eq!(r.get::<Inventory>(INVENTORY, 1).unwrap().0[&STONE], 7);
    let deadline = Instant::now() + Duration::from_secs(12);
    while db.owner(GROUP).unwrap().worker.slot == 2 {
        assert!(
            Instant::now() < deadline,
            "eligible group never left overloaded worker"
        );
        run("balance", &authority, &["--once"]);
        std::thread::sleep(Duration::from_millis(200));
    }
    let on_first = db.owner(GROUP).unwrap().worker;
    assert_eq!(on_first.slot, 1);
    fs::remove_file(second.join("capacity.json")).unwrap();
    let (_available, _) = start("worker", &second, &[&store_addr, any], "b3");
    assert!(run("drain", &authority, &["1"]).contains("Slot 1 drained"));
    assert_eq!(db.owner(GROUP).unwrap().worker.slot, 2);
    assert!(matches!(
        db.create_group(Id::new(), on_first),
        Err(is_mesh::Error::Busy(_))
    ));
    run("resume", &authority, &["1"]);
    db.create_group(Id::new(), on_first).unwrap();
    let health: serde_json::Value =
        serde_json::from_str(&run("health", &authority, &[&aa])).unwrap();
    assert_eq!(health["ready"], true);
    let metrics: serde_json::Value =
        serde_json::from_str(&run("metrics", &authority, &[&aa])).unwrap();
    assert!(metrics["transport_in_bytes"].as_u64().unwrap() > 0);
    assert!(metrics["transport_out_bytes"].as_u64().unwrap() > 0);
    assert!(metrics["process"]["resident_bytes"].as_u64().unwrap() > 0);
    assert!(metrics["request_bytes_in_service"].as_u64().unwrap() > 0);
    assert!(matches!(
        connect_player(&aa).call(&Request::Telemetry),
        Ok(Response::Error(_))
    ));
    assert!(matches!(
        connect_player(&aa).call(&Request::Shutdown),
        Ok(Response::Error(_))
    ));
    assert!(matches!(
        Client::connect(
            aa.parse().unwrap(),
            deployment::GAME_DNS,
            &fs::read(authority.join("ca.der")).unwrap(),
            &identity(&authority, "operator").unwrap()
        )
        .unwrap()
        .call(&Request::Shutdown),
        Ok(Response::Error(_))
    ));
    run("drain", &authority, &["1"]);
    assert!(run("shutdown", &authority, &[&aa]).contains("stopped"));
    assert!(db.check_worker(on_first).is_err());
    for dir in [&first, &second, &player] {
        for file in ["world.sqlite", "issuer.pem", "operator.key", "store.key"] {
            assert!(!dir.join(file).exists());
        }
    }
    assert!(!first.join("player.key").exists());
    assert!(!player.join("enrol.key").exists());
}
