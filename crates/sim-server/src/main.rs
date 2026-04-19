mod components;
mod conversion;
mod coordinator_client;
mod delta;
mod encoding;
mod game_loop;
mod metrics;
mod metrics_server;
mod network;
pub mod peer_bounds;
mod receiver;
mod resources;
mod schedule;
mod systems;

use clap::Parser;
use shared::entity::ServerId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, RwLock};
use tokio::net::UdpSocket;

#[derive(Parser, Debug)]
#[command(name = "sim-server", about = "Game simulation server")]
struct Args {
    /// Unique server ID
    #[arg(long, default_value_t = 1)]
    server_id: u16,

    /// WebSocket port for client/gateway connections
    #[arg(long, default_value_t = 7878)]
    port: u16,

    /// UDP port for peer server communication
    #[arg(long, default_value_t = 9101)]
    udp_port: u16,

    /// Coordinator address (host:port)
    #[arg(long, default_value = "127.0.0.1:7870")]
    coordinator: String,

    /// Number of NPCs to spawn initially (only honored when `--cold-start`
    /// is set; ignored otherwise — see WI-215 finding 085).
    #[arg(long, default_value_t = 20)]
    initial_npcs: u32,

    /// Spawn the `initial_npcs` NPCs on boot. Set only on fresh cluster
    /// bootstrap (i.e., when no surviving peer has already adopted this
    /// server's previous entities). On crash-restart or rolling redeploy,
    /// survivors hold the adopted entities and re-spawning would duplicate
    /// them; in that case, omit `--cold-start`. Deployment tooling
    /// (`start.sh`, `start-unoq.sh`) sets this on initial cluster launch;
    /// per-board restart paths (board-dashboard's Start button, systemd
    /// restart) do NOT.
    ///
    /// Default: false. See design 016 (Batch 5, WI-215).
    #[arg(long, default_value_t = false)]
    cold_start: bool,

    /// HTTP port for metrics endpoint (default: 8100 + server_id)
    #[arg(long)]
    metrics_port: Option<u16>,

    /// Advertised hostname for peer discovery (for Docker/k8s networking)
    #[arg(long)]
    advertise_host: Option<String>,

    /// Path to TLS certificate file (PEM). When both --tls-cert and --tls-key
    /// are provided, the WebSocket server accepts TLS connections (wss://).
    /// The same cert is also used as the trust anchor for the outbound
    /// coordinator connection when `--coordinator-tls` is set (WI-223).
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// Path to TLS private key file (PEM).
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Use WSS (TLS) when connecting to the coordinator control channel.
    /// Required when the coordinator serves its WS port over TLS (e.g., on
    /// the UNO Q boards, where both coordinator endpoints are TLS-only).
    /// When enabled, `--tls-cert` is also used as the trust anchor so a
    /// self-signed cert is accepted without an external CA.
    ///
    /// Default: false. See design 016 (Batch 6, WI-223).
    #[arg(long, default_value_t = false)]
    coordinator_tls: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let server_id = ServerId(args.server_id);
    let metrics_port = args.metrics_port.unwrap_or(8100 + args.server_id);

    // Generate a unique session id for this process run (design 013).
    // Used on every outbound PeerMessage so peers can detect a restart and
    // reset stale receiver state (reassembly map, latest_tick, shadows).
    //
    // WI-210 (finding 089): regenerate until nonzero. The value 0 collides
    // with the "never seen" sentinel at game_loop.rs peer_health snapshot
    // (`.unwrap_or(0)`) and metrics.rs, where 0 means "no session observed
    // yet". Making the production value nonzero keeps those sentinels
    // unambiguous. Collision probability with 0 is 1/2^64, but the cost
    // of the loop is negligible (expected single iteration).
    let session_id: u64 = loop {
        let s = rand::random::<u64>();
        if s != 0 {
            break s;
        }
    };
    tracing::info!(
        server_id = server_id.0,
        session_id = format_args!("0x{:016x}", session_id),
        "sim-server session id generated"
    );

    // Build TLS config if cert+key provided
    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let acceptor = shared::tls::build_tls_acceptor(cert_path, key_path)
                .expect("Failed to build TLS config from provided cert/key");
            tracing::info!(
                "TLS enabled for WebSocket server (cert={}, key={})",
                cert_path.display(),
                key_path.display()
            );
            Some(acceptor)
        }
        (Some(_), None) | (None, Some(_)) => {
            panic!("Both --tls-cert and --tls-key must be provided together");
        }
        (None, None) => None,
    };
    let use_tls = tls_acceptor.is_some();

    let ws_scheme = if use_tls { "wss" } else { "ws" };
    tracing::info!(
        "Sim-server {} starting ({ws_scheme}:{} udp:{} metrics:{})",
        server_id,
        args.port,
        args.udp_port,
        metrics_port
    );

    // Create shared async-to-ECS buffers
    let shared_inbound = Arc::new(std::sync::Mutex::new(resources::InboundQueue::default()));
    let shared_coord_events =
        Arc::new(std::sync::Mutex::new(resources::CoordEventQueue::default()));

    // Shared entity count for coordinator heartbeats
    let entity_count_atomic = Arc::new(AtomicU32::new(0));
    // Weighted load: players count 3x, NPCs count 1x
    let weighted_load_atomic = Arc::new(AtomicU32::new(0));
    // Outbound coordinator messages (TransferResult, etc.) from game loop
    let coord_outbound = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Create metrics collector
    let metrics_collector = Arc::new(std::sync::Mutex::new(metrics::MetricsCollector::new(
        args.server_id,
    )));

    // Shared peer health accumulator (WI-102/105): receiver writes, game loop reads
    let peer_health = Arc::new(std::sync::Mutex::new(
        game_loop::PeerHealthAccumulator::default(),
    ));

    // Drain-complete signal (WI-191). The game loop calls notify_one() when
    // ShutdownRequested fires and it's about to break the tick loop; main's
    // ctrl_c branch awaits .notified() with a 35 s timeout so the process
    // doesn't exit mid-drain.
    let drain_complete = Arc::new(tokio::sync::Notify::new());

    // WI-202: shared set of currently-authoritative Player EntityIds,
    // refreshed at the end of each ECS tick. The WS handler reads this to
    // detect when a claimed player entity has arrived via AuthorityTransfer
    // so it can adopt the existing entity instead of spawning a new one.
    let auth_players = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // WI-212: shared atomic of the current local tick. Game loop writes on
    // each frame; UDP receiver reads on session change to stamp
    // `peer_health.session_changed_at_tick`. `Relaxed` is sufficient.
    let local_tick_atomic: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // WI-213: shared set of currently-active peer server_ids. The ECS
    // (`process_coord_events`) writes on each PeerList; the UDP receiver
    // reads on its periodic sweep to prune state for departed peers.
    let active_peers: Arc<RwLock<std::collections::HashSet<u16>>> =
        Arc::new(RwLock::new(std::collections::HashSet::new()));

    // Bind UDP socket for peer communication
    let udp_socket = Arc::new(
        UdpSocket::bind(format!("0.0.0.0:{}", args.udp_port))
            .await
            .expect("Failed to bind UDP port"),
    );
    tracing::info!("UDP bound on port {}", args.udp_port);

    // Start metrics HTTP server
    let metrics_handle = tokio::spawn(metrics_server::run_metrics_server(
        metrics_port,
        Arc::clone(&metrics_collector),
    ));

    // Start WebSocket server for clients
    let ws_handle = tokio::spawn(network::run_websocket_server(
        args.port,
        Arc::clone(&shared_inbound),
        Arc::clone(&auth_players),
        tls_acceptor,
    ));

    // Start UDP receiver (incoming peer entity states)
    let recv_handle = tokio::spawn(receiver::run_udp_receiver(
        Arc::clone(&udp_socket),
        Arc::clone(&shared_inbound),
        Arc::clone(&peer_health),
        Arc::clone(&local_tick_atomic),
        Arc::clone(&active_peers),
    ));

    // WI-223: build the outbound TLS client config once. When
    // `--coordinator-tls` is set, we connect to the coordinator's WS port via
    // wss:// and trust only the cert at `--tls-cert`. If `--coordinator-tls`
    // is false, this is None and the client falls back to plain ws://.
    let coord_tls_config = if args.coordinator_tls {
        let cert_path = args.tls_cert.as_ref().expect(
            "--coordinator-tls requires --tls-cert to be set as the trust anchor",
        );
        Some(
            coordinator_client::build_coord_tls_config(cert_path)
                .expect("Failed to build TLS client config from --tls-cert"),
        )
    } else {
        None
    };

    // Connect to coordinator
    let coord_handle = tokio::spawn(coordinator_client::run_coordinator_client(
        args.coordinator,
        args.server_id,
        args.udp_port,
        args.port,
        Arc::clone(&shared_coord_events),
        args.advertise_host,
        Arc::clone(&entity_count_atomic),
        Arc::clone(&weighted_load_atomic),
        use_tls,
        Arc::clone(&coord_outbound),
        coord_tls_config,
    ));

    // Create two UDP sender channels: replication (bulk, best-effort) and
    // control (latency-critical: transfers, forwarded inputs).
    let (udp_repl_tx, udp_repl_rx) = tokio::sync::mpsc::channel::<game_loop::UdpOutbound>(
        game_loop::UDP_REPLICATION_CHANNEL_SIZE,
    );
    let (udp_ctrl_tx, udp_ctrl_rx) =
        tokio::sync::mpsc::channel::<game_loop::UdpOutbound>(game_loop::UDP_CONTROL_CHANNEL_SIZE);
    let sender_handle = tokio::spawn(game_loop::run_udp_sender(
        Arc::clone(&udp_socket),
        udp_ctrl_rx,
        udp_repl_rx,
    ));

    // Run the ECS game loop (receives both channel senders, not the socket)
    let sim_handle = tokio::spawn(game_loop::run_game_loop_ecs(
        server_id,
        session_id,
        args.initial_npcs,
        args.cold_start,
        Arc::clone(&shared_inbound),
        Arc::clone(&shared_coord_events),
        udp_repl_tx,
        udp_ctrl_tx,
        Arc::clone(&metrics_collector),
        Arc::clone(&entity_count_atomic),
        Arc::clone(&weighted_load_atomic),
        Arc::clone(&coord_outbound),
        Arc::clone(&peer_health),
        Arc::clone(&drain_complete),
        Arc::clone(&auth_players),
        Arc::clone(&local_tick_atomic),
        Arc::clone(&active_peers),
    ));

    // WI-192: Second-ctrl-c force-exit. The first SIGINT is consumed by the
    // main select arm below; subsequent SIGINTs mean the operator is
    // impatient, so we exit with code 130 (standard "terminated by SIGINT").
    // Tokio's signal handler broadcasts, so two concurrent waiters are safe.
    tokio::spawn(async {
        // Absorb the first SIGINT (the main select arm is also waiting).
        tokio::signal::ctrl_c().await.ok();
        // Any subsequent SIGINTs force exit.
        loop {
            tokio::signal::ctrl_c().await.ok();
            tracing::warn!("Received additional SIGINT -- forcing exit");
            std::process::exit(130);
        }
    });

    tokio::select! {
        r = metrics_handle => { if let Err(e) = r { tracing::error!("Metrics error: {e}"); } }
        r = ws_handle => { if let Err(e) = r { tracing::error!("WS error: {e}"); } }
        r = recv_handle => { if let Err(e) = r { tracing::error!("UDP receiver error: {e}"); } }
        r = coord_handle => { if let Err(e) = r { tracing::error!("Coordinator client error: {e}"); } }
        r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
        r = sender_handle => { if let Err(e) = r { tracing::error!("UDP sender error: {e}"); } }
        // WI-192: handle both SIGINT (interactive ctrl_c) and SIGTERM
        // (systemd default stop signal). Inner select fires on whichever
        // signal arrives first.
        _ = async {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate()
            ).expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        } => {
            tracing::info!(
                "Received shutdown signal -- sent Draining to coordinator, waiting for drain to complete (max 35s)"
            );
            // Send CoordClientMsg::Draining to the coordinator. The
            // coordinator's existing handler calls registry.drain_server(),
            // which broadcasts DrainDirective with the real target list
            // back to this server. process_coord_events then activates
            // the drain machinery via DrainState/DrainInfo.
            //
            // We push to the outbound coord queue (same queue the game
            // loop uses for TransferResult etc). The coordinator_client
            // task drains it. This replaces the old hack that pushed an
            // empty Vec onto shared_coord_events.drain_directives, which
            // was the wrong source of truth (finding 077).
            {
                let mut q = coord_outbound.lock().unwrap_or_else(|e| e.into_inner());
                q.push(shared::protocol::CoordClientMsg::Draining {
                    server_id: ServerId(args.server_id),
                });
            }
            // Await drain_complete.notified() with a 35 s ceiling. The
            // game loop signals this when ShutdownRequested fires.
            // DRAIN_TIMEOUT_TICKS is 600 ticks (30s); 5s slack covers the
            // coordinator round-trip + transfer ACK latency.
            match tokio::time::timeout(
                std::time::Duration::from_secs(35),
                drain_complete.notified(),
            ).await {
                Ok(()) => tracing::info!("Graceful drain complete"),
                Err(_) => tracing::warn!("Drain timeout -- forcing exit"),
            }
        }
    }
}
