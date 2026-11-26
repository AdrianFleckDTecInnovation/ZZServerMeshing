use bevy::prelude::*;
use infinite_stars::{movement::Drive, *};
use is_mesh::*;

#[test]
fn shipped_space_configuration_matches_the_runtime_defaults() {
    let settings: SpaceSettings =
        serde_json::from_str(include_str!("../space.example.json")).unwrap();
    assert_eq!(settings, SpaceSettings::default());
    for json in [
        r#"{"player_sigth_m":10}"#,
        r#"{"ship_sight_m":-1}"#,
        r#"{"contact_buffer_m":5000000000}"#,
    ] {
        assert!(serde_json::from_str::<SpaceSettings>(json).is_err());
    }
}

fn player(world: &mut World, id: Id, group: Id, frame: Id, mm: [i64; 3]) -> Entity {
    world
        .spawn((
            Managed::new(id, group),
            Player,
            Position { frame, mm },
            Drive::default(),
            Inventory::default(),
        ))
        .id()
}
fn ids(records: &std::collections::BTreeMap<Id, Record>) -> Vec<Id> {
    records.keys().copied().collect()
}

#[test]
fn private_checkpoints_skip_public_codecs_spatial_rules_and_peer_projection() {
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Component, Serialize, Deserialize)]
    struct PublicProbe(u32);
    #[derive(Resource)]
    struct SpatialCalls(SpatialRule, AtomicUsize);
    static PUBLIC: AtomicUsize = AtomicUsize::new(0);
    let db = Store::open(":memory:").unwrap();
    let workers = [db.start_worker(1).unwrap(), db.start_worker(2).unwrap()];
    let actor = Id::numbered(107);
    let mut pool = Pool::new(db.clone(), 256);
    pool.policy.every = 0;
    for (i, w) in workers.into_iter().enumerate() {
        let mut app = world(&db, w);
        let id = Id::numbered(107 + i as u128);
        let rule = app.world_mut().remove_resource::<SpatialRule>().unwrap();
        app.insert_resource(SpatialCalls(rule, AtomicUsize::new(0)))
            .insert_resource(SpatialRule(|w, e| {
                let rule = w.resource::<SpatialCalls>();
                rule.1.fetch_add(1, Ordering::Relaxed);
                (rule.0 .0)(w, e)
            }));
        app.mesh_component::<PublicProbe>(
            302,
            1,
            Rules {
                visibility: Visibility::Public,
                persist: true,
                migrate: true,
            },
            ComponentCodec {
                encode: |value, _| {
                    PUBLIC.fetch_add(1, Ordering::Relaxed);
                    encode(value)
                },
                ..Default::default()
            },
        )
        .unwrap();
        let entity = player(app.world_mut(), id, id, PLANET_ID, [0; 3]);
        app.world_mut().entity_mut(entity).insert(PublicProbe(42));
        pool.add(w, app).unwrap();
    }
    for _ in 0..5 {
        pool.step().unwrap();
        pool.deliver_all().unwrap();
    }
    let before = pool.link_stats().0;
    PUBLIC.store(0, Ordering::Relaxed);
    let worker = workers[0];
    let world = pool.app(worker).unwrap().world();
    let entity = world.resource::<MeshState>().entities[&actor];
    world
        .resource::<SpatialCalls>()
        .1
        .store(0, Ordering::Relaxed);
    for tick in 1..=40 {
        pool.app_mut(worker)
            .unwrap()
            .world_mut()
            .get_mut::<Inventory>(entity)
            .unwrap()
            .0
            .insert(STONE, tick);
        pool.step().unwrap();
        assert_eq!(pool.deliver_all().unwrap(), 0);
    }
    assert_eq!(pool.link_stats().0, before);
    assert_eq!(PUBLIC.load(Ordering::Relaxed), 0);
    assert_eq!(
        pool.app(worker)
            .unwrap()
            .world()
            .resource::<SpatialCalls>()
            .1
            .load(Ordering::Relaxed),
        0
    );
    let Lookup::Present(saved) = db.lookup(actor).unwrap() else {
        panic!("checkpoint missing")
    };
    assert!(saved.get::<Inventory>(INVENTORY, 1).unwrap().0[&STONE] >= 20);
    assert_eq!(saved.get::<PublicProbe>(302, 1).unwrap().0, 42);
}

#[test]
fn the_tick_does_no_sight_reads_and_neighbouring_readers_share_the_public_ship() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Resource)]
    struct CountedRule(SightRule, AtomicUsize);
    let db = Store::open(":memory:").unwrap();
    let w = db.start_worker(1).unwrap();
    let mut app = world(&db, w);
    let original = app.world_mut().remove_resource::<SightRule>().unwrap();
    app.insert_resource(CountedRule(original, AtomicUsize::default()))
        .insert_resource(SightRule(|world, entity| {
            let CountedRule(rule, count) = world.resource::<CountedRule>();
            count.fetch_add(1, Ordering::Relaxed);
            rule.0(world, entity)
        }));
    let observers = [Id::numbered(1), Id::numbered(2)];
    let ship = Id::numbered(3);
    for actor in observers {
        player(app.world_mut(), actor, actor, PLANET_ID, [0; 3]);
    }
    app.world_mut().spawn((
        Managed::new(ship, ship),
        Ship,
        Position {
            frame: PLANET_ID,
            mm: [400000, 0, 0],
        },
    ));
    let mut pool = Pool::new(db, 64);
    pool.add(w, app).unwrap();
    pool.step().unwrap();
    pool.deliver_all().unwrap();
    let count = &pool.app(w).unwrap().world().resource::<CountedRule>().1;
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let mut shared = Vec::new();
    pool.visit_player_sights(&observers, |_, records| {
        assert_eq!(records.len(), 2);
        let public_ship = records.iter().copied().find(|r| r.id == ship).unwrap();
        shared.push(std::ptr::from_ref(public_ship));
    })
    .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 3);
    assert_eq!(shared.len(), 2);
    assert_eq!(shared[0], shared[1]);
}

#[test]
fn each_player_has_an_exact_spherical_view_independent_of_physical_islands() {
    let db = Store::open(":memory:").unwrap();
    let one = db.start_worker(1).unwrap();
    let two = db.start_worker(2).unwrap();
    let mut pool = Pool::new(db.clone(), 10000);
    pool.policy.every = 0;
    // These physical boxes deliberately form a single transitive island.
    let settings = SpaceSettings {
        player_half_extent_mm: 60_000,
        ..Default::default()
    };
    let mut a = world_with_settings(&db, one, settings);
    let mut b = world_with_settings(&db, two, settings);
    let g = Id::numbered(501);
    let other = Id::numbered(502);
    let observer = Id::numbered(1);
    let near = Id::numbered(2);
    let chained = Id::numbered(3);
    let diagonal = Id::numbered(4);
    let ship = Id::numbered(5);
    let corner = Id::numbered(6);
    let hidden_frame = Id::numbered(7);
    let same_position = Id::numbered(8);
    player(a.world_mut(), observer, g, PLANET_ID, [0; 3]);
    let e = player(a.world_mut(), near, g, PLANET_ID, [100000, 0, 0]);
    player(a.world_mut(), chained, g, PLANET_ID, [200000, 0, 0]);
    player(b.world_mut(), diagonal, other, PLANET_ID, [0, 60000, 80000]);
    let s = b
        .world_mut()
        .spawn((
            Managed::new(ship, other),
            Ship,
            Position {
                frame: PLANET_ID,
                mm: [300000, 400000, 0],
            },
        ))
        .id();
    player(b.world_mut(), corner, other, PLANET_ID, [80000, 80000, 0]);
    player(
        b.world_mut(),
        hidden_frame,
        other,
        Id::numbered(9000),
        [0; 3],
    );
    player(a.world_mut(), same_position, g, PLANET_ID, [0; 3]);
    pool.add(one, a).unwrap();
    pool.add(two, b).unwrap();
    pool.step().unwrap();
    pool.deliver_all().unwrap();
    assert!(pool
        .directory
        .island_groups()
        .iter()
        .any(|i| i.contains(&observer) && i.contains(&near) && i.contains(&chained)));
    let view = pool.player_sight(observer).unwrap();
    assert_eq!(ids(&view), [near, diagonal, ship, same_position]);
    let viewers = [observer, near, same_position, hidden_frame];
    let expected: Vec<_> = viewers
        .iter()
        .map(|&id| pool.player_sight(id).unwrap())
        .collect();
    let mut checked = 0;
    pool.visit_player_sights(&viewers, |viewer, rows| {
        assert_eq!(viewer, viewers[checked]);
        assert!(rows.iter().copied().eq(expected[checked].values()));
        checked += 1;
    })
    .unwrap();
    assert_eq!(checked, viewers.len());
    assert!(pool.player_sight(near).unwrap().contains_key(&observer));
    assert_eq!(
        view[&diagonal].get::<Position>(POSITION, 1).unwrap().mm,
        [0, 60000, 80000]
    );
    assert_eq!(
        view[&near].fields.keys().copied().collect::<Vec<_>>(),
        [PLAYER, POSITION]
    );
    assert_eq!(
        view[&ship].fields.keys().copied().collect::<Vec<_>>(),
        [POSITION, SHIP]
    );
    pool.app_mut(one)
        .unwrap()
        .world_mut()
        .get_mut::<Position>(e)
        .unwrap()
        .mm[0] += 1;
    pool.app_mut(two)
        .unwrap()
        .world_mut()
        .get_mut::<Position>(s)
        .unwrap()
        .mm[2] += 1;
    pool.step().unwrap();
    assert_eq!(
        ids(&pool.player_sight(observer).unwrap()),
        [diagonal, same_position]
    );
    assert_eq!(db.owner(g).unwrap().worker, one);
    assert_eq!(db.owner(other).unwrap().worker, two);
}

#[test]
fn five_remote_players_can_share_one_writer_and_a_dense_view_has_no_player_cap() {
    let db = Store::open(":memory:").unwrap();
    let worker = db.start_worker(1).unwrap();
    let mut app = world(&db, worker);
    let g = Id::numbered(9999);
    let mut observers = Vec::new();
    for (i, x) in [i64::MIN, -1_000_000_000_000, 0, 1_000_000_000_000, i64::MAX]
        .into_iter()
        .enumerate()
    {
        let id = Id::numbered(i as u128 + 1);
        player(app.world_mut(), id, g, PLANET_ID, [x, 0, 0]);
        observers.push(id);
    }
    let mut pool = Pool::new(db, 10000);
    pool.add(worker, app).unwrap();
    pool.step().unwrap();
    for &id in &observers {
        assert!(pool.player_sight(id).unwrap().is_empty());
    }
    let mut expected = Vec::new();
    for i in 0..140 {
        let id = Id::numbered(40 + i);
        // Separate groups keep the persistence batch within its own per-group limit.
        player(
            pool.app_mut(worker).unwrap().world_mut(),
            id,
            id,
            PLANET_ID,
            [i64::MAX - 100000, i as i64 % 2, 0],
        );
        if i % 2 == 0 {
            expected.push(id);
        }
    }
    for i in 0..135 {
        let id = Id::numbered(400 + i);
        player(
            pool.app_mut(worker).unwrap().world_mut(),
            id,
            id,
            PLANET_ID,
            [i64::MAX - 2, 0, 0],
        );
        expected.push(id);
    }
    pool.step().unwrap();
    assert_eq!(ids(&pool.player_sight(observers[4]).unwrap()), expected);
    assert!(pool.player_sight(observers[0]).unwrap().is_empty());
}

#[test]
fn configurable_contact_buffer_changes_peer_replication_while_player_sight_stays_independent() {
    let db = Store::open(":memory:").unwrap();
    let w1 = db.start_worker(1).unwrap();
    let w2 = db.start_worker(2).unwrap();
    let settings: SpaceSettings = serde_json::from_str(
        r#"{"player_half_extent_m":8,"contact_buffer_m":2.5,"forecast_ticks":0}"#,
    )
    .unwrap();
    let mut a = world_with_settings(&db, w1, settings);
    let mut b = world_with_settings(&db, w2, settings);
    let p = Id::numbered(123);
    let q = Id::numbered(456);
    player(a.world_mut(), p, p, PLANET_ID, [0; 3]);
    let moving = player(b.world_mut(), q, q, PLANET_ID, [16001, 0, 0]);
    let mut pool = Pool::new(db, 64);
    pool.add(w1, a).unwrap();
    pool.add(w2, b).unwrap();
    for (x, replicated) in [
        (16001, false),
        (16000, true),
        (18500, true),
        (18501, false),
        (18500, false),
        (16000, true),
    ] {
        pool.app_mut(w2)
            .unwrap()
            .world_mut()
            .get_mut::<Position>(moving)
            .unwrap()
            .mm[0] = x;
        pool.step().unwrap();
        pool.deliver_all().unwrap();
        assert_eq!(
            pool.directory
                .subscribers(q)
                .any(|(&to, _)| to == Recipient::Worker(w1)),
            replicated
        );
        assert!(pool.player_sight(p).unwrap().contains_key(&q));
    }
}

#[test]
fn all_public_fields_reach_the_player_across_the_wire_byte_budget() {
    #[derive(Component, Clone, serde::Serialize, serde::Deserialize)]
    struct Appearance(Vec<u8>);
    let db = Store::open(":memory:").unwrap();
    let worker = db.start_worker(1).unwrap();
    let mut app = world(&db, worker);
    let schema = 70_000;
    app.mesh_component::<Appearance>(
        schema,
        1,
        Rules {
            visibility: Visibility::Public,
            persist: false,
            migrate: false,
        },
        Default::default(),
    )
    .unwrap();
    let observer = Id::numbered(100);
    player(app.world_mut(), observer, observer, PLANET_ID, [0; 3]);
    for i in 1u8..=20 {
        let id = Id::numbered(i as u128);
        let e = player(app.world_mut(), id, id, PLANET_ID, [17, 0, 0]);
        app.world_mut()
            .entity_mut(e)
            .insert(Appearance(vec![i; 90_000]));
    }
    let mut pool = Pool::new(db, 100);
    pool.add(worker, app).unwrap();
    pool.step().unwrap();
    let view = pool.player_sight(observer).unwrap();
    assert_eq!(view.len(), 20);
    for i in 1u8..=20 {
        let record = &view[&Id::numbered(i as u128)];
        assert_eq!(record.get::<Position>(POSITION, 1).unwrap().mm, [17, 0, 0]);
        assert_eq!(
            record.get::<Appearance>(schema, 1).unwrap().0,
            vec![i; 90_000]
        );
        assert_eq!(
            record.fields.keys().copied().collect::<Vec<_>>(),
            [PLAYER, POSITION, schema]
        );
    }
}

#[test]
fn transient_subscriber_budget_ack_timeout_and_field_removal_are_isolated() {
    use std::time::{Duration, SystemTime};
    let mut app = App::new();
    app.add_plugins((
        MeshPlugin::transient(),
        WorldPlugin,
        MeshSessionPlugin::default(),
        LocalSubscriberPlugin,
    ));
    assert!(!app.world().contains_resource::<Store>());
    let actor = Id::numbered(4);
    let neighbor = Id::numbered(5);
    let owner = player(app.world_mut(), actor, actor, PLANET_ID, [0; 3]);
    let other = player(
        app.world_mut(),
        neighbor,
        neighbor,
        PLANET_ID,
        [10_000, 0, 0],
    );
    app.update();
    let dir = app.world().resource::<SessionDirectory>().clone();
    let grant = |id| SessionGrant {
        object: id,
        principal: PrincipalId(id),
        session: SessionId(id),
        control_epoch: 1,
        expires_at: SystemTime::now() + Duration::from_secs(60),
    };
    let mut views = Vec::new();
    for object in [actor, neighbor] {
        let binding = dir.admit(grant(object), ConnectionId(Id::new())).unwrap();
        let id = SubscriberId(Id::new());
        dir.subscribe(binding, Some(id)).unwrap();
        views.push((
            binding,
            SubscriberStream::open(
                &dir,
                binding,
                id,
                SubscriberPolicy {
                    events_per_poll: 3,
                    ack_timeout_ticks: 2,
                    ..Default::default()
                },
            )
            .unwrap(),
        ));
    }
    let mut enc = SubscriberEncoder::default();
    let (b, mut slow) = views.remove(0);
    let (fast_binding, mut fast) = views.remove(0);
    let first = slow.next_local(app.world(), &mut enc).unwrap().unwrap();
    let live = fast.next_local(app.world(), &mut enc).unwrap().unwrap();
    fast.applied(fast_binding, live.sequence).unwrap();
    app.world_mut().entity_mut(other).remove::<Inventory>();
    app.update();
    assert!(slow.next_local(app.world(), &mut enc).unwrap().is_none());
    let delta = fast.next_local(app.world(), &mut enc).unwrap().unwrap();
    assert!(delta.events.iter().any(|e|matches!(e,SubscriberEvent::Delta{object,fields,..} if *object==neighbor&&fields.iter().any(|(k,v)|*k==INVENTORY&&v.is_none()))));
    fast.applied(fast_binding, delta.sequence).unwrap();
    app.update();
    let reset = slow.next_local(app.world(), &mut enc).unwrap().unwrap();
    assert!(matches!(reset.events.first(), Some(SubscriberEvent::Reset)));
    assert!(slow.applied(b, first.sequence).is_err());
    slow.applied(b, reset.sequence).unwrap();
    app.world_mut().get_mut::<Position>(owner).unwrap().mm[0] = 800_000;
    app.update();
    assert!(fast
        .next_local(app.world(), &mut enc)
        .unwrap()
        .unwrap()
        .events
        .iter()
        .any(|e| matches!(e,SubscriberEvent::Leave(id) if *id==actor)));
    dir.revoke(b).unwrap();
    assert!(matches!(
        slow.next_local(app.world(), &mut enc),
        Err(is_mesh::Error::Denied)
    ));
}
