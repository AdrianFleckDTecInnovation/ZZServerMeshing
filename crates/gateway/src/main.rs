mod aggregator;

use aggregator::Aggregator;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use shared::protocol::{CoordClientMsg, CoordServerMsg, ServerMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(name = "gateway", about = "Client gateway — aggregates sim-server state")]
struct Args {
    /// WebSocket port for browser clients
    #[arg(long, default_value_t = 4322)]
    port: u16,

    /// Coordinator address (host:port)
    #[arg(long, default_value = "127.0.0.1:7870")]
    coordinator: String,

    /// HTTP port for metrics endpoint
    #[arg(long, default_value_t = 8200)]
    metrics_port: u16,
}

/// Incoming server snapshot — sent through a channel to avoid lock contention.
struct InboundSnapshot {
    server_id: u16,
    msg: ServerMessage,
    raw_size: usize,
}

/// Connection/routing state (rarely changes — only on connect/disconnect).
struct ConnState {
    server_senders: HashMap<u16, mpsc::UnboundedSender<String>>,
    client_senders: Vec<mpsc::UnboundedSender<Vec<u8>>>,
    client_players: HashMap<usize, String>,
    next_client_id: usize,
}

impl ConnState {
    fn new() -> Self {
        Self {
            server_senders: HashMap::new(),
            client_senders: Vec::new(),
            client_players: HashMap::new(),
            next_client_id: 0,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Aggregator behind its own mutex — only the broadcast loop touches it
    let aggregator = Arc::new(Mutex::new(Aggregator::new()));
    // Connection state behind RwLock — rarely written
    let conn = Arc::new(RwLock::new(ConnState::new()));
    // Channel for server snapshots → broadcast loop (no lock contention)
    let (snap_tx, mut snap_rx) = mpsc::unbounded_channel::<InboundSnapshot>();

    tracing::info!("Rust gateway starting (ws:{} metrics:{})", args.port, args.metrics_port);

    // Start metrics HTTP server
    let metrics_agg = Arc::clone(&aggregator);
    let metrics_conn = Arc::clone(&conn);
    tokio::spawn(run_metrics_server(args.metrics_port, metrics_agg, metrics_conn));

    // Start client WebSocket listener
    let client_listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind gateway port");
    tracing::info!("Gateway listening for clients on port {}", args.port);

    // Spawn client acceptor
    let accept_conn = Arc::clone(&conn);
    let accept_agg = Arc::clone(&aggregator);
    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = client_listener.accept().await {
                let c = Arc::clone(&accept_conn);
                let a = Arc::clone(&accept_agg);
                tokio::spawn(handle_client(stream, addr, c, a));
            }
        }
    });

    // Connect to coordinator
    let coord_conn = Arc::clone(&conn);
    let coord_agg = Arc::clone(&aggregator);
    tokio::spawn(run_coordinator(args.coordinator, args.port, coord_conn, coord_agg, snap_tx));

    // Broadcast loop: fixed timer matching tick rate.
    // Drains all pending server snapshots, then broadcasts to clients.
    let broadcast_conn = Arc::clone(&conn);
    let broadcast_agg = Arc::clone(&aggregator);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        shared::tick::TICK_DURATION_MS,
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        {
            let mut agg = broadcast_agg.lock().await;
            while let Ok(snap) = snap_rx.try_recv() {
                agg.on_server_snapshot(snap.server_id, snap.msg, snap.raw_size);
            }

            let metrics = agg.update_metrics();
            let frame = agg.get_broadcast_frame(&metrics);

            if let Some((binary_bytes, _count)) = frame {
                let t0 = Instant::now();
                let c = broadcast_conn.read().await;
                for tx in &c.client_senders {
                    let _ = tx.send(binary_bytes.clone());
                }
                drop(c);
                agg.last_broadcast_us = t0.elapsed().as_micros() as u64;
            }
        }
    }
}

async fn handle_client(
    stream: TcpStream,
    addr: std::net::SocketAddr,
    conn: Arc<RwLock<ConnState>>,
    aggregator: Arc<Mutex<Aggregator>>,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let (mut ws_sink, mut ws_stream) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let client_id;
    {
        let mut c = conn.write().await;
        client_id = c.next_client_id;
        c.next_client_id += 1;
        c.client_senders.push(tx);

        let server_ids: Vec<u16> = c.server_senders.keys()
            .filter(|&&id| id > 0)
            .copied()
            .collect();

        if !server_ids.is_empty() {
            let agg = aggregator.lock().await;
            let target_id = agg.get_least_loaded_server(&server_ids);
            drop(agg);

            let player_id = uuid::Uuid::new_v4().to_string();

            if let Some(server_tx) = c.server_senders.get(&target_id) {
                let spawn_msg = serde_json::json!({
                    "type": "SpawnPlayer",
                    "player_id": player_id,
                    "position": [0, 0, 0]
                });
                let _ = server_tx.send(serde_json::to_string(&spawn_msg).unwrap());
            }

            let assign_msg = serde_json::json!({
                "type": "PlayerAssigned",
                "player_id": player_id,
            });
            let _ = ws_sink.send(Message::Text(serde_json::to_string(&assign_msg).unwrap().into())).await;

            c.client_players.insert(client_id, player_id.clone());
            tracing::info!("Client {addr} assigned player {} to S{target_id}", &player_id[..8]);
        }
    }

    tracing::info!("Client connected from {addr}");

    // Forward broadcast snapshots to this client
    let sink_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    // Read client inputs and route them
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(&text) {
                let c = conn.read().await;
                let agg = aggregator.lock().await;
                route_input(&input, &c, &agg);
            }
        }
    }

    // Client disconnected
    {
        let mut c = conn.write().await;
        if let Some(player_id) = c.client_players.remove(&client_id) {
            let agg = aggregator.lock().await;
            let auth = agg.get_authority_server(&player_id);
            drop(agg);
            if let Some(server_id) = auth {
                if let Some(tx) = c.server_senders.get(&server_id) {
                    let remove = serde_json::json!({
                        "type": "RemoveEntity",
                        "entity_id": player_id,
                    });
                    let _ = tx.send(serde_json::to_string(&remove).unwrap());
                }
            }
            tracing::info!("Client {addr} disconnected, removed player {}", &player_id[..8]);
        }
    }

    sink_task.abort();
}

fn route_input(msg: &serde_json::Value, conn: &ConnState, agg: &Aggregator) {
    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let json = serde_json::to_string(msg).unwrap();

    match msg_type {
        "MoveInput" => {
            if let Some(entity_id) = msg.get("entity_id").and_then(|v| v.as_str()) {
                if let Some(server_id) = agg.get_authority_server(entity_id) {
                    if let Some(tx) = conn.server_senders.get(&server_id) {
                        let _ = tx.send(json);
                        return;
                    }
                }
            }
            broadcast_to_servers(&json, &conn.server_senders);
        }
        "SpawnEntity" => {
            let server_ids: Vec<u16> = conn.server_senders.keys()
                .filter(|&&id| id > 0)
                .copied()
                .collect();
            if server_ids.is_empty() { return; }
            let target = server_ids[rand_index(server_ids.len())];
            if let Some(tx) = conn.server_senders.get(&target) {
                let _ = tx.send(json);
            }
        }
        _ => {
            broadcast_to_servers(&json, &conn.server_senders);
        }
    }
}

fn broadcast_to_servers(json: &str, senders: &HashMap<u16, mpsc::UnboundedSender<String>>) {
    for (&id, tx) in senders {
        if id > 0 {
            let _ = tx.send(json.to_string());
        }
    }
}

fn rand_index(len: usize) -> usize {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as usize;
    nanos % len
}

// ── Coordinator connection ──

async fn run_coordinator(
    coordinator_addr: String,
    gateway_port: u16,
    conn: Arc<RwLock<ConnState>>,
    aggregator: Arc<Mutex<Aggregator>>,
    snap_tx: mpsc::UnboundedSender<InboundSnapshot>,
) {
    loop {
        tracing::info!("Connecting to coordinator at {coordinator_addr}...");
        let url = format!("ws://{coordinator_addr}");
        let ws = match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                tracing::warn!("Coordinator connection failed: {e}, retrying in 2s...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        tracing::info!("Connected to coordinator");
        let (mut ws_sink, mut ws_stream) = ws.split();

        let reg = CoordClientMsg::Register {
            server_id: 0,
            udp_port: 0,
            ws_port: gateway_port,
            host: None,
        };
        if let Ok(json) = serde_json::to_string(&reg) {
            let _ = ws_sink.send(Message::Text(json.into())).await;
        }

        while let Some(Ok(msg)) = ws_stream.next().await {
            if let Message::Text(text) = msg {
                if let Ok(CoordServerMsg::PeerList { peers }) = serde_json::from_str(&text) {
                    let sim_peers: Vec<_> = peers.iter().filter(|p| p.server_id > 0).collect();
                    tracing::info!(
                        "Peer list: {}",
                        sim_peers.iter().map(|p| format!("S{}({})", p.server_id, p.ws_addr)).collect::<Vec<_>>().join(", ")
                    );

                    let active_ids: std::collections::HashSet<u16> = sim_peers.iter().map(|p| p.server_id).collect();

                    for peer in &sim_peers {
                        let has = conn.read().await.server_senders.contains_key(&peer.server_id);
                        if !has {
                            let server_id = peer.server_id;
                            let ws_addr = peer.ws_addr.clone();
                            let c = Arc::clone(&conn);
                            let tx = snap_tx.clone();
                            tokio::spawn(connect_to_server(server_id, ws_addr, c, tx));
                        }
                    }

                    // Remove stale peers
                    let mut c = conn.write().await;
                    let stale: Vec<u16> = c.server_senders.keys()
                        .filter(|&&id| id > 0 && !active_ids.contains(&id))
                        .copied()
                        .collect();
                    for id in stale {
                        c.server_senders.remove(&id);
                        let mut agg = aggregator.lock().await;
                        agg.remove_server(id);
                        tracing::info!("Removed stale server S{id}");
                    }
                }
            }
        }

        tracing::warn!("Lost coordinator connection, retrying in 2s...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn connect_to_server(
    server_id: u16,
    ws_addr: String,
    conn: Arc<RwLock<ConnState>>,
    snap_tx: mpsc::UnboundedSender<InboundSnapshot>,
) {
    let url = format!("ws://{ws_addr}");
    tracing::info!("Connecting to S{server_id} at {url}...");

    let ws = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::warn!("Failed to connect to S{server_id}: {e}");
            return;
        }
    };

    tracing::info!("Connected to S{server_id}");
    let (mut ws_sink, mut ws_stream) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        let mut c = conn.write().await;
        c.server_senders.insert(server_id, tx);
    }

    let sink_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read server snapshots — send through channel, NO lock acquisition
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            let raw_size = text.len();
            if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(&text) {
                if matches!(server_msg, ServerMessage::WorldSnapshot { .. }) {
                    let _ = snap_tx.send(InboundSnapshot {
                        server_id,
                        msg: server_msg,
                        raw_size,
                    });
                }
            }
        }
    }

    // Server disconnected
    {
        let mut c = conn.write().await;
        c.server_senders.remove(&server_id);
    }
    sink_task.abort();
    tracing::info!("Lost connection to S{server_id}");
}

// ── Metrics HTTP server ──

async fn run_metrics_server(
    port: u16,
    aggregator: Arc<Mutex<Aggregator>>,
    conn: Arc<RwLock<ConnState>>,
) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind gateway metrics port {port}: {e}");
            return;
        }
    };
    tracing::info!("Gateway metrics on port {port}");

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let agg = Arc::clone(&aggregator);
        let conn = Arc::clone(&conn);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;

            let mut a = agg.lock().await;
            let gw_metrics = a.update_metrics();
            let c = conn.read().await;
            let metrics = serde_json::json!({
                "service": "gateway-rust",
                "snapshots_per_sec": gw_metrics.snapshots_per_sec,
                "total_entities": gw_metrics.total_entities,
                "aggregate_us": gw_metrics.aggregate_us,
                "broadcast_us": gw_metrics.broadcast_us,
                "bandwidth_kbps": gw_metrics.bandwidth_kbps,
                "rebalance_events": gw_metrics.rebalance_events,
                "server_count": c.server_senders.len(),
                "client_count": c.client_senders.len(),
                "broadcast_tick": a.broadcast_tick,
            });
            drop(a);
            drop(c);

            let body = serde_json::to_string_pretty(&metrics).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}
