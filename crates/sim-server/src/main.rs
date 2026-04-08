mod world;
mod simulation;
mod network;
mod replication;
mod receiver;
mod coordinator_client;
mod metrics;
mod metrics_server;

use clap::Parser;
use shared::entity::ServerId;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, RwLock};

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

    /// Number of NPCs to spawn initially
    #[arg(long, default_value_t = 20)]
    initial_npcs: u32,

    /// HTTP port for metrics endpoint (default: 8100 + server_id)
    #[arg(long)]
    metrics_port: Option<u16>,

    /// Advertised hostname for peer discovery (for Docker/k8s networking)
    #[arg(long)]
    advertise_host: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let server_id = ServerId(args.server_id);
    let metrics_port = args.metrics_port.unwrap_or(8100 + args.server_id);
    tracing::info!(
        "Sim-server {} starting (ws:{} udp:{} metrics:{})",
        server_id, args.port, args.udp_port, metrics_port
    );

    // Create the world
    let mut world = world::World::new(server_id);
    world.spawn_initial_npcs(args.initial_npcs);
    tracing::info!("Spawned {} initial NPCs", args.initial_npcs);

    let world = Arc::new(RwLock::new(world));

    // Create metrics collector
    let metrics_collector = Arc::new(Mutex::new(metrics::MetricsCollector::new(args.server_id)));

    // Bind UDP socket for peer communication
    let udp_socket = Arc::new(
        UdpSocket::bind(format!("0.0.0.0:{}", args.udp_port))
            .await
            .expect("Failed to bind UDP port"),
    );
    tracing::info!("UDP bound on port {}", args.udp_port);

    // Channel for client inputs -> game loop
    let (input_tx, input_rx) = mpsc::unbounded_channel();

    // Start metrics HTTP server
    let metrics_handle = tokio::spawn(metrics_server::run_metrics_server(
        metrics_port,
        Arc::clone(&metrics_collector),
    ));

    // Start WebSocket server for clients/gateway
    let ws_world = Arc::clone(&world);
    let ws_handle = tokio::spawn(network::run_websocket_server(
        args.port,
        ws_world,
        input_tx,
    ));

    // Start UDP receiver (incoming peer entity states)
    let recv_world = Arc::clone(&world);
    let recv_socket = Arc::clone(&udp_socket);
    let recv_handle = tokio::spawn(receiver::run_udp_receiver(recv_socket, recv_world));

    // Connect to coordinator
    let coord_world = Arc::clone(&world);
    let coord_handle = tokio::spawn(coordinator_client::run_coordinator_client(
        args.coordinator,
        args.server_id,
        args.udp_port,
        args.port,
        coord_world,
        args.advertise_host,
    ));

    // Run the game loop (includes replication)
    let sim_world = Arc::clone(&world);
    let sim_handle = tokio::spawn(simulation::run_game_loop(
        sim_world,
        input_rx,
        udp_socket,
        Arc::clone(&metrics_collector),
    ));

    tokio::select! {
        r = metrics_handle => { if let Err(e) = r { tracing::error!("Metrics error: {e}"); } }
        r = ws_handle => { if let Err(e) = r { tracing::error!("WS error: {e}"); } }
        r = recv_handle => { if let Err(e) = r { tracing::error!("UDP receiver error: {e}"); } }
        r = coord_handle => { if let Err(e) = r { tracing::error!("Coordinator client error: {e}"); } }
        r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal — initiating graceful drain");
            // Tell the world to drain, the game loop will handle it
            let mut w = world.write().await;
            w.draining = true;
            // Drain targets = all peers
            let targets: Vec<u16> = w.peer_addrs.keys().copied().collect();
            if targets.is_empty() {
                tracing::info!("No peers to drain to — shutting down immediately");
            } else {
                w.drain_targets = Some(targets);
                drop(w);
                // Give the game loop time to drain entities
                tracing::info!("Draining entities to peers...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            tracing::info!("Shutdown complete");
        }
    }
}
