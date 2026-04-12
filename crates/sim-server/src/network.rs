use crate::world::World;
use shared::entity::EntityId;
use shared::protocol::{ClientMessage, ServerMessage};
use shared::tick::TICK_DURATION_MS;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

pub async fn run_websocket_server(
    port: u16,
    world: Arc<RwLock<World>>,
    input_tx: mpsc::UnboundedSender<ClientMessage>,
) {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind WebSocket port");

    tracing::info!("WebSocket server listening on port {port}");

    // Broadcast channel for world state -> all clients
    let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    // Spawn the broadcast ticker: reads world state and sends to all clients
    let bcast_tx = broadcast_tx.clone();
    let bcast_world = Arc::clone(&world);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(TICK_DURATION_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let w = bcast_world.read().await;
            let snapshot = ServerMessage::WorldSnapshot {
                tick: w.tick,
                entities: w.get_client_snapshot(),
            };
            drop(w);

            if let Ok(json) = serde_json::to_string(&snapshot) {
                // Ignore send errors (no receivers)
                let _ = bcast_tx.send(json);
            }
        }
    });

    // Accept client connections
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Accept error: {e}");
                continue;
            }
        };

        tracing::info!("Client connected from {addr}");

        let ws_stream = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!("WebSocket handshake error: {e}");
                continue;
            }
        };

        let input_tx = input_tx.clone();
        let mut broadcast_rx = broadcast_tx.subscribe();
        let world_clone = Arc::clone(&world);

        tokio::spawn(async move {
            let (mut ws_sink, mut ws_stream) = ws_stream.split();

            // Spawn a player entity for this client
            let player_uuid = uuid::Uuid::new_v4();
            {
                let mut w = world_clone.write().await;
                w.spawn_player_with_id(player_uuid, glam::Vec3::ZERO);
            }

            // Send PlayerAssigned to the client
            let assign_msg = ServerMessage::PlayerAssigned {
                player_id: player_uuid.to_string(),
            };
            if let Ok(json) = serde_json::to_string(&assign_msg) {
                if ws_sink.send(Message::Text(json.into())).await.is_err() {
                    // Client disconnected immediately — clean up
                    let mut w = world_clone.write().await;
                    w.remove_entity(&EntityId(player_uuid));
                    return;
                }
            }

            tracing::info!(
                "Assigned player {} to client {addr}",
                &player_uuid.to_string()[..8]
            );

            loop {
                tokio::select! {
                    // Forward broadcast state to this client
                    msg = broadcast_rx.recv() => {
                        match msg {
                            Ok(json) => {
                                if ws_sink.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Client {addr} lagged {n} messages");
                            }
                            Err(_) => break,
                        }
                    }
                    // Receive input from this client
                    msg = ws_stream.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<ClientMessage>(&text) {
                                    Ok(input) => {
                                        let _ = input_tx.send(input);
                                    }
                                    Err(e) => {
                                        tracing::warn!("Invalid client message: {e}");
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::info!("Client {addr} disconnected");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Client disconnected — remove their player entity
            let mut w = world_clone.write().await;
            w.remove_entity(&EntityId(player_uuid));
            tracing::info!(
                "Removed player {} on disconnect",
                &player_uuid.to_string()[..8]
            );
        });
    }
}
