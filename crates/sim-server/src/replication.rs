use crate::world::World;
use shared::protocol::PeerMessage;
use tokio::net::UdpSocket;

/// After each tick, send authoritative entity states to all peer servers via UDP.
pub async fn replicate_to_peers(
    world: &World,
    udp_socket: &UdpSocket,
) {
    if world.peer_addrs.is_empty() || world.entities.is_empty() {
        return;
    }

    let msg = PeerMessage::EntityStateUpdate {
        tick: world.tick,
        server_id: world.server_id,
        entities: world.entities.values().cloned().collect(),
    };

    let bytes = match bincode::serialize(&msg) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Failed to serialize replication message: {e}");
            return;
        }
    };

    // If message is too large for a single UDP packet, chunk it
    // For now, just send it — bincode is compact enough for hundreds of entities
    if bytes.len() > 60_000 {
        tracing::warn!(
            "Replication packet too large ({} bytes), {} entities — chunking needed",
            bytes.len(),
            world.entities.len()
        );
        // TODO: implement chunking for very large entity counts
        return;
    }

    for (_, peer_addr) in &world.peer_addrs {
        if let Err(e) = udp_socket.send_to(&bytes, peer_addr).await {
            tracing::warn!("Failed to send to peer {peer_addr}: {e}");
        }
    }
}
