use bevy_app::App;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use zz_mesh::prelude::*;
use zz_mesh::{prepare_world, Error, Worker};

#[derive(Component, Serialize, Deserialize)]
struct Energy(u32);

#[test]
fn periodic_checkpoints_spread_groups_and_save_all_within_twenty_ticks() {
    let db = Store::open(":memory:").unwrap();
    let worker = db.start_worker(1).unwrap();
    let mut app = game(&db, worker);
    let mut objects = Vec::new();
    for n in [4, 13] {
        let id = Id::numbered(n);
        let entity = app
            .world_mut()
            .spawn((Managed::new(id, id), Energy(1)))
            .id();
        objects.push((id, entity));
    }
    app.update();
    for (_, e) in &objects {
        app.world_mut().get_mut::<Energy>(*e).unwrap().0 = 2;
    }
    let version = |a: &App, id| a.world().resource::<zz_mesh::MeshState>().version(id);
    let base = version(&app, objects[0].0);
    for _ in 0..3 {
        app.update();
    }
    assert!(version(&app, objects[0].0) > base);
    assert_eq!(version(&app, objects[1].0), base);
    for _ in 0..17 {
        app.update();
    }
    assert!(version(&app, objects[1].0) > base);
}

fn game(store: &Store, worker: Worker) -> App {
    let mut app = App::new();
    app.add_plugins(MeshPlugin::persistent(store.clone(), worker));
    app.mesh_component::<Energy>(
        1,
        1,
        Rules {
            visibility: Visibility::Public,
            migrate: true,
            persist: true,
        },
        ComponentCodec {
            encode: |energy, _| {
                if energy.0 > 100 {
                    return Err(zz_mesh::invalid("energy exceeds capacity"));
                }
                zz_mesh::encode(energy)
            },
            ..Default::default()
        },
    )
    .unwrap();
    app
}

#[test]
fn restore_is_idempotent_and_only_the_current_worker_can_recover_its_slot() {
    let db = Store::open(":memory:").unwrap();
    let first = db.start_worker(1).unwrap();
    let mut source = game(&db, first);
    let (id, group) = (Id::numbered(1), Id::numbered(2));
    source
        .world_mut()
        .spawn((Managed::new(id, group), Energy(21)));
    source.update();
    let other = db.start_worker(2).unwrap();
    assert!(matches!(
        restore_group(game(&db, other).world_mut(), group),
        Err(Error::Denied)
    ));

    let second = db.start_worker(first.slot).unwrap();
    let mut restored = game(&db, second);
    let owner = restore_group(restored.world_mut(), group).unwrap();
    assert_eq!(owner.worker, second);
    assert!(matches!(
        restore_group(source.world_mut(), group),
        Err(Error::Fenced)
    ));
    source.update();
    let old = source.world().resource::<MeshState>().entities[&id];
    assert!(source.world().get::<Simulated>(old).is_none());
    let e = restored.world().resource::<MeshState>().entities[&id];
    assert_eq!(restored.world().get::<Energy>(e).unwrap().0, 21);
    restored.world_mut().get_mut::<Energy>(e).unwrap().0 = 22;
    assert_eq!(restore_group(restored.world_mut(), group).unwrap(), owner);
    assert_eq!(restored.world().resource::<MeshState>().entities[&id], e);
    assert_eq!(restored.world().get::<Energy>(e).unwrap().0, 22);
}

#[test]
fn a_failing_game_codec_discards_the_entire_staging_group_before_cutover() {
    let db = Store::open(":memory:").unwrap();
    let worker = db.start_worker(1).unwrap();
    let mut app = game(&db, worker);
    let group = Id::numbered(3);
    for id in [Id::numbered(1), Id::numbered(2)] {
        app.world_mut().spawn((Managed::new(id, group), Energy(10)));
    }
    app.update();
    let before = app.world().resource::<MeshState>().entities.clone();
    let mut snapshot = db.snapshot(group).unwrap();
    snapshot.records[1].fields.get_mut(&1).unwrap().bytes = zz_mesh::encode(&Energy(101)).unwrap();
    assert!(
        matches!(prepare_world(app.world_mut(), &snapshot), Err(Error::Invalid(reason)) if reason == "energy exceeds capacity")
    );
    let w = app.world_mut();
    assert_eq!(
        w.query_filtered::<Entity, With<Staged>>().iter(w).count(),
        0
    );
    assert_eq!(w.resource::<MeshState>().entities, before);
    for e in before.values() {
        assert!(w.get::<Simulated>(*e).is_some());
        assert_eq!(w.get::<Energy>(*e).unwrap().0, 10);
    }
    assert_eq!(
        w.resource::<MeshState>().authority(group),
        Some(db.owner(group).unwrap())
    );
}

#[test]
fn legacy_store_keeps_world_and_game_metadata_and_future_format_fails_closed() {
    let path = std::env::temp_dir().join(format!("zz-mesh-{}.sqlite", Id::new()));
    let group = Id::numbered(7);
    let id = Id::numbered(8);
    let db = Store::open(&path).unwrap();
    let worker = db.start_worker(6).unwrap();
    let owner = db.create_group(group, worker).unwrap();
    let revision = db
        .checkpoint(group, owner, 0, vec![zz_mesh::Record::new(id, group)])
        .unwrap();
    drop(db);
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("UPDATE documents SET body=substr(body,1,length(body)-37); DROP TABLE zz_mesh_format; PRAGMA user_version=43; CREATE TABLE game_log(line TEXT); INSERT INTO game_log VALUES('saved');").unwrap();
    drop(raw);

    let upgraded = Store::open(&path).unwrap();
    assert_eq!(upgraded.owner(group).unwrap(), owner);
    assert_eq!(upgraded.snapshot(group).unwrap().revision, revision);
    assert!(matches!(
        upgraded.lookup(id).unwrap(),
        zz_mesh::Lookup::Present(_)
    ));
    drop(upgraded);
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        43
    );
    assert_eq!(
        raw.query_row("SELECT line FROM game_log", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "saved"
    );
    raw.execute_batch(
        "UPDATE zz_mesh_format SET version=99; DROP INDEX doc_unit; PRAGMA journal_mode=DELETE;",
    )
    .unwrap();
    drop(raw);
    assert!(
        matches!(Store::open(&path), Err(Error::Invalid(reason)) if reason.contains("format 99"))
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        raw.query_row("SELECT version FROM zz_mesh_format", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        99
    );
    assert_eq!(
        raw.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    assert!(!raw
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='doc_unit')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    drop(raw);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_workers_open_one_new_world() {
    let path = std::env::temp_dir().join(format!("zz-mesh-{}.sqlite", Id::new()));
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|threads| {
        for slot in 1..=4 {
            let (path, start) = (&path, &start);
            threads.spawn(move || {
                start.wait();
                let store = Store::open(path).unwrap();
                let worker = store.start_worker(slot).unwrap();
                store
                    .create_group(Id::numbered(slot.into()), worker)
                    .unwrap();
            });
        }
    });
    let db = Store::open(&path).unwrap();
    for slot in 1..=4 {
        assert_eq!(db.owned(slot).unwrap(), vec![Id::numbered(slot.into())]);
    }
    drop(db);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn draining_a_slot_aborts_inbound_frozen_work_and_persists_across_incarnations() {
    use zz_mesh::*;
    let db = Store::open(":memory:").unwrap();
    let a = db.start_worker(1).unwrap();
    let b = db.start_worker(2).unwrap();
    let g = Id::new();
    let owner = db.create_group(g, a).unwrap();
    let ticket = Handoff {
        id: Id::new(),
        group: g,
        source: owner,
        target: b,
    };
    db.begin_handoff(ticket).unwrap();
    db.freeze(ticket, 0, &[]).unwrap();
    db.set_draining(b, true).unwrap();
    assert_eq!(db.handoff_phase(ticket).unwrap(), Phase::Aborted);
    assert!(db.switch(ticket).is_err());
    assert_eq!(db.checkpoint(g, owner, 0, vec![]).unwrap(), 1);
    let next = db.start_worker(2).unwrap();
    let retry = Handoff {
        id: Id::new(),
        target: next,
        ..ticket
    };
    assert!(matches!(db.begin_handoff(retry), Err(Error::Busy(_))));
    assert!(matches!(
        db.create_group(Id::new(), next),
        Err(Error::Busy(_))
    ));
    db.set_draining(next, false).unwrap();
    db.begin_handoff(retry).unwrap();
}

#[test]
fn async_checkpoint_is_bounded_and_a_late_commit_preserves_newer_live_fields() {
    use zz_mesh::{AsyncCheckpoints, BulkExecutor};
    let file = std::env::temp_dir().join(format!("zz-async-{}.sqlite", Id::new()));
    let db = Store::open(&file).unwrap();
    let worker = db.start_worker(1).unwrap();
    let mut app = game(&db, worker);
    let executor = BulkExecutor::new(2);
    let (release, wait) = std::sync::mpsc::channel();
    let held = executor
        .submit(move || {
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
    app.insert_resource(AsyncCheckpoints::new(
        executor,
        1.try_into().unwrap(),
        65536.try_into().unwrap(),
    ));
    let id = Id::numbered(1);
    let e = app
        .world_mut()
        .spawn((Managed::new(id, id), Energy(5)))
        .id();
    app.update();
    assert_eq!(app.world().resource::<AsyncCheckpoints>().pending().0, 1);
    assert_eq!(db.snapshot(id).unwrap().revision, 0);
    app.world_mut().get_mut::<Energy>(e).unwrap().0 = 9;
    for _ in 0..21 {
        app.update();
    }
    assert_eq!(app.world().resource::<AsyncCheckpoints>().pending().0, 1);
    release.send(()).unwrap();
    held.recv().unwrap().unwrap();
    checkpoint_group(app.world_mut(), id).unwrap();
    assert_eq!(app.world().get::<Energy>(e).unwrap().0, 9);
    assert_eq!(
        db.snapshot(id).unwrap().records[0]
            .get::<Energy>(1, 1)
            .unwrap()
            .0,
        9
    );
    let newer = db.start_worker(1).unwrap();
    let mut replacement = game(&db, newer);
    restore_group(replacement.world_mut(), id).unwrap();
    let e = replacement.world().resource::<MeshState>().entities[&id];
    assert_eq!(replacement.world().get::<Energy>(e).unwrap().0, 9);
    drop(app);
    drop(replacement);
    drop(db);
    let raw = rusqlite::Connection::open(&file).unwrap();
    let mut body: Vec<u8> = raw
        .query_row("SELECT body FROM documents", [], |r| r.get(0))
        .unwrap();
    body[0] ^= 1;
    raw.execute("UPDATE documents SET body=?", [body]).unwrap();
    drop(raw);
    let damaged = Store::open(&file).unwrap();
    assert!(matches!(damaged.snapshot(id),Err(Error::Invalid(s)) if s.contains("checksum")));
    drop(damaged);
    std::fs::remove_file(file).unwrap();
}
