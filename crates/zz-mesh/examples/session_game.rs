//! Headless external game and trusted transport adapter using public interfaces.
use bevy_app::App;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zz_mesh::*;

#[derive(Component, Serialize, Deserialize)]
struct Position(i64);
const POSITION: u32 = 1;
const BALANCE: u32 = 2;

fn worker(db: &Store, w: Worker) -> Result<App> {
    let mut app = App::new();
    app.add_plugins((
        MeshPlugin::persistent(db.clone(), w),
        MeshSessionPlugin::default(),
    ));
    app.mesh_component::<Position>(
        POSITION,
        1,
        Rules {
            visibility: Visibility::Public,
            persist: true,
            migrate: true,
        },
        Default::default(),
    )?
    .mesh_component::<ExternalValue<u64>>(
        BALANCE,
        1,
        Rules::external(Visibility::Private),
        Default::default(),
    )?;
    app.init_resource::<SubscriberSource>()
        .insert_resource(SpatialDependencies([POSITION].into_iter().collect()))
        .insert_resource(SightRanges::millimetres(1, [(1, 100_000)]))
        .insert_resource(SightRule(|world, e| {
            Some((
                Id::numbered(0),
                SightPoint {
                    kind: 1,
                    mm: [world.get::<Position>(e)?.0, 0, 0],
                },
            ))
        }))
        .insert_resource(SpatialRule(|world, e| {
            let point = [world.get::<Position>(e)?.0, 0, 0];
            Some(SpatialProfile {
                bounds: Box3::new(Id::numbered(0), point, point).ok()?,
                velocity: [0; 3],
                ahead: 0,
                retention: 0,
                principal: world.get::<Managed>(e)?.id,
                active: true,
                needs: Default::default(),
            })
        }));
    Ok(app)
}
fn grant(principal: PrincipalId, session: SessionId, actor: Id, epoch: u64) -> SessionGrant {
    SessionGrant {
        principal,
        session,
        object: actor,
        control_epoch: epoch,
        expires_at: SystemTime::now() + Duration::from_secs(120),
    }
}
fn main() -> Result<()> {
    let store = Store::open(":memory:")?;
    let a = store.start_worker(1)?;
    let b = store.start_worker(2)?;
    let actor = Id::numbered(10);
    let neighbor = Id::numbered(20);
    let mut first = worker(&store, a)?;
    let mut second = worker(&store, b)?;
    let entity = first
        .world_mut()
        .spawn((Managed::new(actor, actor), Position(0)))
        .id();
    let other = second
        .world_mut()
        .spawn((Managed::new(neighbor, neighbor), Position(10_000)))
        .id();
    let directory = second.world().resource::<SessionDirectory>().clone();
    let mut pool = Pool::new(store.clone(), 2048);
    pool.add(a, first)?;
    pool.add(b, second)?;
    pool.step()?;

    let principal = PrincipalId(Id::new());
    let session = SessionId(Id::new());
    let subscriber = SubscriberId(Id::new());
    let binding = directory.admit(grant(principal, session, actor, 1), ConnectionId(Id::new()))?;
    directory.subscribe(binding, Some(subscriber))?;
    let target = store.authorities(&[actor])?[&actor].1.worker;
    let ingress = pool
        .app(target)?
        .world()
        .resource::<SessionIngress>()
        .clone();
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    ingress.submit(SessionInput {
        binding,
        sequence: 1,
        at_ms,
        axes: [1, 0, 0],
    })?;
    pool.step()?;
    assert_eq!(
        pool.app(a)?
            .world()
            .get::<ValidatedInput>(entity)
            .unwrap()
            .axes,
        [1, 0, 0]
    );

    // The external service owns the balance and its revision.
    hydrate_external(pool.app_mut(a)?.world_mut(), actor, 4, 100_u64)?;
    hydrate_external(pool.app_mut(b)?.world_mut(), neighbor, 1, 55_u64)?;
    let (port, service) = ExternalActions::bounded(8.try_into().unwrap());
    let key = Id::new();
    port.submit(pool.app(a)?.world(), binding, key, 5_u64)?;
    let result = std::thread::spawn(move || {
        let r = service.recv().unwrap();
        (r.actor, r.correlation, 5, 100 - r.payload)
    })
    .join()
    .unwrap();
    assert_eq!(result.1, key);
    assert!(apply_external(
        pool.app_mut(a)?.world_mut(),
        result.0,
        result.2,
        result.3
    )?);
    assert!(!apply_external(
        pool.app_mut(a)?.world_mut(),
        actor,
        4,
        101_u64
    )?);
    pool.step()?;
    let mut encoder = SubscriberEncoder::default();
    let mut stream = SubscriberStream::open(&directory, binding, subscriber, Default::default())?;
    let initial = stream.next(&pool, &mut encoder)?.unwrap();
    let mut seen = Vec::new();
    for event in initial.events {
        if let SubscriberEvent::Enter { object, fields, .. } = event {
            assert_eq!(fields.iter().any(|(s, _)| *s == BALANCE), object == actor);
            seen.push(object);
        }
    }
    assert_eq!(seen, vec![actor, neighbor]);
    stream.applied(binding, initial.sequence)?;
    directory.suspend(binding)?;
    pool.step()?;
    assert!(pool.app(a)?.world().get::<ValidatedInput>(entity).is_none());
    let resumed = directory.admit(grant(principal, session, actor, 2), ConnectionId(Id::new()))?;
    directory.subscribe(resumed, Some(subscriber))?;
    assert!(directory.close(binding).is_err());
    stream.resume(&directory, resumed, subscriber, initial.sequence)?;
    pool.app_mut(b)?
        .world_mut()
        .get_mut::<Position>(other)
        .unwrap()
        .0 = 200_000;
    pool.step()?;
    let update = stream.next(&pool, &mut encoder)?.unwrap();
    assert!(update
        .events
        .iter()
        .any(|e| matches!(e,SubscriberEvent::Leave(id) if *id==neighbor)));
    stream.applied(resumed, update.sequence)?;
    directory.close(resumed)?;
    assert!(matches!(
        stream.next(&pool, &mut encoder),
        Err(Error::Denied)
    ));
    println!("external session, input, private subscriber, resume and async result passed");
    Ok(())
}
