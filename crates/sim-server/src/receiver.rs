use crate::world::World;
use shared::protocol::PeerMessage;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

/// Listen for incoming UDP packets from peer servers and update shadow entities.
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    world: Arc<RwLock<World>>,
) {
    let mut buf = vec![0u8; 65536];

    loop {
        let (len, addr) = match udp_socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("UDP recv error: {e}");
                continue;
            }
        };

        let msg: PeerMessage = match bincode::deserialize(&buf[..len]) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to deserialize UDP from {addr}: {e}");
                continue;
            }
        };

        match msg {
            PeerMessage::EntityStateUpdate {
                tick,
                server_id: _,
                entities,
            } => {
                let mut w = world.write().await;
                w.apply_peer_update(tick, entities);
            }
            PeerMessage::AuthorityTransfer {
                tick: _,
                entity_id,
                from_server,
                to_server,
                full_state,
            } => {
                let mut w = world.write().await;
                // Validate: only accept if targeted at us
                if to_server != w.server_id {
                    tracing::warn!(
                        "Ignoring transfer for {} targeted at {}, we are {}",
                        entity_id, to_server, w.server_id
                    );
                    continue;
                }
                // Reject if already authoritative (duplicate transfer)
                if w.entities.contains_key(&entity_id) {
                    tracing::warn!(
                        "Ignoring duplicate transfer for {} from {}",
                        entity_id, from_server
                    );
                    continue;
                }
                // Accept: promote from shadow to authoritative
                w.shadow_entities.remove(&entity_id);
                w.shadow_last_seen.remove(&entity_id);
                w.entities.insert(entity_id, full_state);
                tracing::info!(
                    entity = %entity_id,
                    from = from_server.0,
                    to = to_server.0,
                    "Accepted authority transfer"
                );
            }
            PeerMessage::ForwardedInput {
                entity_id,
                direction,
                seq,
            } => {
                let mut w = world.write().await;
                if w.entities.contains_key(&entity_id) {
                    let dir = glam::Vec3::new(direction[0], direction[1], direction[2]);
                    w.apply_move_input(&entity_id, dir);
                    if let Some(entity) = w.entities.get_mut(&entity_id) {
                        entity.last_input_seq = seq;
                    }
                    tracing::debug!(
                        entity = %entity_id,
                        seq = seq,
                        "Applied forwarded input"
                    );
                } else {
                    tracing::debug!(
                        "Received forwarded input for {} but not authoritative",
                        entity_id
                    );
                }
            }
        }
    }
}
