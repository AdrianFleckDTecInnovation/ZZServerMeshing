//! Receive the browser wire protocol over loopback sockets on a launcher-owned mesh.
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use shared::{
    entity::EntityId,
    protocol::{ClientMessage, ServerMessage},
};
use sim_server::encoding::{DELTA_HEADER_SIZE, ENTITY_SIZE, HEADER_SIZE};
use std::{collections::BTreeMap, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch, Semaphore},
    task::JoinSet,
    time::{sleep_until, timeout, Instant},
};
use tokio_tungstenite::{client_async_with_config, tungstenite::Message};

type Failure = Box<dyn std::error::Error + Send + Sync>;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    first_port: u16,
    #[arg(long)]
    servers: u16,
    #[arg(long)]
    players: usize,
    #[arg(long)]
    seconds: u32,
    #[arg(long, default_value_t = 5)]
    input_hz: u32,
    #[arg(long)]
    ready: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    start_file: PathBuf,
    #[arg(long, default_value_t = 0)]
    driver: usize,
    #[arg(long, default_value_t = 1)]
    drivers: usize,
}
#[derive(Default)]
struct Received {
    frames: usize,
    bytes: usize,
    inputs: u32,
    latencies: Vec<u32>,
    peer: bool,
    own_moved: bool,
    missing_ticks: u64,
}
struct Snapshot<'a> {
    tick: u64,
    records: &'a [u8],
}
fn parse(data: &[u8]) -> Result<Snapshot<'_>, Failure> {
    let header = match data.first() {
        Some(1) => HEADER_SIZE,
        Some(2) => DELTA_HEADER_SIZE,
        _ => return Err("snapshot type".into()),
    };
    if data.len() < header {
        return Err("short header".into());
    }
    let u32_at = |i| u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
    let records = u32_at(9) * ENTITY_SIZE;
    let removed = if data[0] == 2 { u32_at(13) * 16 } else { 0 };
    if (header + records + removed) != data.len() {
        return Err("snapshot length".into());
    }
    Ok(Snapshot {
        tick: u64::from_le_bytes(data[1..9].try_into().unwrap()),
        records: &data[header..header + records],
    })
}
#[derive(Clone, Copy)]
struct Window {
    start: Instant,
    end: Instant,
    input_period_us: u64,
    input_count: u32,
}

// Leave one scheduler quantum before the end of the observation window.
fn input_offset(index: usize, w: Window) -> Duration {
    Duration::from_micros((index as u64 * 9973) % (w.input_period_us - 20_000))
}
fn percentile<T: Copy>(sorted: &[T], n: usize) -> Option<T> {
    sorted
        .get((sorted.len() * n).div_ceil(100).saturating_sub(1))
        .copied()
}
#[tokio::main]
async fn main() -> Result<(), Failure> {
    let a = Args::parse();
    if a.servers == 0
        || !(1..=20000).contains(&a.players)
        || !(2..=60).contains(&a.seconds)
        || !(1..=10).contains(&a.input_hz)
        || a.drivers == 0
        || a.driver >= a.drivers
        || a.drivers > a.players
        || a.first_port.checked_add(a.servers - 1).is_none()
    {
        return Err("invalid loopback workload".into());
    }
    let (ready, mut incoming) = mpsc::channel(a.players);
    let (start, window) = watch::channel(None);
    let permits = Arc::new(Semaphore::new(256));
    let mut tasks = JoinSet::new();
    let mut local_count = 0;
    for i in (a.driver..a.players).step_by(a.drivers) {
        local_count += 1;
        let lane = (i % usize::from(a.servers)) as u16;
        tasks.spawn(player(
            i,
            lane + 1,
            a.first_port + lane,
            permits.clone(),
            ready.clone(),
            window.clone(),
        ));
    }
    drop(ready);
    timeout(Duration::from_secs(100),async {
        for _ in 0..local_count {
            tokio::select! {
                v=incoming.recv()=>v.ok_or("join lost")?,
                done=tasks.join_next()=>{done.ok_or("no players")???;return Err::<(),Failure>("player exited during join".into());}
            }
        }
        Ok(())
    }).await??;
    std::fs::write(&a.ready, b"ready")?;
    let start_ms = timeout(Duration::from_secs(115), async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&a.start_file) {
                if let Ok(t) = s.parse::<u64>() {
                    break t;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let wall = std::time::UNIX_EPOCH + Duration::from_millis(start_ms);
    let clock = Instant::now() + wall.duration_since(std::time::SystemTime::now())?;
    let duration = Duration::from_secs(u64::from(a.seconds));
    let cpu = std::thread::spawn(move || {
        std::thread::sleep(clock.saturating_duration_since(Instant::now()));
        let wall = Instant::now();
        let before = sim_server::cpu::process_ns();
        std::thread::sleep(duration);
        sim_server::cpu::process_ns().saturating_sub(before) as f64
            / wall.elapsed().as_nanos() as f64
    });
    start.send(Some(Window {
        start: clock,
        end: clock + duration,
        input_period_us: 1_000_000 / u64::from(a.input_hz),
        input_count: a.seconds * a.input_hz,
    }))?;
    let mut frames = Vec::with_capacity(a.players);
    let mut latencies = Vec::new();
    let (mut sent, mut peers, mut moved, mut bytes, mut missing) =
        (0usize, 0usize, 0usize, 0usize, 0u64);
    while let Some(done) = tasks.join_next().await {
        let stats = done??;
        sent += stats.inputs as usize;
        peers += usize::from(stats.peer);
        moved += usize::from(stats.own_moved);
        bytes += stats.bytes;
        missing += stats.missing_ticks;
        frames.push(stats.frames as f64 / f64::from(a.seconds));
        latencies.extend(stats.latencies);
    }
    frames.sort_by(f64::total_cmp);
    latencies.sort_unstable();
    let expected = local_count * a.seconds as usize * a.input_hz as usize;
    let result = json!({"players":local_count,"sim_servers":a.servers,"measurement_seconds":a.seconds,
        "transport":"browser WebSocket, native loopback clients","target_os":std::env::consts::OS,
        "expected_inputs":expected,"sent_inputs":sent,"confirmed_inputs":latencies.len(),
        "input_p50_us":percentile(&latencies,50),"input_p95_us":percentile(&latencies,95),
        "input_p99_us":percentile(&latencies,99),"receive_hz_min":frames.first(),
        "receive_hz_p05":percentile(&frames,5),"receive_hz_p50":percentile(&frames,50),
        "players_with_peer":peers,"players_with_confirmed_motion":moved,"bytes_received":bytes,
        "skipped_snapshot_ticks":missing,"frame_rates":frames,"input_us":latencies,
        "driver_cpu_cores":cpu.join().map_err(|_|"CPU sampler stopped")?});
    std::fs::write(a.output, serde_json::to_string_pretty(&result)?)?;
    Ok(())
}

async fn player(
    index: usize,
    server: u16,
    port: u16,
    permits: Arc<Semaphore>,
    ready: mpsc::Sender<()>,
    mut window: watch::Receiver<Option<Window>>,
) -> Result<Received, Failure> {
    let mut permit = Some(permits.acquire_owned().await?);
    let stream = timeout(
        Duration::from_secs(8),
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await??;
    stream.set_nodelay(true)?;
    let (mut ws, _) = client_async_with_config(
        format!("ws://127.0.0.1:{port}"),
        stream,
        Some(sim_server::network::socket_config()),
    )
    .await?;
    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::RequestSnapshot)?.into(),
    ))
    .await?;
    let mut id = None::<EntityId>;
    let mut home = None::<[f32; 2]>;
    let mut active = None::<Window>;
    let mut next_send = Instant::now() + Duration::from_secs(120);
    let mut stop = next_send;
    let mut previous_tick = None;
    let mut last_ack = None;
    let mut pending = BTreeMap::new();
    let mut stats = Received::default();
    loop {
        tokio::select! {
            event = ws.next() => match event.ok_or("connection closed")?? {
                Message::Text(s) => match serde_json::from_str::<ServerMessage>(&s)? {
                    ServerMessage::PlayerAssigned { player_id, protocol_version }
                        if protocol_version == shared::PROTOCOL_VERSION => {
                            id = Some(EntityId(uuid::Uuid::parse_str(&player_id)?));
                        }
                    ServerMessage::Reconnect { .. } => return Err("player reassigned in fixed placement run".into()),
                    _ => return Err("unsupported assignment version".into()),
                },
                Message::Binary(bytes) => {
                    let received = Instant::now();
                    let frame = parse(&bytes)?;
                    let timed = active.is_some_and(|w| received >= w.start && received < w.end);
                    if let Some(prev) = previous_tick {
                        if frame.tick <= prev {
                            return Err("non-increasing snapshot tick".into());
                        }
                        if timed {
                            stats.missing_ticks += frame.tick - prev - 1;
                        }
                    }
                    previous_tick = Some(frame.tick);
                    let mut peer = false;
                    for record in frame.records.chunks_exact(ENTITY_SIZE) {
                        let f32_at = |n| f32::from_le_bytes(record[n..n + 4].try_into().unwrap());
                        let position = [f32_at(16), f32_at(24)];
                        if !position.iter().all(|x| x.is_finite()) {
                            return Err("nonfinite position".into());
                        }
                        if record[40] != 1 {
                            continue;
                        }
                        if let Some(h) = home {
                            if (h[0] - position[0]).abs() > 250. || (h[1] - position[1]).abs() > 250. {
                                return Err("snapshot contains player from another island".into());
                            }
                        }
                        peer |= u16::from_le_bytes(record[41..43].try_into().unwrap()) != server;
                        if id.is_some_and(|i| i.0.as_bytes() == &record[..16]) {
                            if let Some(h) = home {
                                stats.own_moved |= timed && (h[0] - position[0]).abs() > 0.01;
                            } else {
                                home = Some(position);
                                ready.send(()).await?;
                                permit.take();
                            }
                            let seq = u32::from_le_bytes(record[43..47].try_into().unwrap());
                            if let Some(when) = pending.remove(&seq) {
                                let latency = received.duration_since(when).as_micros();
                                stats.latencies.push(latency.min(u32::MAX as u128) as u32);
                            }
                        }
                    }
                    if last_ack.is_none_or(|tick| frame.tick.saturating_sub(tick) >= 4) {
                        let mut ack = bytes[..9].to_vec();
                        ack[0] = 3;
                        ws.send(Message::Binary(ack.into())).await?;
                        last_ack = Some(frame.tick);
                    }
                    if timed {
                        stats.frames += 1;
                        stats.bytes += bytes.len();
                        stats.peer |= peer;
                    }
                }
                Message::Close(_) => return Err("client closed".into()),
                _ => {}
            },
            changed = window.changed(), if active.is_none() => {
                changed?;
                active = *window.borrow_and_update();
                if let Some(w) = active {
                    next_send = w.start + input_offset(index, w);
                    stop = w.end + Duration::from_secs(2);
                }
            },
            _ = sleep_until(next_send), if active.is_some() => {
                let w = active.unwrap();
                let first = w.start + input_offset(index, w);
                let now = Instant::now();
                next_send = stop + Duration::from_secs(1);
                if now < w.end {
                    let elapsed_us = now.saturating_duration_since(first).as_micros() as u64;
                    let seq = 1 + (elapsed_us / w.input_period_us) as u32;
                    if seq <= w.input_count {
                        let input = ClientMessage::MoveInput {
                            entity_id: id.ok_or("missing assigned player")?,
                            direction: [if seq % 2 == 0 { -1. } else { 1. }, 0., 0.],
                            seq,
                        };
                        pending.insert(seq, Instant::now());
                        ws.send(Message::Text(serde_json::to_string(&input)?.into())).await?;
                        stats.inputs += 1;
                        let next = first + Duration::from_micros(w.input_period_us * u64::from(seq));
                        if next < w.end {
                            next_send = next;
                        }
                    }
                }
            },
            _ = sleep_until(stop) => return Ok(stats),
        }
    }
}
