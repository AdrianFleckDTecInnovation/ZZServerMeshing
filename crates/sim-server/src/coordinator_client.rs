use crate::world::World;
use futures_util::{SinkExt, StreamExt};
use shared::protocol::{CoordClientMsg, CoordServerMsg};
use std::sync::Arc;
use tokio::net::lookup_host;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

/// Connect to the coordinator, register this server, and keep the peer list updated.
pub async fn run_coordinator_client(
    coordinator_addr: String,
    server_id: u16,
    udp_port: u16,
    ws_port: u16,
    world: Arc<RwLock<World>>,
    advertise_host: Option<String>,
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

        // Register
        let reg = CoordClientMsg::Register {
            server_id,
            udp_port,
            ws_port,
            host: advertise_host.clone(),
        };
        if let Ok(json) = serde_json::to_string(&reg) {
            let _ = ws_sink.send(Message::Text(json.into())).await;
        }

        // Heartbeat ticker
        let hb_world = Arc::clone(&world);
        let mut hb_sink = ws_sink;
        let hb_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let entity_count = {
                    let w = hb_world.read().await;
                    w.entities.len() as u32
                };
                let hb = CoordClientMsg::Heartbeat {
                    server_id,
                    entity_count,
                };
                if let Ok(json) = serde_json::to_string(&hb) {
                    if hb_sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Listen for coordinator messages
        while let Some(Ok(msg)) = ws_stream.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<CoordServerMsg>(&text) {
                    Ok(CoordServerMsg::PeerList { peers }) => {
                        let mut w = world.write().await;
                        w.peer_addrs.clear();
                        for peer in &peers {
                            // Try direct parse first (IP:port), fall back to DNS resolution (hostname:port)
                            let addr = match peer.udp_addr.parse() {
                                Ok(a) => Some(a),
                                Err(_) => lookup_host(&peer.udp_addr).await.ok().and_then(|mut addrs| addrs.next()),
                            };
                            if let Some(addr) = addr {
                                w.peer_addrs.insert(peer.server_id, addr);
                            } else {
                                tracing::warn!("Could not resolve peer address: {}", peer.udp_addr);
                            }
                        }
                        tracing::info!(
                            "Peer list updated: {} peers {:?}",
                            peers.len(),
                            peers.iter().map(|p| format!("S{}", p.server_id)).collect::<Vec<_>>()
                        );
                    }
                    Ok(CoordServerMsg::RebalanceDirective { entity_ids, target_server }) => {
                        let mut w = world.write().await;
                        w.pending_transfers = Some((entity_ids, target_server));
                        tracing::info!("Received rebalance directive -> S{target_server}");
                    }
                    Ok(CoordServerMsg::DrainDirective { target_servers }) => {
                        let mut w = world.write().await;
                        w.draining = true;
                        w.drain_targets = Some(target_servers.clone());
                        tracing::info!(
                            "Received drain directive -> {:?}",
                            target_servers.iter().map(|s| format!("S{s}")).collect::<Vec<_>>()
                        );
                    }
                    Ok(CoordServerMsg::RecoverEntities { dead_server_id, survivors }) => {
                        let mut w = world.write().await;
                        w.recover_from_dead_server(dead_server_id, &survivors);
                    }
                    Err(e) => {
                        tracing::warn!("Bad coordinator message: {e}");
                    }
                }
            }
        }

        hb_task.abort();
        tracing::warn!("Lost coordinator connection, reconnecting...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
