use crate::game_loop::SharedCoordEvents;
use futures_util::{SinkExt, StreamExt};
use shared::protocol::{CoordClientMsg, CoordServerMsg};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

/// Connect to the coordinator, register this server, and push events to the shared queue.
pub async fn run_coordinator_client(
    coordinator_addr: String,
    server_id: u16,
    udp_port: u16,
    ws_port: u16,
    coord_events: Arc<SharedCoordEvents>,
    advertise_host: Option<String>,
    entity_count: Arc<AtomicU32>,
    tls: bool,
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
            tls,
        };
        if let Ok(json) = serde_json::to_string(&reg) {
            let _ = ws_sink.send(Message::Text(json.into())).await;
        }

        // Heartbeat ticker
        let hb_entity_count = Arc::clone(&entity_count);
        let mut hb_sink = ws_sink;
        let hb_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let count = hb_entity_count.load(Ordering::Relaxed);
                let hb = CoordClientMsg::Heartbeat {
                    server_id,
                    entity_count: count,
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
                        let mut q = coord_events.queue.lock().await;
                        q.peer_list_updates.push(peers);
                        tracing::info!("Received PeerList update");
                    }
                    Ok(CoordServerMsg::RebalanceDirective {
                        entity_ids,
                        target_server,
                    }) => {
                        let mut q = coord_events.queue.lock().await;
                        q.rebalance_directives.push((entity_ids, target_server));
                        tracing::info!("Received rebalance directive -> S{target_server}");
                    }
                    Ok(CoordServerMsg::DrainDirective { target_servers }) => {
                        let mut q = coord_events.queue.lock().await;
                        q.drain_directives.push(target_servers.clone());
                        tracing::info!(
                            "Received drain directive -> {:?}",
                            target_servers
                                .iter()
                                .map(|s| format!("S{s}"))
                                .collect::<Vec<_>>()
                        );
                    }
                    Ok(CoordServerMsg::RecoverEntities {
                        dead_server_id,
                        survivors,
                    }) => {
                        let mut q = coord_events.queue.lock().await;
                        q.recover_events.push((dead_server_id, survivors));
                    }
                    Ok(CoordServerMsg::PlayerTransferResult {
                        account_id,
                        success,
                        error,
                    }) => {
                        if success {
                            tracing::info!(account_id, "Player transfer confirmed by coordinator");
                        } else {
                            tracing::warn!(
                                account_id,
                                error = error.as_deref().unwrap_or("unknown"),
                                "Player transfer rejected by coordinator"
                            );
                        }
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
