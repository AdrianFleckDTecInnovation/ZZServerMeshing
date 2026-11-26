//! Measure on-demand, exact player views outside the simulation tick.
use infinite_stars::{world, Inventory, Player, Position, Ship, INVENTORY, PLANET_ID};
use serde_json::{json, Value};
use std::time::Instant;
use zz_mesh::{
    wire::{Request, Response},
    *,
};

fn group(site: usize, slot: usize) -> Id {
    Id::numbered(1_000_000 + (site * 2 + slot) as u128)
}
fn ship(site: usize) -> Id {
    Id::numbered(2_000_000 + site as u128)
}
fn verify<'a>(
    i: usize,
    group_size: usize,
    records: impl Iterator<Item = &'a Record>,
) -> Result<()> {
    let site = i / group_size;
    let peers = (site * group_size..(site + 1) * group_size)
        .filter(|&n| n != i)
        .map(|n| Id::numbered(n as u128));
    let mut expected = peers.chain((group_size > 1).then(|| ship(site)));
    for r in records {
        if expected.next() != Some(r.id) || r.fields.contains_key(&INVENTORY) {
            return Err(invalid("incorrect public player view"));
        }
    }
    if expected.next().is_some() {
        return Err(invalid("missing visible neighbour"));
    }
    Ok(())
}
fn scene(players: usize, group_size: usize, push: bool) -> Result<Value> {
    let db = Store::open(":memory:")?;
    let mut pool = Pool::new(db.clone(), players * 4);
    pool.policy.every = 0;
    let workers = [db.start_worker(1)?, db.start_worker(2)?];
    for w in workers {
        let mut app = world(&db, w);
        if push {
            app.init_resource::<SubscriberSource>()
                .add_plugins(MeshSessionPlugin {
                    capacity: players.try_into().unwrap(),
                });
        }
        pool.add(w, app)?;
    }
    let viewers: Vec<_> = (0..players).map(|i| Id::numbered(i as u128)).collect();
    for (i, &viewer) in viewers.iter().enumerate() {
        let site = i / group_size;
        let x = site as i64 * 2_000_000;
        let slot = i % 2;
        let position = Position {
            frame: PLANET_ID,
            mm: [x, (i % group_size) as i64 * 1000, 0],
        };
        pool.app_mut(workers[slot])?.world_mut().spawn((
            Managed::new(viewer, group(site, slot)),
            Player,
            position,
            Inventory::default(),
        ));
        if group_size > 1 && i % group_size == 0 {
            pool.app_mut(workers[1])?.world_mut().spawn((
                Managed::new(ship(site), group(site, 1)),
                Ship,
                Position {
                    frame: PLANET_ID,
                    mm: [x + 400_000, 0, 0],
                },
            ));
        }
    }
    pool.step()?;
    pool.deliver_all()?;
    if push {
        return stream_run(&mut pool, workers, &viewers, group_size);
    }
    let mut checked = 0;
    let mut correct = Ok(());
    pool.visit_player_sights(&viewers, |actor, rows| {
        if correct.is_ok() {
            correct = if viewers.get(checked) == Some(&actor) {
                verify(checked, group_size, rows.iter().copied())
            } else {
                Err(invalid("unexpected observer"))
            };
        }
        checked += 1;
    })?;
    correct?;
    if checked != players {
        return Err(invalid("missing observer"));
    }
    let mut runs = Vec::new();
    for _ in 0..3 {
        let mut queries = Vec::with_capacity(players);
        let mut count = 0;
        for (i, &viewer) in viewers.iter().enumerate() {
            let start = Instant::now();
            let view = pool.player_sight(viewer)?;
            queries.push(start.elapsed().as_nanos() as u64);
            verify(i, group_size, view.values())?;
            count += view.len();
        }
        queries.sort_unstable();
        let mut batch_count = 0;
        let start = Instant::now();
        pool.visit_player_sights(&viewers, |_, rows| {
            batch_count += std::hint::black_box(rows).len();
        })?;
        let batch = start.elapsed();
        if batch_count != count {
            return Err(invalid("incomplete batched views"));
        }
        runs.push(json!({
            "views": players, "public_records": count, "read_ms": queries.iter().sum::<u64>() as f64 / 1e6,
            "per_view_p95_us": queries[(95 * players).div_ceil(100) - 1] as f64 / 1e3,
            "batched_ms": batch.as_secs_f64() * 1000.
        }));
    }
    // Take one real source's records. Count both protocol directions, excluding framing and TLS.
    let view = pool.player_sight(viewers[0])?;
    let records = view
        .into_values()
        .filter(|r| {
            pool.directory
                .presence(r.id)
                .is_some_and(|p| p.owner == workers[1])
        })
        .collect();
    let page = SightPage {
        records,
        next: None,
    };
    let source_records = page.records.len();
    let conditional = encode(&Request::SightPage {
        after: None,
        known: Some(page.content_digest()),
    })?
    .len()
        + encode(&Response::SightUnchanged { next: None })?.len();
    let full = encode(&Request::SightPage {
        after: None,
        known: None,
    })?
    .len()
        + encode(&Response::SightPage(page))?.len();
    Ok(json!({
        "players": players, "players_per_island": group_size, "ships": if group_size > 1 { players/group_size } else { 0 },
        "runs": runs, "wire_sample": (source_records > 0).then(|| json!({ "one_source_records": source_records, "full_bytes": full, "unchanged_bytes": conditional }))
    }))
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let players: usize = args
        .next()
        .as_deref()
        .unwrap_or("10000")
        .parse()
        .map_err(|_| invalid("invalid player count"))?;
    if !(16..=65536).contains(&players) || players % 16 != 0 {
        return Err(invalid("use a multiple of 16 players in 16..65536"));
    }
    let out = args.next();
    let push = match args.next().as_deref() {
        None => false,
        Some("--push") => true,
        _ => {
            return Err(invalid(
                "usage: sight-bench [PLAYERS] [OUTPUT.json] [--push]",
            ))
        }
    };
    if args.next().is_some() {
        return Err(invalid("unexpected sight benchmark arguments"));
    }
    let report = json!({ "os": std::env::consts::OS, "push":push, "profiles": [scene(players, 1,push)?, scene(players, 16,push)?] });
    let json = serde_json::to_string_pretty(&report).map_err(|e| invalid(e.to_string()))?;
    if let Some(file) = out {
        std::fs::write(file, &json)?;
    }
    println!("{json}");
    Ok(())
}
fn stream_run(
    pool: &mut Pool,
    workers: [Worker; 2],
    actors: &[Id],
    group_size: usize,
) -> Result<Value> {
    use std::time::{Duration, SystemTime};
    let directory = SessionDirectory::persistent(pool.store.clone());
    let expires = SystemTime::now() + Duration::from_secs(600);
    let before = process_usage().1;
    let mut streams = Vec::with_capacity(actors.len());
    for &object in actors {
        let id = SubscriberId(Id::new());
        let binding = directory.admit(
            SessionGrant {
                principal: PrincipalId(object),
                session: SessionId(Id::new()),
                object,
                control_epoch: 1,
                expires_at: expires,
            },
            ConnectionId(Id::new()),
        )?;
        directory.subscribe(binding, Some(id))?;
        streams.push((
            binding,
            SubscriberStream::open(&directory, binding, id, Default::default())?,
        ));
    }
    let ingress = [
        pool.app(workers[0])?
            .world()
            .resource::<SessionIngress>()
            .clone(),
        pool.app(workers[1])?
            .world()
            .resource::<SessionIngress>()
            .clone(),
    ];
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let input_start = Instant::now();
    runtime.block_on(async {
        let mut tasks = tokio::task::JoinSet::new();
        for (n, (binding, _)) in streams.iter().enumerate() {
            let inbox = ingress[n % 2].clone();
            let binding = *binding;
            tasks.spawn(async move {
                let at_ms = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| invalid("clock"))?
                    .as_millis() as u64;
                inbox.submit(SessionInput {
                    binding,
                    sequence: 1,
                    axes: [1, 0, 0],
                    at_ms,
                })
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.map_err(|e| invalid(e.to_string()))??;
        }
        Ok::<(), Error>(())
    })?;
    pool.step()?;
    let input_ms = input_start.elapsed().as_secs_f64() * 1000.;
    for (n, (binding, _)) in streams.iter().enumerate() {
        let w = pool.app(workers[n % 2])?.world();
        let entity = w.resource::<MeshState>().entities[&binding.object];
        if w.get::<ValidatedInput>(entity)
            .is_none_or(|i| i.sequence != 1)
        {
            return Err(invalid("async adapter input not applied"));
        }
    }
    let mut encoder = SubscriberEncoder::default();
    let start = Instant::now();
    let mut enters = 0usize;
    let mut bytes = 0usize;
    for (binding, s) in &mut streams {
        let frame = s
            .next(pool, &mut encoder)?
            .ok_or_else(|| invalid("missing initial snapshot"))?;
        let mut members = std::collections::BTreeSet::new();
        for event in frame.events {
            bytes += event.budget_bytes();
            if let SubscriberEvent::Enter { object, fields, .. } = event {
                if fields.iter().any(|(id, _)| *id == INVENTORY) != (binding.object == object) {
                    return Err(invalid("subscriber private field boundary"));
                }
                members.insert(object);
            }
        }
        if members.len() != group_size + usize::from(group_size > 1)
            || !members.contains(&binding.object)
        {
            return Err(invalid("incomplete subscriber visibility"));
        }
        enters += members.len();
        s.applied(*binding, frame.sequence)?;
    }
    let initial_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut idle = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        for (_, s) in &mut streams {
            if s.next(pool, &mut encoder)?.is_some() {
                return Err(invalid("unchanged subscriber payload"));
            }
        }
        idle.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let rss = process_usage().1;
    Ok(
        json!({"subscribers":actors.len(),"players_per_island":group_size,"enter_records":enters,"initial_ms":initial_ms,"initial_budgeted_bytes":bytes,
        "idle_ms":idle,"resident_bytes":rss,"rss_growth_per_subscriber":rss.saturating_sub(before)/actors.len() as u64,"confirmed_inputs":streams.len(),"input_and_tick_ms":input_ms,"adapter_threads":1,"transport":"Tokio current-thread logical adapter; two Bevy workers"}),
    )
}
