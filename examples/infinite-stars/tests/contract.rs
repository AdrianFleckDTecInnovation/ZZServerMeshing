use bevy::prelude::{Component, With};
use infinite_stars::{data::*, prediction::*, *};
use zz_mesh::*;
#[test]
fn first_boot_resumes_an_empty_group_and_never_reseeds_a_retired_player() {
    let db = Store::open(":memory:").unwrap();
    let interrupted = db.start_worker(1).unwrap();
    db.create_group(GROUP, interrupted).unwrap();
    let (_, mut app) = host::start_node(&db, 1).unwrap();
    assert_eq!(db.snapshot(GROUP).unwrap().records.len(), 4);
    let assembly = app.world().resource::<MeshState>().entities[&PLANET_ID.child(88)];
    assert_eq!(
        app.world()
            .get::<Assembly>(assembly)
            .unwrap()
            .local_pieces
            .len(),
        1
    );

    let player = app.world().resource::<MeshState>().entities[&ACTOR];
    app.world_mut().despawn(player);
    app.update();
    drop(app);
    let (_, restarted) = host::start_node(&db, 1).unwrap();
    assert_eq!(db.lookup(ACTOR).unwrap(), Lookup::Deleted);
    assert!(!restarted
        .world()
        .resource::<MeshState>()
        .entities
        .contains_key(&ACTOR));
}

fn setup() -> (Store, Worker, Worker, bevy::prelude::App, Id) {
    let db = Store::open(":memory:").unwrap();
    let a = db.start_worker(1).unwrap();
    let b = db.start_worker(2).unwrap();
    let mut app = world(&db, a);
    let chunk = seed(&mut app).unwrap();
    app.update();
    (db, a, b, app, chunk)
}
fn decided(r: &Reply) -> &Receipt {
    let Reply::Decided(r) = r else {
        panic!("undecided {r:?}")
    };
    r
}
fn committed(r: &Reply) -> Effects {
    let Outcome::Committed(data) = &decided(r).outcome else {
        panic!("rejected: {r:?}")
    };
    decode(data).unwrap()
}
fn stock(db: &Store, id: Id) -> Inventory {
    let Lookup::Present(r) = db.lookup(id).unwrap() else {
        panic!()
    };
    r.get(INVENTORY, 1).unwrap()
}
#[test]
fn prediction_batch_economy_and_reconnect() {
    let (db, _, _, mut app, chunk) = setup();
    let a = db.owner(GROUP).unwrap();
    let id = Id::new();
    db.session(id, ACTOR).unwrap();
    let mut cl = Prediction::new(id, GROUP, stock(&db, ACTOR));
    let mine = cl
        .submit(
            Predicted::Mine(MineBlocks(vec![
                Target { chunk, index: 0 },
                Target { chunk, index: 1 },
            ])),
            None,
        )
        .unwrap();
    let drop = cl
        .submit(
            Predicted::Drop(DropItems(vec![
                Stack {
                    kind: STONE,
                    count: 7,
                },
                Stack {
                    kind: STONE,
                    count: 2,
                },
            ])),
            Some(1),
        )
        .unwrap();
    let fail = cl
        .submit(
            Predicted::Drop(DropItems(vec![Stack {
                kind: STONE,
                count: 7,
            }])),
            None,
        )
        .unwrap();
    let dependent = cl
        .submit(
            Predicted::Build(Build(vec![Target { chunk, index: 0 }])),
            Some(3),
        )
        .unwrap();
    assert_eq!(cl.visible().inventory.0[&STONE], 3);
    let batch = vec![mine, drop, fail, dependent];
    let out = db
        .execute(app.world().resource::<Actions>(), ACTOR, a, 2000, &batch)
        .unwrap();
    let gain = committed(&out[0]);
    assert_eq!(gain.blocks.len(), 2);
    let dropped = committed(&out[1]);
    assert!(matches!(decided(&out[2]).outcome, Outcome::Rejected(_)));
    assert!(matches!(decided(&out[3]).outcome, Outcome::Rejected(_)));
    for r in &out {
        cl.receive(decided(r)).unwrap();
        cl.receive(decided(r)).unwrap();
    }
    assert!(cl.retry().is_empty());
    assert_eq!(cl.visible().drops.len(), 2);
    assert_eq!(stock(&db, ACTOR).0[&STONE], 3);
    assert_eq!(
        out,
        db.execute(app.world().resource::<Actions>(), ACTOR, a, 3000, &batch)
            .unwrap()
    );
    let pickup = cl
        .submit(
            Predicted::Pickup(Pickup(dropped.drops.iter().map(|(id, _)| *id).collect())),
            None,
        )
        .unwrap();
    let build = cl
        .submit(
            Predicted::Build(Build(vec![Target { chunk, index: 0 }])),
            Some(5),
        )
        .unwrap();
    let out = db
        .execute(
            app.world().resource::<Actions>(),
            ACTOR,
            a,
            3200,
            &[pickup, build],
        )
        .unwrap();
    for r in &out {
        cl.receive(decided(r)).unwrap();
    }
    assert_eq!(stock(&db, ACTOR).0[&STONE], 11);
    assert!(cl.visible().drops.is_empty());
    assert!(matches!(
        db.lookup(dropped.drops[0].0).unwrap(),
        Lookup::Deleted
    ));
    app.update();
    let player = app.world().resource::<MeshState>().entities[&ACTOR];
    assert_eq!(
        app.world().get::<Inventory>(player),
        Some(&cl.visible().inventory)
    );
    let m = app.world().resource::<Providers>().get(VOXELS).unwrap();
    let desc = m
        .describe(DataKey {
            provider: VOXELS,
            object: PLANET_ID,
            part: 0,
        })
        .unwrap();
    let data = m.snapshot(&desc).unwrap();
    assert_eq!(data[1], 0);
    assert_ne!(data[0], 0);
}
#[test]
fn exact_private_state_and_handoff_while_data_changes() {
    let (db, a, b, mut app, c) = setup();
    let player = app.world().resource::<MeshState>().entities[&ACTOR];
    app.world_mut()
        .entity_mut(player)
        .get_mut::<Position>()
        .unwrap()
        .mm[0] = 167;
    let public = capture(
        app.world(),
        ACTOR,
        View::Client {
            principal: Id::new(),
            owner: ACTOR,
            detail: Detail::Coarse,
        },
    )
    .unwrap();
    assert!(!public.fields.contains_key(&INVENTORY));
    assert_eq!(public.get::<Position>(POSITION, 1).unwrap().mm[0], 100);
    let private = capture(
        app.world(),
        ACTOR,
        View::Client {
            principal: ACTOR,
            owner: ACTOR,
            detail: Detail::Coarse,
        },
    )
    .unwrap();
    assert!(private.fields.contains_key(&INVENTORY));
    let mut pool = Pool::new(db.clone(), 2048);
    pool.add(a, app).unwrap();
    pool.add(b, world(&db, b)).unwrap();
    let mut m = pool.begin(GROUP, b, 16384).unwrap();
    let session = Id::new();
    db.session(session, ACTOR).unwrap();
    let old = db.owner(GROUP).unwrap();
    let mine = Intent::typed(
        ActionKey { session, seq: 1 },
        GROUP,
        &MineBlocks(vec![Target { chunk: c, index: 1 }]),
    )
    .unwrap();
    let result = db
        .execute(
            pool.app(a).unwrap().world().resource::<Actions>(),
            ACTOR,
            old,
            3000,
            &[mine.clone()],
        )
        .unwrap();
    let extra = Id::new();
    pool.app_mut(a).unwrap().world_mut().spawn((
        Managed::new(extra, GROUP),
        Position {
            frame: PLANET_ID,
            mm: [3, 4, 5],
        },
    ));
    pool.step().unwrap();
    let cost = loop {
        if let Some(c) = pool.poll(&mut m).unwrap() {
            break c;
        }
        std::thread::yield_now()
    };
    assert_eq!(cost.authority.worker, b);
    assert!(m.rounds >= 2);
    let dst = pool.app(b).unwrap().world();
    let entity = dst.resource::<MeshState>().entities[&ACTOR];
    assert_eq!(dst.get::<Position>(entity).unwrap().mm[0], 167);
    assert!(dst.get::<Simulated>(entity).is_some());
    assert!(dst.resource::<MeshState>().entities.contains_key(&extra));
    assert_eq!(dst.get::<Inventory>(entity).unwrap().0[&STONE], 11);
    assert_eq!(
        db.execute(
            dst.resource::<Actions>(),
            ACTOR,
            cost.authority,
            4000,
            &[mine]
        )
        .unwrap(),
        result
    );
    let stale = Intent::typed(
        ActionKey { session, seq: 2 },
        GROUP,
        &DropItems(vec![Stack {
            kind: STONE,
            count: 1,
        }]),
    )
    .unwrap();
    assert!(matches!(
        db.execute(dst.resource::<Actions>(), ACTOR, old, 4000, &[stale])
            .unwrap()[0],
        Reply::Retry(_)
    ));
}
#[test]
fn bulk_baselines_resume_pins_and_incompatible_schema() {
    let (db, a, b, app, cid) = setup();
    let provider = app.world().resource::<Providers>().get(VOXELS).unwrap();
    let key = DataKey {
        provider: VOXELS,
        object: PLANET_ID,
        part: 0,
    };
    let old = provider.describe(key).unwrap();
    let data = provider.snapshot(&old).unwrap();
    let mut cache = BlobCache::new(16384);
    assert_eq!(cache.fetch(&old, provider.as_ref()).unwrap(), 4096);
    assert_eq!(cache.fetch(&old, provider.as_ref()).unwrap(), 0);
    let mut download = Download::new(old.clone(), 8192).unwrap();
    download.append(0, &data[..512]).unwrap();
    assert!(download.append(513, &[2]).is_err());
    download.append(0, &data[..512]).unwrap();
    download.append(download.offset(), &data[512..]).unwrap();
    download.finish(&mut cache, provider.as_ref()).unwrap();
    let mut bad = Download::new(old.clone(), 8192).unwrap();
    bad.append(0, &vec![0; 4096]).unwrap();
    assert!(bad.finish(&mut cache, provider.as_ref()).is_err());
    let s = Id::new();
    db.session(s, ACTOR).unwrap();
    let req = Intent::typed(
        ActionKey { session: s, seq: 1 },
        GROUP,
        &MineBlocks(vec![Target {
            chunk: cid,
            index: 3,
        }]),
    )
    .unwrap();
    committed(
        &db.execute(
            app.world().resource::<Actions>(),
            ACTOR,
            db.owner(GROUP).unwrap(),
            4000,
            &[req],
        )
        .unwrap()[0],
    );
    let next = provider.describe(key).unwrap();
    let patch = provider.delta(&old, &next).unwrap().unwrap();
    assert!(patch.apply(&next, &data, &next).is_err());
    let n = cache.fetch(&next, provider.as_ref()).unwrap();
    assert!(n > 0 && n < 512);
    let independent = VoxelProvider::new(db.clone());
    let mut fresh = BlobCache::new(16384);
    fresh.install(&old, data, &independent).unwrap();
    assert_eq!(fresh.fetch(&next, &independent).unwrap(), 4096);
    let mut target = world(&db, b);
    let mut bad_snapshot = db.snapshot(GROUP).unwrap();
    assert!(prepare_world(target.world_mut(), &bad_snapshot).is_err());
    bad_snapshot.required.clear();
    bad_snapshot.records[0]
        .fields
        .insert(999, Field::new(1, &0u8).unwrap());
    assert!(prepare_world(target.world_mut(), &bad_snapshot).is_err());
    assert_eq!(
        target
            .world_mut()
            .query_filtered::<&Managed, With<Staged>>()
            .iter(target.world())
            .count(),
        0
    );
    let _ = a;
}
#[test]
fn stale_preparation_timeout_and_crash_at_each_phase() {
    for phase in 0..5 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("world.db");
        let db = Store::open(&path).unwrap();
        let a = db.start_worker(1).unwrap();
        let b = db.start_worker(2).unwrap();
        let mut src = world(&db, a);
        seed(&mut src).unwrap();
        let s = db.snapshot(GROUP).unwrap();
        let owner = db.owner(GROUP).unwrap();
        let t = Handoff {
            id: Id::new(),
            group: GROUP,
            source: owner,
            target: b,
        };
        db.begin_handoff(t).unwrap();
        let mut dst = world(&db, b);
        for m in &s.required {
            let p = src
                .world()
                .resource::<Providers>()
                .get(m.key.provider)
                .unwrap();
            dst.world_mut()
                .resource_mut::<BlobCache>()
                .fetch(m, p.as_ref())
                .unwrap();
        }
        let early = prepare_world(dst.world_mut(), &s).unwrap();
        assert!(db.acknowledge(t, b, &early).is_err());
        if phase >= 1 {
            db.freeze(t, s.revision, &s.records).unwrap();
        }
        if phase >= 2 {
            db.acknowledge(t, b, &early).unwrap();
        }
        if phase >= 3 {
            db.switch(t).unwrap();
        }
        if phase == 4 {
            early
                .activate(dst.world_mut(), &db, db.owner(GROUP).unwrap())
                .unwrap();
        }
        drop(dst);
        drop(src);
        drop(db);
        let db = Store::open(path).unwrap();
        if phase < 3 {
            let restarted = db.start_worker(1).unwrap();
            let current = db.recover(GROUP, restarted).unwrap();
            assert_ne!(current, owner);
            assert_eq!(db.handoff_phase(t).unwrap(), Phase::Aborted);
            assert!(db.switch(t).is_err());
        } else {
            assert_eq!(db.abort_handoff(t).unwrap(), Phase::Switched);
            let restarted = db.start_worker(2).unwrap();
            db.recover(GROUP, restarted).unwrap();
            assert_eq!(db.owner(GROUP).unwrap().worker, restarted);
        }
        assert!(db.checkpoint(GROUP, owner, s.revision, vec![]).is_err());
        assert_eq!(stock(&db, ACTOR).0[&STONE], 10);
    }
}
#[test]
fn compacted_tombstone_and_old_actions_never_reappear() {
    let (db, _a, _, app, _) = setup();
    let owner = db.owner(GROUP).unwrap();
    let g = GROUP;
    let gone = Id::new();
    let mut rev = db.snapshot(g).unwrap().revision;
    let r = Record::new(gone, g);
    rev = db.checkpoint(g, owner, rev, vec![r]).unwrap();
    let mut del = Record::new(gone, g);
    del.revision = rev;
    del.deleted = true;
    rev = db.checkpoint(g, owner, rev, vec![del]).unwrap();
    db.compact(g, owner, rev).unwrap();
    assert_eq!(db.lookup(gone).unwrap(), Lookup::Deleted);
    assert_eq!(db.lookup(Id::new()).unwrap(), Lookup::Unknown);
    assert!(db
        .checkpoint(g, owner, rev, vec![Record::new(gone, g)])
        .is_err());
    let s = Id::new();
    db.session(s, ACTOR).unwrap();
    let make = |seq| Intent::typed(ActionKey { session: s, seq }, g, &DropItems(vec![])).unwrap();
    for seq in 1..=RECEIPTS + 3 {
        let _ = db
            .execute(
                app.world().resource::<Actions>(),
                ACTOR,
                owner,
                1,
                &[make(seq)],
            )
            .unwrap();
    }
    assert!(matches!(
        db.execute(
            app.world().resource::<Actions>(),
            ACTOR,
            owner,
            1,
            &[make(1)]
        )
        .unwrap()[0],
        Reply::Resync(_)
    ));
}
#[derive(Component, serde::Serialize, serde::Deserialize)]
struct Foreign(Id);
#[test]
fn public_extension_and_unresolved_required_reference() {
    let (db, a, b, mut app, _) = setup();
    let rules = Rules {
        visibility: Visibility::Workers,
        migrate: true,
        persist: true,
    };
    app.mesh_component::<Foreign>(
        851,
        3,
        rules,
        ComponentCodec {
            references: |v| vec![Reference::RequiredLocal(v.0)],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(app
        .mesh_component::<Foreign>(851, 4, rules, Default::default())
        .is_err());
    let group = Id::new();
    let obj = Id::new();
    app.world_mut()
        .spawn((Managed::new(obj, group), Foreign(Id::new())));
    app.update();
    let mut target = world(&db, b);
    target
        .mesh_component::<Foreign>(
            851,
            3,
            rules,
            ComponentCodec {
                references: |v| vec![Reference::RequiredLocal(v.0)],
                ..Default::default()
            },
        )
        .unwrap();
    let snap = db.snapshot(group).unwrap();
    assert!(prepare_world(target.world_mut(), &snap).is_err());
    let mut pool = Pool::new(db.clone(), 128);
    pool.add(a, app).unwrap();
    pool.add(b, target).unwrap();
    assert!(pool.transfer(group, b, 0).is_err());
    assert_eq!(db.owner(group).unwrap().worker, a);
}

#[test]
fn ordered_handoff_keeps_live_motion_durable_and_remaps_local_references() {
    use infinite_stars::movement::Drive;
    let (db, a, b, mut app, chunk) = setup();
    let session = Id::new();
    let sample = Input {
        session,
        sequence: 7,
        at_ms: 1000,
        axes: [32767, 0, 0],
    };
    assert_eq!(
        apply_input(app.world_mut(), ACTOR, &sample, 1020)
            .unwrap()
            .sequence,
        7
    );
    app.update();
    let id = app.world().resource::<MeshState>().entities[&ACTOR];
    let before = app.world().get::<Position>(id).unwrap().mm[0];
    assert_eq!(before, 250);
    assert!(apply_input(app.world_mut(), ACTOR, &sample, 1600).is_err());
    let mut pool = Pool::new(db.clone(), 1024);
    pool.add(a, app).unwrap();
    pool.add(b, world(&db, b)).unwrap();
    let c = pool.transfer(GROUP, b, 16000).unwrap();
    let dst = pool.app_mut(b).unwrap().world_mut();
    let id = dst.resource::<MeshState>().entities[&ACTOR];
    let assembly = dst.resource::<MeshState>().entities[&PLANET_ID.child(88)];
    assert_eq!(
        dst.get::<Assembly>(assembly).unwrap().local_pieces,
        vec![dst.resource::<MeshState>().entities[&chunk]]
    );
    assert_eq!(dst.get::<Drive>(id).unwrap().remaining, 3);
    apply_input(dst, ACTOR, &sample, 1030).unwrap();
    assert_eq!(dst.get::<Drive>(id).unwrap().remaining, 3);
    let Lookup::Present(durable) = db.lookup(ACTOR).unwrap() else {
        panic!()
    };
    assert_eq!(durable.get::<Position>(POSITION, 1).unwrap().mm[0], before);
    let restarted = db.start_worker(b.slot).unwrap();
    db.recover(GROUP, restarted).unwrap();
    let mut recovered = world(&db, restarted);
    infinite_stars::host::load_group(&mut recovered, &db, GROUP, restarted).unwrap();
    assert_ne!(c.authority, db.owner(GROUP).unwrap());
    let id = recovered.world().resource::<MeshState>().entities[&ACTOR];
    assert_eq!(recovered.world().get::<Drive>(id).unwrap().sequence, 7);
}
#[test]
fn membership_departure_and_deleted_member_during_prefetch() {
    let (db, a, b, mut app, _) = setup();
    let out = Id::new();
    let gone = Id::new();
    let g = Id::new();
    for id in [gone, out] {
        app.world_mut().spawn((
            Managed::new(id, GROUP),
            Position {
                frame: PLANET_ID,
                mm: [0; 3],
            },
        ));
    }
    app.update();
    let dest = db.create_group(g, a).unwrap();
    let mut pool = Pool::new(db.clone(), 1024);
    pool.add(a, app).unwrap();
    pool.add(b, world(&db, b)).unwrap();
    let mut m = pool.begin(GROUP, b, 16384).unwrap();
    let owner = db.owner(GROUP).unwrap();
    db.regroup(
        out,
        (GROUP, owner, db.snapshot(GROUP).unwrap().revision),
        (g, dest, 0),
        &[],
    )
    .unwrap();
    let app = pool.app_mut(a).unwrap();
    let old = app.world().resource::<MeshState>().entities[&gone];
    app.world_mut().despawn(old);
    pool.step().unwrap();
    while pool.poll(&mut m).unwrap().is_none() {
        std::thread::yield_now();
    }
    let target = pool.app(b).unwrap().world();
    assert_eq!(local_object(target, gone).unwrap(), LocalObject::Deleted);
    assert_eq!(local_object(target, out).unwrap(), LocalObject::Unloaded);
    assert_eq!(db.group_of(out).unwrap(), Some(g));
    let src = pool.app(a).unwrap().world();
    let e = src.resource::<MeshState>().entities[&out];
    assert_eq!(src.get::<Managed>(e).unwrap().group, g);
}
#[test]
fn measured_placement_moves_feasible_load_and_drain_reports_indivisible_group() {
    let (db, a, b, mut app, _) = setup();
    let small = Id::numbered(9000);
    app.world_mut().spawn((
        Managed::new(small, small),
        Position {
            frame: PLANET_ID,
            mm: [1000000, 0, 0],
        },
    ));
    app.update();
    let mut p = Pool::new(db.clone(), 1024);
    p.add(a, app).unwrap();
    p.add(b, world(&db, b)).unwrap();
    p.policy = BalancePolicy {
        every: 1,
        dwell: 0,
        tick_budget_us: 100000,
        transfer_bytes: 1 << 20,
        memory_bytes: 1 << 20,
    };
    p.app_mut(a)
        .unwrap()
        .world_mut()
        .resource_mut::<GroupTimings>()
        .0
        .extend([(GROUP, 200000), (small, 1000)]);
    for _ in 0..150 {
        p.step().unwrap();
        p.deliver_all().unwrap();
        if db.owner(small).unwrap().worker == b {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(db.owner(small).unwrap().worker, b);
    assert_eq!(db.owner(GROUP).unwrap().worker, a);
    p.step().unwrap();
    assert!(p.overloaded.contains(&GROUP));
    let caps = [a, b].map(|w| Capacity {
        worker: w,
        tick_us: 100,
        budget_us: 2000,
        used: 0,
        memory: 100000,
        draining: false,
    });
    let load = GroupLoad {
        group: small,
        owner: b,
        cost_us: 100,
        bytes: 100,
        moved_at: 0,
        affinity: Default::default(),
        cached: Default::default(),
    };
    let done = p.drain(b, &caps, &[load], 20000).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(db.owner(small).unwrap().worker, a);
}
#[test]
fn pins_and_bounded_preparation_survive_gc() {
    let (db, a, b, app, _) = setup();
    let provider = app.world().resource::<Providers>().get(VOXELS).unwrap();
    let m = provider
        .describe(DataKey {
            provider: VOXELS,
            object: PLANET_ID,
            part: 0,
        })
        .unwrap();
    let blueprint = BlueprintProvider::manifest();
    let mut cache = BlobCache::new(4096);
    cache.fetch(&m, provider.as_ref()).unwrap();
    let pin = cache.pin(&m).unwrap();
    let mut clone = cache.clone();
    assert!(clone.fetch(&blueprint, &BlueprintProvider).is_err());
    drop(pin);
    assert_eq!(clone.fetch(&blueprint, &BlueprintProvider).unwrap(), 16);
    assert!(cache.ready(&m));
    let mut pool = Pool::new(db.clone(), 1024);
    pool.add(a, app).unwrap();
    pool.add(b, world(&db, b)).unwrap();
    assert!(pool.transfer(GROUP, b, 1).is_err());
    assert_eq!(db.owner(GROUP).unwrap().worker, a);
    pool.transfer(GROUP, b, 16000).unwrap();
}

#[test]
fn directed_worlds_retry_lost_ack_and_unload_on_split() {
    let (db, a, b, app, _) = setup();
    let mut dst = world(&db, b);
    let neighbor = Id::new();
    dst.world_mut().spawn((
        Managed::new(neighbor, neighbor),
        Player,
        Position {
            frame: PLANET_ID,
            mm: [3100, 0, 0],
        },
    ));
    let mut pool = Pool::new(db.clone(), 512);
    pool.add(a, app).unwrap();
    pool.add(b, dst).unwrap();
    pool.step().unwrap();
    let near = pool
        .messages
        .iter()
        .find(|m| m.to == Recipient::Worker(b) && m.update.object == ACTOR)
        .unwrap()
        .clone();
    let rest = std::mem::take(&mut pool.messages);
    for m in rest {
        pool.deliver_with_ack(m, false).unwrap();
    }
    let dst = pool.app(b).unwrap().world();
    let local = dst.resource::<MeshState>().entities[&ACTOR];
    assert!(dst.get::<Replica>(local).is_some());
    assert!(dst.get::<Simulated>(local).is_none());
    assert!(dst.get::<Inventory>(local).is_none());
    for _ in 0..4 {
        pool.step().unwrap();
    }
    assert!(pool
        .messages
        .iter()
        .any(|m| m.update.object == ACTOR && m.update.serial == near.update.serial));
    pool.deliver_all().unwrap();
    for _ in 0..5 {
        pool.step().unwrap();
    }
    assert!(pool.messages.is_empty());
    let dst = pool.app_mut(b).unwrap().world_mut();
    let e = dst.resource::<MeshState>().entities[&neighbor];
    dst.get_mut::<Position>(e).unwrap().mm[2] = 1000000;
    pool.step().unwrap();
    assert_eq!(pool.directory.islands().len(), 2);
    assert_eq!(
        local_object(pool.app(b).unwrap().world(), ACTOR).unwrap(),
        LocalObject::Unloaded
    );
    assert!(!pool.deliver(near.clone()).unwrap());
    let owner = db.owner(GROUP).unwrap();
    let src = pool.app_mut(a).unwrap().world_mut();
    let actor = src.resource::<MeshState>().entities[&ACTOR];
    src.get_mut::<Position>(actor).unwrap().mm = [123, 0, 0];
    let dst = pool.app_mut(b).unwrap().world_mut();
    dst.get_mut::<Position>(e).unwrap().mm[2] = 0;
    pool.step().unwrap();
    assert!(!pool.deliver(near.clone()).unwrap());
    pool.deliver_all().unwrap();
    assert!(!pool.deliver(near).unwrap());
    let dst = pool.app(b).unwrap().world();
    let replica = dst.resource::<MeshState>().entities[&ACTOR];
    assert_eq!(dst.get::<Position>(replica).unwrap().mm, [123, 0, 0]);
    assert!(dst.get::<Inventory>(replica).is_none());
    assert_eq!(pool.directory.islands().len(), 1);
    assert_eq!(db.owner(GROUP).unwrap(), owner);
}
#[test]
fn old_ready_and_lost_decision_cannot_resume_a_stale_source() {
    let (db, a, b, app, _) = setup();
    let mut target = world(&db, b);
    let s = db.snapshot(GROUP).unwrap();
    let owner = db.owner(GROUP).unwrap();
    let t = Handoff {
        id: Id::new(),
        group: GROUP,
        source: owner,
        target: b,
    };
    db.begin_handoff(t).unwrap();
    for m in &s.required {
        let p = app
            .world()
            .resource::<Providers>()
            .get(m.key.provider)
            .unwrap();
        target
            .world_mut()
            .resource_mut::<BlobCache>()
            .fetch(m, p.as_ref())
            .unwrap();
    }
    let early = prepare_world(target.world_mut(), &s).unwrap();
    let key = ActionKey {
        session: Id::new(),
        seq: 1,
    };
    db.session(key.session, ACTOR).unwrap();
    let action = Intent::typed(
        key,
        GROUP,
        &DropItems(vec![Stack {
            kind: STONE,
            count: 3,
        }]),
    )
    .unwrap();
    db.execute(
        app.world().resource::<Actions>(),
        ACTOR,
        owner,
        2000,
        &[action],
    )
    .unwrap();
    let s = db.snapshot(GROUP).unwrap();
    db.freeze(t, s.revision, &s.records).unwrap();
    assert!(db.acknowledge(t, b, &early).is_err());
    assert!(db.switch(t).is_err());
    assert!(db.checkpoint(GROUP, owner, s.revision, vec![]).is_err());
    early.discard(target.world_mut());
    let final_state = prepare_world(target.world_mut(), &s).unwrap();
    db.acknowledge(t, b, &final_state).unwrap();
    let grant = db.switch(t).unwrap();
    assert_eq!(db.abort_handoff(t).unwrap(), Phase::Switched);
    assert_ne!(grant.worker, a);
    assert!(db.checkpoint(GROUP, owner, s.revision, vec![]).is_err());
}
