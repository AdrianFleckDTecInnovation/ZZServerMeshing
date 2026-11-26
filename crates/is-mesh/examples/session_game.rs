//! Headless external game and trusted transport adapter using public interfaces.
use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use is_mesh::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Component, Serialize, Deserialize)]
struct Position(i64);
const POSITION: u32 = 1;
const BALANCE: u32 = 2;

#[derive(Component, Serialize, Deserialize)]
struct Figure {
    ship: Id,
}
#[derive(Component, Serialize, Deserialize)]
struct Ship {
    pilot: Id,
    chunk: Id,
    #[serde(skip)]
    local: Option<[Entity; 2]>,
}
#[derive(Component, Serialize, Deserialize)]
struct Chunk {
    ship: Id,
    dataset: DataKey,
    revision: u64,
}
#[derive(Component, Serialize, Deserialize)]
struct Asteroid {
    surface: DataKey,
}
const SHIP_ID: Id = Id([31; 16]);
const CHUNK_ID: Id = Id([32; 16]);
const ROCK_ID: Id = Id([33; 16]);
fn steer(
    mut actors: Query<
        (&ValidatedInput, &mut Position),
        (With<Figure>, With<Simulated>, With<MeshReady>),
    >,
) {
    for (input, mut p) in &mut actors {
        p.0 += i64::from(input.axes[0]);
    }
}
fn verify_assembly(world: &World, actor: Id) -> Result<()> {
    let ids = &world.resource::<MeshState>().entities;
    let ship = world
        .get::<Ship>(ids[&SHIP_ID])
        .ok_or_else(|| invalid("ship missing"))?;
    let chunk = world
        .get::<Chunk>(ids[&CHUNK_ID])
        .ok_or_else(|| invalid("chunk missing"))?;
    assert_eq!(ship.local, Some([ids[&actor], ids[&CHUNK_ID]]));
    assert_eq!(world.get::<Figure>(ids[&actor]).unwrap().ship, SHIP_ID);
    assert_eq!(
        (chunk.ship, chunk.dataset.object, chunk.revision),
        (SHIP_ID, ROCK_ID, 7)
    );
    assert_eq!(
        world.get::<Asteroid>(ids[&ROCK_ID]).unwrap().surface,
        chunk.dataset
    );
    Ok(())
}
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
    let public = Rules {
        visibility: Visibility::Public,
        migrate: true,
        persist: true,
    };
    app.mesh_component::<Figure>(
        3,
        1,
        public,
        ComponentCodec {
            references: |v| vec![Reference::RequiredLocal(v.ship)],
            ..Default::default()
        },
    )?
    .mesh_component::<Ship>(
        4,
        1,
        public,
        ComponentCodec {
            references: |v| {
                vec![
                    Reference::RequiredLocal(v.pilot),
                    Reference::RequiredLocal(v.chunk),
                ]
            },
            resolve: |v, local| {
                v.local = Some([local[&v.pilot], local[&v.chunk]]);
                Ok(())
            },
            ..Default::default()
        },
    )?
    .mesh_component::<Chunk>(
        5,
        1,
        public,
        ComponentCodec {
            references: |v| {
                vec![
                    Reference::RequiredLocal(v.ship),
                    Reference::Remote(v.dataset.object),
                ]
            },
            ..Default::default()
        },
    )?
    .mesh_component::<Asteroid>(6, 1, public, Default::default())?;
    app.add_systems(Update, steer);
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
        .spawn((
            Managed::new(actor, actor),
            Position(0),
            Figure { ship: SHIP_ID },
        ))
        .id();
    let dataset = DataKey {
        provider: 42,
        object: ROCK_ID,
        part: 3,
    };
    first.world_mut().spawn((
        Managed::new(SHIP_ID, actor),
        Ship {
            pilot: actor,
            chunk: CHUNK_ID,
            local: None,
        },
        Position(50_000),
    ));
    first.world_mut().spawn((
        Managed::new(CHUNK_ID, actor),
        Chunk {
            ship: SHIP_ID,
            dataset,
            revision: 7,
        },
        Position(150_000),
    ));
    first.world_mut().spawn((
        Managed::new(ROCK_ID, actor),
        Asteroid { surface: dataset },
        Position(180_000),
    ));
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
    assert_eq!(seen, vec![actor, neighbor, SHIP_ID]);
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
    directory.renew(resumed, SystemTime::now() + Duration::from_secs(240))?;
    // An authorized request is in flight while the mesh changes authority.
    let (out, work) = ExternalActions::bounded(std::num::NonZeroUsize::MIN);
    let correlation = Id::new();
    out.submit(pool.app(a)?.world(), resumed, correlation, ())?;
    let (release, cutover) = std::sync::mpsc::sync_channel(1);
    let result = std::thread::spawn(move || {
        let request = work.recv().unwrap();
        cutover.recv().unwrap();
        (request.actor, request.correlation, 6, 91_u64)
    });
    pool.transfer(actor, b, 1024 * 1024)?;
    release.send(()).unwrap();
    let (object, key, revision, value) = result.join().unwrap();
    assert_eq!(key, correlation);
    let owner = store.authorities(&[object])?[&object].1;
    assert_eq!(owner.worker, b);
    assert!(matches!(
        apply_external(pool.app_mut(a)?.world_mut(), object, revision, value),
        Err(Error::Fenced)
    ));
    let destination = pool.app_mut(owner.worker)?.world_mut();
    verify_assembly(destination, actor)?;
    assert!(matches!(
        apply_external(destination, object, revision, value),
        Err(Error::Missing(_))
    ));
    hydrate_external(destination, object, 5, 95_u64)?;
    assert!(apply_external(destination, object, revision, value)?);
    assert!(!apply_external(destination, object, revision, value)?);
    assert!(!apply_external(destination, object, 4, 100_u64)?);
    directory.revoke(resumed)?;
    directory.close(resumed)?;
    assert!(matches!(
        stream.next(&pool, &mut encoder),
        Err(Error::Denied)
    ));
    let replacement = store.start_worker(b.slot)?;
    let mut restarted = worker(&store, replacement)?;
    restore_group(restarted.world_mut(), actor)?;
    verify_assembly(restarted.world(), actor)?;
    let entity = restarted.world().resource::<MeshState>().entities[&actor];
    assert!(restarted
        .world()
        .get::<ExternalValue<u64>>(entity)
        .is_none());
    hydrate_external(restarted.world_mut(), actor, 6, 91_u64)?;
    restarted
        .world()
        .resource::<SessionIngress>()
        .attach(resumed)?;
    restarted.update();
    assert!(restarted.world().get::<MeshReady>(entity).is_none());
    assert_eq!(
        restarted
            .world()
            .get::<ExternalValue<u64>>(entity)
            .unwrap()
            .value,
        91
    );
    println!("public game API: figure, ship, asteroid, chunk, authority transfer, external revision, restore and subscriber resume passed");
    Ok(())
}
