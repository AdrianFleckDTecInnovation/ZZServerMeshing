mod registry;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use registry::Registry;
use shared::protocol::CoordClientMsg;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(name = "coordinator", about = "Server mesh coordinator")]
struct Args {
    /// Port for sim-server connections (WebSocket)
    #[arg(long, default_value_t = 7870)]
    port: u16,
}

/// Internal event from a sim-server connection to the main registry loop.
enum RegistryEvent {
    Register {
        server_id: u16,
        udp_port: u16,
        ws_port: u16,
        host: Option<String>,
        /// Channel to send messages back to this server's WS connection.
        reply_tx: mpsc::UnboundedSender<String>,
    },
    Heartbeat {
        server_id: u16,
        entity_count: u32,
    },
    Disconnected {
        server_id: u16,
    },
    Draining {
        server_id: u16,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let (event_tx, event_rx) = mpsc::unbounded_channel::<RegistryEvent>();

    // Spawn the registry manager
    tokio::spawn(run_registry(event_rx));

    // Accept sim-server WebSocket connections
    let listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind coordinator port");
    tracing::info!("Coordinator listening on port {}", args.port);

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Accept error: {e}");
                continue;
            }
        };

        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!("WS handshake error from {addr}: {e}");
                continue;
            }
        };

        tracing::info!("Sim-server connecting from {addr}");
        let event_tx = event_tx.clone();
        tokio::spawn(handle_server_connection(ws, addr, event_tx));
    }
}

async fn handle_server_connection(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    addr: std::net::SocketAddr,
    event_tx: mpsc::UnboundedSender<RegistryEvent>,
) {
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();

    let mut server_id: Option<u16> = None;

    // Forward outbound messages to WS
    let sink_task = tokio::spawn(async move {
        while let Some(msg) = reply_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read inbound messages
    while let Some(Ok(msg)) = ws_stream.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<CoordClientMsg>(&text) {
                Ok(CoordClientMsg::Register {
                    server_id: sid,
                    udp_port,
                    ws_port,
                    host,
                }) => {
                    // Use advertised host if provided, otherwise use the peer's source IP
                    let resolved_host = host.unwrap_or_else(|| addr.ip().to_string());
                    server_id = Some(sid);
                    let _ = event_tx.send(RegistryEvent::Register {
                        server_id: sid,
                        udp_port,
                        ws_port,
                        host: Some(resolved_host),
                        reply_tx: reply_tx.clone(),
                    });
                }
                Ok(CoordClientMsg::Heartbeat {
                    server_id: sid,
                    entity_count,
                }) => {
                    let _ = event_tx.send(RegistryEvent::Heartbeat {
                        server_id: sid,
                        entity_count,
                    });
                }
                Ok(CoordClientMsg::Draining { server_id: sid }) => {
                    let _ = event_tx.send(RegistryEvent::Draining { server_id: sid });
                }
                Err(e) => {
                    tracing::warn!("Bad message from {addr}: {e}");
                }
            }
        }
    }

    // Server disconnected
    if let Some(sid) = server_id {
        tracing::info!("Sim-server S{sid} disconnected ({addr})");
        let _ = event_tx.send(RegistryEvent::Disconnected { server_id: sid });
    }
    sink_task.abort();
}

async fn run_registry(mut event_rx: mpsc::UnboundedReceiver<RegistryEvent>) {
    let mut registry = Registry::new();
    let mut rebalance_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let rebalance_cooldown = std::time::Duration::from_secs(5);
    let mut last_rebalance = std::time::Instant::now() - rebalance_cooldown;

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    RegistryEvent::Register { server_id, udp_port, ws_port, host, reply_tx } => {
                        registry.register(server_id, udp_port, ws_port, host, reply_tx);
                        tracing::info!(
                            "Registered S{server_id} (udp:{udp_port} ws:{ws_port}) | {} servers total",
                            registry.server_count()
                        );
                        registry.broadcast_peer_list();
                    }
                    RegistryEvent::Heartbeat { server_id, entity_count } => {
                        registry.heartbeat(server_id, entity_count);
                    }
                    RegistryEvent::Disconnected { server_id } => {
                        let had_entities = registry.server_count() > 0;
                        registry.unregister(server_id);
                        tracing::info!(
                            "Unregistered S{server_id} | {} servers remain",
                            registry.server_count()
                        );
                        registry.broadcast_peer_list();
                        // If this was an unexpected disconnect (crash), trigger recovery
                        if server_id > 0 && had_entities {
                            tracing::warn!("S{server_id} crashed — broadcasting recovery");
                            registry.broadcast_recovery(server_id);
                        }
                    }
                    RegistryEvent::Draining { server_id } => {
                        tracing::info!("S{server_id} requesting graceful drain");
                        registry.drain_server(server_id);
                    }
                }
            }
            _ = rebalance_interval.tick() => {
                // Check for dead servers
                let dead = registry.detect_dead_servers();
                for sid in dead {
                    tracing::warn!("S{sid} missed heartbeat — marking as dead");
                    registry.unregister(sid);
                    registry.broadcast_peer_list();
                    registry.broadcast_recovery(sid);
                }
                // Rebalance
                if last_rebalance.elapsed() >= rebalance_cooldown && registry.try_rebalance() {
                    last_rebalance = std::time::Instant::now();
                }
            }
        }
    }
}
