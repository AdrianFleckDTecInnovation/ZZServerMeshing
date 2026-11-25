use bevy_app::App;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use zz_mesh::prelude::*;
use zz_mesh::{prepare_world, Error, Worker};

#[derive(Component, Serialize, Deserialize)]
struct Energy(u32);

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
    raw.execute_batch("DROP TABLE zz_mesh_format; PRAGMA user_version=43; CREATE TABLE game_log(line TEXT); INSERT INTO game_log VALUES('saved');").unwrap();
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
