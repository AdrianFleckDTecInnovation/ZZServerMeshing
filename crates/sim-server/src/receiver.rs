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
                entity_id,
                from_server,
                to_server: _,
                full_state,
            } => {
                let mut w = world.write().await;
                // Accept: add to our authoritative entities, remove from shadows
                w.shadow_entities.remove(&entity_id);
                w.shadow_last_seen.remove(&entity_id);
                w.entities.insert(entity_id, full_state);
                tracing::info!(
                    "Accepted entity {} from S{}",
                    entity_id,
                    from_server
                );
            }
        }
    }
}
