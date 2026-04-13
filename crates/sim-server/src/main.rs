mod network;
mod receiver;
mod coordinator_client;
mod metrics;
mod metrics_server;
mod components;
mod resources;
mod conversion;
#[allow(dead_code)]
mod encoding;
mod delta;
mod systems;
mod schedule;
mod game_loop;

use clap::Parser;
use shared::entity::ServerId;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

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

    /// Path to TLS certificate file (PEM). When both --tls-cert and --tls-key
    /// are provided, the WebSocket server accepts TLS connections (wss://).
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// Path to TLS private key file (PEM).
    #[arg(long)]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let server_id = ServerId(args.server_id);
    let metrics_port = args.metrics_port.unwrap_or(8100 + args.server_id);

    // Build TLS config if cert+key provided
    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let acceptor = build_tls_acceptor(cert_path, key_path)
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
        server_id, args.port, args.udp_port, metrics_port
    );

    // Create shared async-to-ECS buffers
    let shared_inbound = Arc::new(game_loop::SharedInbound {
        queue: tokio::sync::Mutex::new(resources::InboundQueue::default()),
    });
    let shared_coord_events = Arc::new(game_loop::SharedCoordEvents {
        queue: tokio::sync::Mutex::new(resources::CoordEventQueue::default()),
    });

    // Shared entity count for coordinator heartbeats
    let entity_count_atomic = Arc::new(AtomicU32::new(0));

    // Create metrics collector
    let metrics_collector = Arc::new(Mutex::new(metrics::MetricsCollector::new(args.server_id)));

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
        tls_acceptor,
    ));

    // Start UDP receiver (incoming peer entity states)
    let recv_handle = tokio::spawn(receiver::run_udp_receiver(
        Arc::clone(&udp_socket),
        Arc::clone(&shared_inbound),
    ));

    // Connect to coordinator
    let coord_handle = tokio::spawn(coordinator_client::run_coordinator_client(
        args.coordinator,
        args.server_id,
        args.udp_port,
        args.port,
        Arc::clone(&shared_coord_events),
        args.advertise_host,
        Arc::clone(&entity_count_atomic),
        use_tls,
    ));

    // Run the ECS game loop
    let sim_handle = tokio::spawn(game_loop::run_game_loop_ecs(
        server_id,
        args.initial_npcs,
        Arc::clone(&shared_inbound),
        Arc::clone(&shared_coord_events),
        Arc::clone(&udp_socket),
        Arc::clone(&metrics_collector),
        Arc::clone(&entity_count_atomic),
    ));

    tokio::select! {
        r = metrics_handle => { if let Err(e) = r { tracing::error!("Metrics error: {e}"); } }
        r = ws_handle => { if let Err(e) = r { tracing::error!("WS error: {e}"); } }
        r = recv_handle => { if let Err(e) = r { tracing::error!("UDP receiver error: {e}"); } }
        r = coord_handle => { if let Err(e) = r { tracing::error!("Coordinator client error: {e}"); } }
        r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal -- initiating graceful drain");
            // Push drain event through the coordinator event queue
            let mut q = shared_coord_events.queue.lock().await;
            // Drain targets: we need the peer addresses, but we don't have direct access
            // to the ECS world. Instead, push an empty drain directive -- the game loop
            // will use whatever peers it knows about.
            q.drain_directives.push(vec![]);
            drop(q);
            // Give the game loop time to drain entities
            tracing::info!("Draining entities to peers...");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            tracing::info!("Shutdown complete");
        }
    }
}

/// Build a tokio-rustls TLS acceptor from PEM certificate and key files.
fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    use rustls::pki_types::PrivateKeyDer;
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("Failed to open cert file {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to parse certs: {e}"))?;
    if certs.is_empty() {
        return Err("No certificates found in cert file".into());
    }

    let key_file = std::fs::File::open(key_path)
        .map_err(|e| format!("Failed to open key file {}: {e}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("Failed to parse private key: {e}"))?
        .ok_or("No private key found in key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("Failed to build TLS config: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}
