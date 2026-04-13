use crate::game_loop::SharedInbound;
use crate::resources::{ForwardedInput, ForwardedRemove, InboundTransfer, PeerUpdate};
use shared::protocol::PeerMessage;
use std::sync::Arc;
use tokio::net::UdpSocket;

/// Listen for incoming UDP packets from peer servers and push events to the shared inbound queue.
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    inbound: Arc<SharedInbound>,
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
                server_id,
                entities,
            } => {
                let mut q = inbound.queue.lock().await;
                q.peer_updates.push(PeerUpdate {
                    tick,
                    server_id,
                    entities,
                });
            }
            PeerMessage::AuthorityTransfer {
                tick,
                entity_id,
                from_server,
                to_server,
                full_state,
            } => {
                let mut q = inbound.queue.lock().await;
                q.authority_transfers.push(InboundTransfer {
                    tick,
                    entity_id,
                    from_server,
                    to_server,
                    full_state,
                });
                tracing::info!(
                    entity = %entity_id,
                    from = from_server.0,
                    to = to_server.0,
                    "Queued authority transfer"
                );
            }
            PeerMessage::ForwardedInput {
                entity_id,
                direction,
                seq,
            } => {
                let mut q = inbound.queue.lock().await;
                q.forwarded_inputs.push(ForwardedInput {
                    entity_id,
                    direction,
                    seq,
                });
            }
            PeerMessage::ForwardedRemove { entity_id } => {
                let mut q = inbound.queue.lock().await;
                q.forwarded_removes.push(ForwardedRemove { entity_id });
                tracing::info!(
                    entity = %entity_id,
                    "Queued forwarded remove"
                );
            }
        }
    }
}
