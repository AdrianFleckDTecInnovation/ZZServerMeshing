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
    weighted_load: Arc<AtomicU32>,
    tls: bool,
    coord_outbound: Arc<crate::game_loop::SharedCoordOutbound>,
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
            protocol_version: shared::PROTOCOL_VERSION,
        };
        if let Ok(json) = serde_json::to_string(&reg) {
            let _ = ws_sink.send(Message::Text(json.into())).await;
        }

        // Heartbeat ticker + outbound coordinator message drain
        let hb_entity_count = Arc::clone(&entity_count);
        let hb_weighted_load = Arc::clone(&weighted_load);
        let hb_coord_outbound = Arc::clone(&coord_outbound);
        let mut hb_sink = ws_sink;
        let hb_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;

                // Drain any outbound coordinator messages (TransferResult, etc.)
                let outbound: Vec<CoordClientMsg> = {
                    let mut q = hb_coord_outbound.queue.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *q)
                };
                for msg in outbound {
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if hb_sink.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                }

                let count = hb_entity_count.load(Ordering::Relaxed);
                let wl = hb_weighted_load.load(Ordering::Relaxed);
                let hb = CoordClientMsg::Heartbeat {
                    server_id,
                    entity_count: count,
                    weighted_load: wl,
                };
                if let Ok(json) = serde_json::to_string(&hb) {
                    if hb_sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Listen for coordinator messages.
        // Read timeout as defense-in-depth: if the TCP connection is half-open
        // (no FIN received), we won't block forever waiting for frames.
        // The coordinator sends keepalives every 5s on the rebalance interval,
        // so 30s gives 6 missed keepalives before we consider the connection dead.
        let read_timeout = std::time::Duration::from_secs(30);
        loop {
            let frame = tokio::time::timeout(read_timeout, ws_stream.next()).await;
            let msg = match frame {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(e))) => {
                    tracing::warn!("Coordinator WebSocket error: {e}");
                    break;
                }
                Ok(None) => {
                    // Stream ended (connection closed cleanly)
                    break;
                }
                Err(_) => {
                    // Read timeout — assume connection is dead
                    tracing::warn!(
                        server_id,
                        "No coordinator message for {read_timeout:?}, assuming dead connection"
                    );
                    break;
                }
            };
            if let Message::Text(text) = msg {
                match serde_json::from_str::<CoordServerMsg>(&text) {
                    Ok(CoordServerMsg::PeerList { peers }) => {
                        let mut q = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.peer_list_updates.push(peers);
                        tracing::info!("Received PeerList update");
                    }
                    Ok(CoordServerMsg::RebalanceDirective {
                        entity_ids,
                        target_server,
                    }) => {
                        let mut q = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.rebalance_directives.push((entity_ids, target_server));
                        tracing::info!("Received rebalance directive -> S{target_server}");
                    }
                    Ok(CoordServerMsg::DrainDirective { target_servers }) => {
                        let mut q = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut q = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
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
                    Ok(CoordServerMsg::Keepalive) => {
                        // Coordinator is alive — nothing to do, the read timeout is reset
                    }
                    Err(e) => {
                        tracing::warn!("Bad coordinator message: {e}");
                    }
                }
            }
        }

        hb_task.abort();
        tracing::warn!(server_id, "Lost coordinator connection, will re-register as S{server_id} in 2s...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
