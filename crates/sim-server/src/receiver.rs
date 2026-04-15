use crate::game_loop::SharedInbound;
use crate::resources::{
    ForwardedInput, ForwardedRemove, InboundTransferAck, InboundTransfer, PeerUpdate,
};
use shared::entity::EntityState;
use shared::protocol::PeerMessage;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::net::UdpSocket;

/// Listen for incoming UDP packets from peer servers and push events to the shared inbound queue.
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    inbound: Arc<SharedInbound>,
) {
    let mut buf = vec![0u8; 65536];

    // Chunk reassembly state for ChunkedEntityUpdate
    struct ChunkReassembly {
        total_chunks: u16,
        received: HashSet<u16>,
        entities: Vec<EntityState>,
    }
    let mut reassembly: HashMap<(u16, u64), ChunkReassembly> = HashMap::new();
    let mut latest_tick: HashMap<u16, u64> = HashMap::new();

    /// How often (in loop iterations) to run the global reassembly cleanup sweep.
    const GLOBAL_SWEEP_INTERVAL: u64 = 100;
    /// Reassembly entries older than this many ticks behind the per-server latest are evicted.
    const GLOBAL_STALE_TICKS: u64 = 40;
    /// Maximum number of reassembly entries before forced eviction.
    /// Prevents unbounded memory growth from crashed peers that never complete chunks.
    const MAX_REASSEMBLY_ENTRIES: usize = 1000;

    let mut loop_counter: u64 = 0;

    loop {
        loop_counter += 1;

        // Periodic sweep: evict stale reassembly entries using per-server latest tick.
        // Uses per-server tick instead of global max to avoid evicting entries from
        // legitimately slow peers (e.g., newly joined server at tick 0).
        if loop_counter % GLOBAL_SWEEP_INTERVAL == 0 {
            let before = reassembly.len();
            reassembly.retain(|&(sid, t), _| {
                if let Some(&server_latest) = latest_tick.get(&sid) {
                    server_latest.saturating_sub(t) <= GLOBAL_STALE_TICKS
                } else {
                    true // no tick info for this server yet, keep it
                }
            });
            let evicted = before - reassembly.len();
            if evicted > 0 {
                tracing::debug!(
                    evicted,
                    remaining = reassembly.len(),
                    "Per-server reassembly sweep evicted stale entries"
                );
            }
        }

        // Hard cap: if reassembly map exceeds limit, evict oldest entries first.
        if reassembly.len() > MAX_REASSEMBLY_ENTRIES {
            let mut entries: Vec<(u16, u64)> = reassembly.keys().copied().collect();
            entries.sort_by_key(|&(_sid, tick)| tick);
            let to_evict = reassembly.len() - MAX_REASSEMBLY_ENTRIES;
            for key in entries.into_iter().take(to_evict) {
                reassembly.remove(&key);
            }
            tracing::warn!(
                evicted = to_evict,
                remaining = reassembly.len(),
                "Reassembly map exceeded {MAX_REASSEMBLY_ENTRIES} entries, evicted oldest"
            );
        }
        let (len, addr) = match udp_socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("UDP recv error: {e}");
                continue;
            }
        };

        // Check protocol version prefix (first 2 bytes)
        if len < 2 {
            tracing::warn!("UDP packet too short from {addr}: {len} bytes");
            continue;
        }
        let wire_version = u16::from_le_bytes([buf[0], buf[1]]);
        if wire_version != shared::PROTOCOL_VERSION {
            tracing::warn!(
                wire_version,
                expected = shared::PROTOCOL_VERSION,
                "UDP protocol version mismatch from {addr}, dropping packet"
            );
            continue;
        }

        let msg: PeerMessage = match bincode::deserialize(&buf[2..len]) {
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
                let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
                q.forwarded_inputs.push(ForwardedInput {
                    entity_id,
                    direction,
                    seq,
                });
            }
            PeerMessage::ForwardedRemove { entity_id } => {
                let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
                q.forwarded_removes.push(ForwardedRemove { entity_id });
                tracing::info!(
                    entity = %entity_id,
                    "Queued forwarded remove"
                );
            }
            PeerMessage::TransferAck { entity_id, tick } => {
                let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
                q.transfer_acks.push(InboundTransferAck { entity_id, tick });
                tracing::debug!(
                    entity = %entity_id,
                    tick,
                    "Queued transfer ACK"
                );
            }
            PeerMessage::ChunkedEntityUpdate {
                tick,
                server_id,
                chunk_index,
                total_chunks,
                entities,
            } => {
                let key = (server_id.0, tick);

                // Update latest tick for this server
                let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
                if tick > *prev_latest {
                    *prev_latest = tick;
                    // Clean up stale reassemblies (more than 2 ticks behind)
                    reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
                }

                let entry = reassembly.entry(key).or_insert_with(|| ChunkReassembly {
                    total_chunks,
                    received: HashSet::new(),
                    entities: Vec::new(),
                });

                if entry.received.contains(&chunk_index) {
                    continue; // duplicate chunk
                }

                entry.received.insert(chunk_index);
                entry.entities.extend(entities);

                if entry.received.len() == entry.total_chunks as usize {
                    // Complete! Push to inbound queue
                    let complete = reassembly.remove(&key).unwrap();
                    let mut q = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
                    q.peer_updates.push(PeerUpdate {
                        tick,
                        server_id,
                        entities: complete.entities,
                    });
                }
            }
        }
    }
}
