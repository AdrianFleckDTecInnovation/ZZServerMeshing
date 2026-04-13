use crate::world::World;
use shared::entity::{EntityId, EntityState, EntityType};
use shared::protocol::{ClientMessage, ServerMessage};
use shared::tick::TICK_DURATION_MS;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

const HEADER_SIZE: usize = 27; // 1 + 8 + 4 + 14
const DELTA_HEADER_SIZE: usize = 31; // 1 + 8 + 4 + 4 + 14
const ENTITY_SIZE: usize = 47; // 16 + 12 + 12 + 1 + 2 + 4

/// Encode a single entity into the binary format (47 bytes).
fn encode_entity(buf: &mut Vec<u8>, e: &EntityState) {
    // UUID (16 bytes)
    buf.extend_from_slice(e.id.0.as_bytes());
    // Position (3 × f32 LE)
    buf.extend_from_slice(&e.position.x.to_le_bytes());
    buf.extend_from_slice(&e.position.y.to_le_bytes());
    buf.extend_from_slice(&e.position.z.to_le_bytes());
    // Velocity (3 × f32 LE)
    buf.extend_from_slice(&e.velocity.x.to_le_bytes());
    buf.extend_from_slice(&e.velocity.y.to_le_bytes());
    buf.extend_from_slice(&e.velocity.z.to_le_bytes());
    // Entity type (u8: 0 = Npc, 1 = Player)
    buf.push(match e.entity_type {
        EntityType::Player => 1,
        EntityType::Npc => 0,
    });
    // Authority server (u16 LE)
    buf.extend_from_slice(&e.authority_server.0.to_le_bytes());
    // Last input seq (u32 LE)
    buf.extend_from_slice(&e.last_input_seq.to_le_bytes());
}

/// Encode a full snapshot into the binary frame format expected by the client.
///
/// Format: [type:u8=0x01][tick:u64 LE][count:u32 LE][metrics:14 bytes]
///         [entity × N: 47 bytes each]
fn encode_binary_snapshot(tick: u64, entities: &[EntityState]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + entities.len() * ENTITY_SIZE);

    buf.push(0x01);
    buf.extend_from_slice(&tick.to_le_bytes());
    buf.extend_from_slice(&(entities.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0u8; 14]); // metrics placeholder

    for e in entities {
        encode_entity(&mut buf, e);
    }

    buf
}

/// Encode a delta frame: only changed/new entities + removed entity IDs.
///
/// Format: [type:u8=0x02][tick:u64 LE][update_count:u32 LE][removal_count:u32 LE][metrics:14 bytes]
///         [updated entities × N: 47 bytes each]
///         [removed UUIDs × M: 16 bytes each]
fn encode_binary_delta(tick: u64, updates: &[EntityState], removals: &[EntityId]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        DELTA_HEADER_SIZE + updates.len() * ENTITY_SIZE + removals.len() * 16,
    );

    buf.push(0x02);
    buf.extend_from_slice(&tick.to_le_bytes());
    buf.extend_from_slice(&(updates.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(removals.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0u8; 14]); // metrics placeholder

    for e in updates {
        encode_entity(&mut buf, e);
    }
    for id in removals {
        buf.extend_from_slice(id.0.as_bytes());
    }

    buf
}

pub async fn run_websocket_server(
    port: u16,
    world: Arc<RwLock<World>>,
    input_tx: mpsc::UnboundedSender<ClientMessage>,
) {
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind WebSocket port");

    tracing::info!("WebSocket server listening on port {port}");

    // Broadcast channel: one snapshot encoded per tick, all clients receive it
    let (broadcast_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(64);

    // Broadcast ticker: encodes world state as binary, sends to all clients
    let bcast_tx = broadcast_tx.clone();
    let bcast_world = Arc::clone(&world);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(TICK_DURATION_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let w = bcast_world.read().await;
            let frame = encode_binary_snapshot(w.tick, &w.get_client_snapshot());
            drop(w);

            let _ = bcast_tx.send(frame);
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

            // Send PlayerAssigned as JSON text frame
            let assign_msg = ServerMessage::PlayerAssigned {
                player_id: player_uuid.to_string(),
            };
            if let Ok(json) = serde_json::to_string(&assign_msg) {
                if ws_sink.send(Message::Text(json.into())).await.is_err() {
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
                    // Forward binary snapshot frames to this client
                    msg = broadcast_rx.recv() => {
                        match msg {
                            Ok(frame) => {
                                if ws_sink.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Client {addr} lagged {n} messages");
                            }
                            Err(_) => break,
                        }
                    }
                    // Receive input from this client (still JSON text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::{EntityState, ServerId};
    use glam::Vec3;

    #[test]
    fn encode_binary_snapshot_empty() {
        let buf = encode_binary_snapshot(42, &[]);
        assert_eq!(buf.len(), 27); // header only
        assert_eq!(buf[0], 0x01); // frame type
        assert_eq!(u64::from_le_bytes(buf[1..9].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 0);
    }

    #[test]
    fn encode_binary_snapshot_roundtrip() {
        let entities = vec![
            EntityState::new_npc(Vec3::new(1.0, 2.0, 3.0), ServerId(1)),
            EntityState::new_player(Vec3::new(10.0, 0.0, -5.0), ServerId(2)),
        ];
        let buf = encode_binary_snapshot(100, &entities);
        assert_eq!(buf.len(), 27 + 2 * 47); // header + 2 entities @ 47 bytes

        // Verify first entity
        let offset = 27;
        let uuid_bytes = &buf[offset..offset + 16];
        assert_eq!(uuid_bytes, entities[0].id.0.as_bytes());

        let px = f32::from_le_bytes(buf[offset + 16..offset + 20].try_into().unwrap());
        assert_eq!(px, 1.0);

        // Entity type: Npc = 0
        assert_eq!(buf[offset + 40], 0);

        // Authority: server 1
        let auth = u16::from_le_bytes(buf[offset + 41..offset + 43].try_into().unwrap());
        assert_eq!(auth, 1);

        // last_input_seq: 0
        let seq = u32::from_le_bytes(buf[offset + 43..offset + 47].try_into().unwrap());
        assert_eq!(seq, 0);

        // Second entity type: Player = 1
        let offset2 = 27 + 47;
        assert_eq!(buf[offset2 + 40], 1);
        let auth2 = u16::from_le_bytes(buf[offset2 + 41..offset2 + 43].try_into().unwrap());
        assert_eq!(auth2, 2);
    }

    #[test]
    fn encode_binary_delta_roundtrip() {
        let updates = vec![
            EntityState::new_npc(Vec3::new(5.0, 0.0, 5.0), ServerId(1)),
        ];
        let removals = vec![EntityId::new()];
        let buf = encode_binary_delta(50, &updates, &removals);
        // header(31) + 1 entity(47) + 1 removal UUID(16)
        assert_eq!(buf.len(), 31 + 47 + 16);
        assert_eq!(buf[0], 0x02);

        let tick = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(tick, 50);

        let update_count = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(update_count, 1);

        let removal_count = u32::from_le_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(removal_count, 1);

        // Verify removal UUID matches
        let removal_offset = 31 + 47;
        assert_eq!(&buf[removal_offset..removal_offset + 16], removals[0].0.as_bytes());
    }

    // -- last_input_seq encoding tests --

    #[test]
    fn encode_entity_size_is_47() {
        let e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(buf.len(), 47);
    }

    #[test]
    fn encode_entity_last_input_seq() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = 99;
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, 99);
    }

    #[test]
    fn encode_snapshot_preserves_last_input_seq() {
        let mut player = EntityState::new_player(Vec3::ZERO, ServerId(1));
        player.last_input_seq = 42;
        let buf = encode_binary_snapshot(1, &[player]);
        // Header 27 + entity starts at 27, seq at offset 43 within entity
        let seq = u32::from_le_bytes(buf[27 + 43..27 + 47].try_into().unwrap());
        assert_eq!(seq, 42);
    }

    #[test]
    fn encode_npc_has_zero_last_input_seq() {
        let npc = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &npc);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, 0);
    }

    // -- encoding edge cases / robustness --

    #[test]
    fn encode_entity_extreme_positions() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        e.position = Vec3::new(f32::MAX, f32::MIN, -0.0);
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let px = f32::from_le_bytes(buf[16..20].try_into().unwrap());
        let py = f32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(px, f32::MAX);
        assert_eq!(py, f32::MIN);
    }

    #[test]
    fn encode_entity_max_authority() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(u16::MAX));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let auth = u16::from_le_bytes(buf[41..43].try_into().unwrap());
        assert_eq!(auth, u16::MAX);
    }

    #[test]
    fn encode_entity_max_seq() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = u32::MAX;
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, u32::MAX);
    }

    #[test]
    fn encode_snapshot_100_entities_size() {
        let entities: Vec<EntityState> = (0..100)
            .map(|i| {
                let mut e = EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(1));
                e.last_input_seq = i;
                e
            })
            .collect();
        let buf = encode_binary_snapshot(999, &entities);
        assert_eq!(buf.len(), 27 + 100 * 47);
        // Verify entity count in header
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 100);
    }

    #[test]
    fn encode_snapshot_preserves_all_entity_uuids() {
        let entities: Vec<EntityState> = (0..10)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();
        let buf = encode_binary_snapshot(1, &entities);
        for (i, e) in entities.iter().enumerate() {
            let offset = 27 + i * 47;
            assert_eq!(&buf[offset..offset + 16], e.id.0.as_bytes());
        }
    }

    #[test]
    fn encode_delta_empty() {
        let buf = encode_binary_delta(1, &[], &[]);
        assert_eq!(buf.len(), 31); // header only
        assert_eq!(buf[0], 0x02);
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 0); // updates
        assert_eq!(u32::from_le_bytes(buf[13..17].try_into().unwrap()), 0); // removals
    }
}
