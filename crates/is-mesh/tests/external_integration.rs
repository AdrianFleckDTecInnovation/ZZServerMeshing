use bevy_app::App;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Barrier},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use is_mesh::*;

fn grant(actor: Id, principal: PrincipalId, session: SessionId, epoch: u64) -> SessionGrant {
    SessionGrant {
        object: actor,
        principal,
        session,
        control_epoch: epoch,
        expires_at: SystemTime::now() + Duration::from_secs(60),
    }
}

#[test]
fn independent_store_connections_arbitrate_one_control_epoch_and_keep_its_fence_after_reopen() {
    let path = std::env::temp_dir().join(format!("zz-control-{}.sqlite", Id::new()));
    let a = Id::new();
    let principal = PrincipalId(Id::new());
    let session = SessionId(Id::new());
    let stores = [Store::open(&path).unwrap(), Store::open(&path).unwrap()];
    let barrier = Arc::new(Barrier::new(2));
    let mut jobs = vec![];
    for db in stores {
        let gate = barrier.clone();
        jobs.push(std::thread::spawn(move || {
            let directory = SessionDirectory::persistent(db);
            gate.wait();
            directory.admit(grant(a, principal, session, 1), ConnectionId(Id::new()))
        }));
    }
    let winners: Vec<_> = jobs
        .into_iter()
        .filter_map(|j| j.join().unwrap().ok())
        .collect();
    assert_eq!(winners.len(), 1);
    {
        let directory = SessionDirectory::persistent(Store::open(&path).unwrap());
        let b = directory
            .admit(grant(a, principal, session, 2), ConnectionId(Id::new()))
            .unwrap();
        assert!(matches!(directory.suspend(winners[0]), Err(Error::Fenced)));
        assert!(directory
            .admit(
                grant(a, PrincipalId(Id::new()), session, 3),
                ConnectionId(Id::new())
            )
            .is_err());
        assert!(directory
            .admit(
                grant(Id::new(), principal, session, 3),
                ConnectionId(Id::new())
            )
            .is_err());
        assert_eq!(directory.states(&[a]).unwrap()[0].unwrap().binding, b);
        directory.close(b).unwrap();
        assert!(directory
            .renew(b, SystemTime::now() + Duration::from_secs(180))
            .is_err());
        assert!(directory
            .admit(grant(a, principal, session, 2), ConnectionId(Id::new()))
            .is_err());
    }
    std::fs::remove_file(path).unwrap();
}
#[test]
fn transient_world_runs_without_storage_and_rechecks_expiry_and_queued_samples() {
    let mut app = App::new();
    app.add_plugins((MeshPlugin::transient(), MeshSessionPlugin::default()));
    assert!(app.world().get_resource::<Store>().is_none());
    let actor = Id::new();
    let e = app.world_mut().spawn(Managed::new(actor, actor)).id();
    let d = app.world().resource::<SessionDirectory>().clone();
    let p = PrincipalId(Id::new());
    let s = SessionId(Id::new());
    let ingress = app.world().resource::<SessionIngress>().clone();
    let old = d
        .admit(grant(actor, p, s, 1), ConnectionId(Id::new()))
        .unwrap();
    let input = |binding, sequence| SessionInput {
        binding,
        sequence,
        at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        axes: [1, 0, 0],
    };
    ingress.submit(input(old, 1)).unwrap();
    app.update();
    assert_eq!(app.world().get::<ValidatedInput>(e).unwrap().sequence, 1);
    ingress.submit(input(old, 2)).unwrap();
    d.suspend(old).unwrap();
    app.update();
    assert!(app.world().get::<MeshReady>(e).is_none());
    assert!(app.world().get::<ValidatedInput>(e).is_none());
    assert!(app.world().get::<Simulated>(e).is_some());
    let new = d
        .admit(grant(actor, p, s, 2), ConnectionId(Id::new()))
        .unwrap();
    ingress.submit(input(new, 4)).unwrap();
    app.update();
    ingress.submit(input(old, 8)).unwrap();
    app.update();
    assert_eq!(app.world().get::<ValidatedInput>(e).unwrap().sequence, 4);
    let other = Id::new();
    let wrong = app.world_mut().spawn(Managed::new(other, other)).id();
    ingress
        .submit(input(
            ControlBinding {
                object: other,
                ..new
            },
            9,
        ))
        .unwrap();
    app.update();
    assert!(app.world().get::<MeshReady>(wrong).is_none());
    let mut expired = grant(actor, p, s, 3);
    expired.expires_at = UNIX_EPOCH;
    assert!(d.admit(expired, ConnectionId(Id::new())).is_err());
    let mut short = grant(actor, p, s, 3);
    short.expires_at = SystemTime::now() + Duration::from_millis(40);
    let expiring = d.admit(short, ConnectionId(Id::new())).unwrap();
    ingress.attach(expiring).unwrap();
    std::thread::sleep(Duration::from_millis(45));
    app.update();
    assert!(app.world().get::<MeshReady>(e).is_none());
    assert!(checkpoint_group(app.world_mut(), actor).is_err());
}

#[derive(Serialize, Deserialize)]
struct ProbeWriter(std::path::PathBuf);
impl Action for ProbeWriter {
    const ID: u32 = 42;
    const VERSION: u16 = 1;
    type Output = u32;
}
#[test]
fn compatibility_handler_can_open_an_independent_sql_writer_before_atomic_commit() {
    let path = std::env::temp_dir().join(format!("zz-prepare-{}.sqlite", Id::new()));
    let db = Store::open(&path).unwrap();
    let w = db.start_worker(1).unwrap();
    let id = Id::new();
    let owner = db.create_group(id, w).unwrap();
    let session = Id::new();
    db.session(session, id).unwrap();
    let mut record = Record::new(id, id);
    record.put(1, 1, &0_u32).unwrap();
    db.checkpoint(id, owner, 0, vec![record]).unwrap();
    let mut actions = Actions::default();
    actions
        .register::<ProbeWriter>(|edit, p| {
            let other = rusqlite::Connection::open(p.0)?;
            other.busy_timeout(Duration::ZERO)?;
            other.execute_batch("BEGIN IMMEDIATE; ROLLBACK")?;
            let mut record = edit.read(edit.actor)?;
            let next = record.get::<u32>(1, 1)? + 1;
            record.put(1, 1, &next)?;
            edit.save(record)?;
            Ok(next)
        })
        .unwrap();
    let intents: Vec<_> = (1..=3)
        .map(|seq| {
            Intent::typed(ActionKey { session, seq }, id, &ProbeWriter(path.clone())).unwrap()
        })
        .collect();
    let first = db.execute(&actions, id, owner, 0, &intents).unwrap();
    assert!(first.iter().all(|r| matches!(
        r,
        Reply::Decided(Receipt {
            outcome: Outcome::Committed(_),
            ..
        })
    )));
    assert_eq!(db.execute(&actions, id, owner, 0, &intents).unwrap(), first);
    assert_eq!(
        db.snapshot(id).unwrap().records[0]
            .get::<u32>(1, 1)
            .unwrap(),
        3
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
}
