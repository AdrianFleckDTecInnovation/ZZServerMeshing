use crate::game_loop::{SharedInbound, SharedPeerHealth};
use crate::resources::{
    ForwardedInput, ForwardedRemove, InboundTransfer, InboundTransferAck, PeerSessionReset,
    PeerUpdate,
};
use shared::entity::{EntityState, ServerId};
use shared::protocol::PeerMessage;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::net::UdpSocket;

/// Chunk reassembly state for [`PeerMessage::ChunkedEntityUpdate`].
struct ChunkReassembly {
    total_chunks: u16,
    received: HashSet<u16>,
    entities: Vec<EntityState>,
}

/// How often (in loop iterations) to run the global reassembly cleanup sweep.
const GLOBAL_SWEEP_INTERVAL: u64 = 100;
/// Reassembly entries older than this many ticks behind the per-(server,session) latest are evicted.
const GLOBAL_STALE_TICKS: u64 = 40;
/// Maximum number of reassembly entries before forced eviction.
/// Prevents unbounded memory growth from crashed peers that never complete chunks.
const MAX_REASSEMBLY_ENTRIES: usize = 1000;

/// Observe the `(server_id, session_id)` pair on an incoming PeerMessage.
///
/// If this is the first packet from `server_id` or the session matches the
/// stored one, the call is a cheap insert/no-op. If the incoming `session_id`
/// differs from the stored session for `server_id`, that indicates a peer
/// restart: we drop all reassembly entries belonging to the old session,
/// remove the old `(server_id, old_session_id)` entry from `latest_tick`,
/// clear the per-server `peer_health` counters, and push a [`PeerSessionReset`]
/// event onto the inbound queue so downstream ECS systems can react
/// (WI-135 consumes this to reset shadow `last_peer_tick`; design 015).
///
/// Note: we always trust the latest-arriving `session_id` as the new current
/// session. Out-of-order stragglers from an older session arriving after a
/// new-session packet will trigger a reset back to the old session; the next
/// new-session packet (typically within one tick) will correct this. This is
/// acceptable because we have no authoritative "newer" ordering for random
/// session nonces.
fn note_session(
    server_id: u16,
    session_id: u64,
    session_of_peer: &mut HashMap<u16, u64>,
    reassembly: &mut HashMap<(u16, u64, u64), ChunkReassembly>,
    latest_tick: &mut HashMap<(u16, u64), u64>,
    peer_health: &Arc<SharedPeerHealth>,
    inbound: &Arc<SharedInbound>,
) {
    match session_of_peer.get(&server_id).copied() {
        Some(prev) if prev != session_id => {
            tracing::info!(
                server_id,
                old_session_id = format_args!("0x{:016x}", prev),
                new_session_id = format_args!("0x{:016x}", session_id),
                "peer session changed — resetting receiver state"
            );
            // Drop reassembly entries for the old session.
            reassembly.retain(|&(sid, ssid, _), _| !(sid == server_id && ssid == prev));
            // Remove the old (server_id, old_session_id) latest_tick entry.
            latest_tick.remove(&(server_id, prev));
            // Clear this server's peer_health counters and record the new
            // session so /metrics reflects the restart immediately (WI-136).
            {
                let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                ph.latest_tick.remove(&server_id);
                ph.packets_received.remove(&server_id);
                ph.chunk_evictions.remove(&server_id);
                ph.peer_sessions.insert(server_id, session_id);
            }
            // Push a PeerSessionReset event onto the inbound queue for the ECS
            // to consume (WI-135 will reset shadow last_peer_tick on receipt;
            // design 015).
            {
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                q.peer_session_resets.push(PeerSessionReset {
                    server_id: ServerId(server_id),
                    new_session_id: session_id,
                });
            }
            session_of_peer.insert(server_id, session_id);
        }
        Some(_) => {
            // Same session as before — nothing to do. peer_sessions was
            // already populated by the first-sight path below.
        }
        None => {
            // First time we've seen this server — record the session in both
            // the receiver-local map and the shared peer_health snapshot so
            // /metrics exposes the current session id (WI-136).
            // Do NOT emit a reset event: there is no prior state to clear.
            session_of_peer.insert(server_id, session_id);
            let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
            ph.peer_sessions.insert(server_id, session_id);
        }
    }
}

/// Listen for incoming UDP packets from peer servers and push events to the shared inbound queue.
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    inbound: Arc<SharedInbound>,
    peer_health: Arc<SharedPeerHealth>,
) {
    let mut buf = vec![0u8; 65536];

    // Reassembly keyed by (server_id, session_id, tick). Per-session keying
    // (WI-133) prevents old-session entries from colliding with or evicting
    // new-session entries after a peer restart.
    let mut reassembly: HashMap<(u16, u64, u64), ChunkReassembly> = HashMap::new();
    // Latest observed tick per (server_id, session_id). Keyed by session_id so
    // a peer restart to a lower tick no longer starves the reassembly sweep.
    let mut latest_tick: HashMap<(u16, u64), u64> = HashMap::new();
    // The currently-known session_id for each server_id. Used to detect
    // session changes (peer restart) — see `note_session`.
    let mut session_of_peer: HashMap<u16, u64> = HashMap::new();

    let mut loop_counter: u64 = 0;

    loop {
        loop_counter += 1;

        // Periodic sweep: evict stale reassembly entries using per-(server,session) latest tick.
        // Uses per-session tick instead of global max to avoid evicting entries from
        // legitimately slow peers (e.g., newly joined server at tick 0) or a peer
        // that just restarted and is replicating from a lower tick.
        if loop_counter % GLOBAL_SWEEP_INTERVAL == 0 {
            // Collect evictions per peer before the retain call (WI-103)
            let mut sweep_evictions: HashMap<u16, u32> = HashMap::new();
            for (&(sid, ssid, t), _) in reassembly.iter() {
                if let Some(&server_latest) = latest_tick.get(&(sid, ssid)) {
                    if server_latest.saturating_sub(t) > GLOBAL_STALE_TICKS {
                        *sweep_evictions.entry(sid).or_insert(0) += 1;
                    }
                }
            }
            let before = reassembly.len();
            reassembly.retain(|&(sid, ssid, t), _| {
                if let Some(&server_latest) = latest_tick.get(&(sid, ssid)) {
                    server_latest.saturating_sub(t) <= GLOBAL_STALE_TICKS
                } else {
                    true // no tick info for this (server, session) yet, keep it
                }
            });
            let evicted = before - reassembly.len();
            if evicted > 0 {
                tracing::debug!(
                    evicted,
                    remaining = reassembly.len(),
                    "Per-session reassembly sweep evicted stale entries"
                );
                // Record evictions per peer (WI-103)
                let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                for (sid, count) in sweep_evictions {
                    *ph.chunk_evictions.entry(sid).or_insert(0) += count;
                }
            }
        }

        // Hard cap: if reassembly map exceeds limit, evict oldest entries first.
        // Sort by the tick component of the composite key so the oldest-tick
        // (most likely to be stale) entries are evicted first.
        if reassembly.len() > MAX_REASSEMBLY_ENTRIES {
            let mut entries: Vec<(u16, u64, u64)> = reassembly.keys().copied().collect();
            entries.sort_by_key(|&(_sid, _ssid, tick)| tick);
            let to_evict = reassembly.len() - MAX_REASSEMBLY_ENTRIES;
            // Count evictions per peer (WI-103)
            let mut cap_evictions: HashMap<u16, u32> = HashMap::new();
            for &(sid, _, _) in entries.iter().take(to_evict) {
                *cap_evictions.entry(sid).or_insert(0) += 1;
            }
            for key in entries.into_iter().take(to_evict) {
                reassembly.remove(&key);
            }
            tracing::warn!(
                evicted = to_evict,
                remaining = reassembly.len(),
                "Reassembly map exceeded {MAX_REASSEMBLY_ENTRIES} entries, evicted oldest"
            );
            if !cap_evictions.is_empty() {
                let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                for (sid, count) in cap_evictions {
                    *ph.chunk_evictions.entry(sid).or_insert(0) += count;
                }
            }
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
                session_id,
                entities,
            } => {
                note_session(
                    server_id.0,
                    session_id,
                    &mut session_of_peer,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                );
                // Per-peer health tracking (WI-103)
                {
                    let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                    *ph.packets_received.entry(server_id.0).or_insert(0) += 1;
                    let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
                    if tick > *lt {
                        *lt = tick;
                    }
                }
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                q.peer_updates.push(PeerUpdate { tick, entities });
            }
            PeerMessage::AuthorityTransfer {
                tick: _,
                session_id,
                entity_id,
                from_server,
                to_server,
                full_state,
            } => {
                note_session(
                    from_server.0,
                    session_id,
                    &mut session_of_peer,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                );
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                q.authority_transfers.push(InboundTransfer {
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
                session_id: _,
                entity_id,
                direction,
                seq,
            } => {
                // ForwardedInput doesn't carry server_id, so we can't key a
                // session change to a specific peer. The receiver simply
                // consumes these; session reset happens via any peer message
                // that does carry server_id (EntityStateUpdate, chunk, etc.).
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                q.forwarded_inputs.push(ForwardedInput {
                    entity_id,
                    direction,
                    seq,
                });
            }
            PeerMessage::ForwardedRemove {
                session_id: _,
                entity_id,
            } => {
                // Same rationale as ForwardedInput: no server_id in the payload.
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                q.forwarded_removes.push(ForwardedRemove { entity_id });
                tracing::info!(
                    entity = %entity_id,
                    "Queued forwarded remove"
                );
            }
            PeerMessage::TransferAck {
                session_id: _,
                entity_id,
                tick,
            } => {
                // Same rationale: no server_id in the payload.
                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
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
                session_id,
                chunk_index,
                total_chunks,
                entities,
            } => {
                note_session(
                    server_id.0,
                    session_id,
                    &mut session_of_peer,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                );

                let key = (server_id.0, session_id, tick);

                // Per-peer health tracking (WI-103)
                {
                    let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                    *ph.packets_received.entry(server_id.0).or_insert(0) += 1;
                    let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
                    if tick > *lt {
                        *lt = tick;
                    }
                }

                // Update latest tick for this (server, session)
                let prev_latest = latest_tick.entry((server_id.0, session_id)).or_insert(0);
                if tick > *prev_latest {
                    // Count inline evictions before the retain (WI-103)
                    let before_inline = reassembly.len();
                    *prev_latest = tick;
                    // Clean up stale reassemblies for this (server, session)
                    // more than 2 ticks behind the freshly-advanced tick.
                    reassembly.retain(|&(sid, ssid, t), _| {
                        !(sid == server_id.0 && ssid == session_id) || tick - t <= 2
                    });
                    let inline_evicted = before_inline - reassembly.len();
                    if inline_evicted > 0 {
                        let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                        *ph.chunk_evictions.entry(server_id.0).or_insert(0) +=
                            inline_evicted as u32;
                    }
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
                    let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                    q.peer_updates.push(PeerUpdate {
                        tick,
                        entities: complete.entities,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for receiver state management.
    //!
    //! These tests exercise `note_session` directly and simulate the key
    //! reassembly / latest_tick bookkeeping the receive loop does. They avoid
    //! spinning up a real UDP socket.

    use super::*;
    use crate::game_loop::PeerHealthAccumulator;
    use crate::resources::InboundQueue;
    use std::sync::Mutex;

    type SessionOfPeer = HashMap<u16, u64>;
    type ReassemblyMap = HashMap<(u16, u64, u64), ChunkReassembly>;
    type LatestTickMap = HashMap<(u16, u64), u64>;
    type TestState = (
        SessionOfPeer,
        ReassemblyMap,
        LatestTickMap,
        Arc<SharedPeerHealth>,
        Arc<SharedInbound>,
    );

    fn test_state() -> TestState {
        let session_of_peer: HashMap<u16, u64> = HashMap::new();
        let reassembly: HashMap<(u16, u64, u64), ChunkReassembly> = HashMap::new();
        let latest_tick: HashMap<(u16, u64), u64> = HashMap::new();
        let peer_health: Arc<SharedPeerHealth> =
            Arc::new(Mutex::new(PeerHealthAccumulator::default()));
        let inbound: Arc<SharedInbound> = Arc::new(Mutex::new(InboundQueue::default()));
        (
            session_of_peer,
            reassembly,
            latest_tick,
            peer_health,
            inbound,
        )
    }

    /// Helper: insert a partial reassembly entry directly.
    fn insert_partial(
        reassembly: &mut HashMap<(u16, u64, u64), ChunkReassembly>,
        key: (u16, u64, u64),
        total_chunks: u16,
    ) {
        reassembly.insert(
            key,
            ChunkReassembly {
                total_chunks,
                received: HashSet::new(),
                entities: Vec::new(),
            },
        );
    }

    #[test]
    fn reassembly_completes_normally_for_single_session_across_ticks() {
        // Simulates the happy path: one peer, stable session_id, chunks arrive
        // for multiple ticks and each tick reassembles independently under the
        // new composite key.
        let (mut session_of_peer, mut reassembly, mut latest_tick, peer_health, inbound) =
            test_state();
        let server_id = 2u16;
        let session_a = 0xAAAA_AAAA_AAAA_AAAA_u64;

        // Tick 100: two chunks
        for tick in [100u64, 101] {
            for chunk_index in [0u16, 1u16] {
                note_session(
                    server_id,
                    session_a,
                    &mut session_of_peer,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                );
                let key = (server_id, session_a, tick);
                let prev_latest = latest_tick.entry((server_id, session_a)).or_insert(0);
                if tick > *prev_latest {
                    *prev_latest = tick;
                }
                let entry = reassembly.entry(key).or_insert_with(|| ChunkReassembly {
                    total_chunks: 2,
                    received: HashSet::new(),
                    entities: Vec::new(),
                });
                entry.received.insert(chunk_index);
                if entry.received.len() == entry.total_chunks as usize {
                    reassembly.remove(&key);
                }
            }
        }

        // After both ticks' chunks delivered, reassembly should be empty
        // (both ticks completed) and latest_tick should be 101.
        assert!(
            reassembly.is_empty(),
            "expected both ticks to reassemble cleanly, got {:?}",
            reassembly.keys().collect::<Vec<_>>()
        );
        assert_eq!(latest_tick.get(&(server_id, session_a)), Some(&101));
        assert_eq!(session_of_peer.get(&server_id), Some(&session_a));
        // No session reset should have been emitted (single session throughout).
        let q = inbound.lock().unwrap();
        assert!(q.peer_session_resets.is_empty());
    }

    #[test]
    fn old_session_reassembly_entries_are_dropped_on_new_session_packet() {
        // Seed partial reassembly entries under session A, then receive a
        // session B packet. note_session must drop the A entries.
        let (mut session_of_peer, mut reassembly, mut latest_tick, peer_health, inbound) =
            test_state();
        let server_id = 2u16;
        let session_a = 0xAAAA_AAAA_AAAA_AAAA_u64;
        let session_b = 0xBBBB_BBBB_BBBB_BBBB_u64;

        // Prime: server 2 already known under session A with a partial reassembly
        // for tick 100 and 101.
        session_of_peer.insert(server_id, session_a);
        insert_partial(&mut reassembly, (server_id, session_a, 100), 2);
        insert_partial(&mut reassembly, (server_id, session_a, 101), 2);
        latest_tick.insert((server_id, session_a), 101);

        // Also seed a different server (3) under its own session — should NOT be touched.
        session_of_peer.insert(3, 0xCCCC_CCCC_CCCC_CCCC);
        insert_partial(&mut reassembly, (3, 0xCCCC_CCCC_CCCC_CCCC, 50), 2);
        latest_tick.insert((3, 0xCCCC_CCCC_CCCC_CCCC), 50);

        // Now a new-session packet arrives for server 2.
        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );

        // All (server_id, session_a, *) entries must be gone.
        assert!(
            !reassembly
                .keys()
                .any(|&(sid, ssid, _)| sid == server_id && ssid == session_a),
            "expected no residual session_a entries for server {server_id}, got {:?}",
            reassembly.keys().collect::<Vec<_>>()
        );
        // Server 3's entry is untouched.
        assert!(reassembly.contains_key(&(3, 0xCCCC_CCCC_CCCC_CCCC, 50)));
        // Old latest_tick entry for (server_id, session_a) is gone.
        assert!(!latest_tick.contains_key(&(server_id, session_a)));
        // Server 3's latest_tick is untouched.
        assert_eq!(latest_tick.get(&(3, 0xCCCC_CCCC_CCCC_CCCC)), Some(&50));
        // session_of_peer now reflects session_b.
        assert_eq!(session_of_peer.get(&server_id), Some(&session_b));
    }

    #[test]
    fn peer_session_reset_event_pushed_exactly_once_per_detected_change() {
        // After a detected session change, subsequent packets on the new
        // session must NOT push additional reset events.
        let (mut session_of_peer, mut reassembly, mut latest_tick, peer_health, inbound) =
            test_state();
        let server_id = 2u16;
        let session_a = 0x1111_1111_1111_1111_u64;
        let session_b = 0x2222_2222_2222_2222_u64;

        // First packet ever from this peer: session A. Should NOT emit a reset.
        note_session(
            server_id,
            session_a,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );
        assert!(
            inbound.lock().unwrap().peer_session_resets.is_empty(),
            "first packet from a brand-new server_id must not emit a reset"
        );

        // Second packet on the same session: still no reset.
        note_session(
            server_id,
            session_a,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );
        assert!(inbound.lock().unwrap().peer_session_resets.is_empty());

        // Session change A -> B: reset event emitted.
        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );
        {
            let q = inbound.lock().unwrap();
            assert_eq!(
                q.peer_session_resets.len(),
                1,
                "expected exactly one reset event after session change"
            );
            let ev = &q.peer_session_resets[0];
            assert_eq!(ev.server_id, ServerId(server_id));
            assert_eq!(ev.new_session_id, session_b);
        }

        // Subsequent packets on session B should NOT push more resets.
        for _ in 0..5 {
            note_session(
                server_id,
                session_b,
                &mut session_of_peer,
                &mut reassembly,
                &mut latest_tick,
                &peer_health,
                &inbound,
            );
        }
        let q = inbound.lock().unwrap();
        assert_eq!(
            q.peer_session_resets.len(),
            1,
            "subsequent same-session packets must not add more reset events"
        );
    }

    #[test]
    fn first_packet_from_new_server_id_does_not_emit_reset() {
        // Distinct from the previous test: isolate the "unknown peer" case.
        let (mut session_of_peer, mut reassembly, mut latest_tick, peer_health, inbound) =
            test_state();

        // Arbitrary session_id on first sight of server 7.
        note_session(
            7,
            0xDEAD_BEEF_CAFE_BABE,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );
        assert_eq!(session_of_peer.get(&7), Some(&0xDEAD_BEEF_CAFE_BABE));
        assert!(
            inbound.lock().unwrap().peer_session_resets.is_empty(),
            "no reset should be emitted on the very first packet from a new peer"
        );
        // No stale reassembly should be created, no latest_tick entry should be added.
        assert!(reassembly.is_empty());
        assert!(latest_tick.is_empty());
    }

    #[test]
    fn session_change_clears_peer_health_counters_for_that_server() {
        // Seed peer_health counters for server 2 under session A, then
        // simulate a session change and assert the counters were cleared.
        let (mut session_of_peer, mut reassembly, mut latest_tick, peer_health, inbound) =
            test_state();
        let server_id = 2u16;
        let session_a = 0xAA_u64;
        let session_b = 0xBB_u64;

        session_of_peer.insert(server_id, session_a);
        {
            let mut ph = peer_health.lock().unwrap();
            ph.latest_tick.insert(server_id, 14000);
            ph.packets_received.insert(server_id, 42);
            ph.chunk_evictions.insert(server_id, 7);
            // Also seed server 3 to ensure isolation.
            ph.latest_tick.insert(3, 50);
            ph.packets_received.insert(3, 3);
        }

        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
        );

        let ph = peer_health.lock().unwrap();
        assert_eq!(ph.latest_tick.get(&server_id), None);
        assert_eq!(ph.packets_received.get(&server_id), None);
        assert_eq!(ph.chunk_evictions.get(&server_id), None);
        // Server 3's counters are untouched.
        assert_eq!(ph.latest_tick.get(&3), Some(&50));
        assert_eq!(ph.packets_received.get(&3), Some(&3));
    }

    #[test]
    fn latest_tick_keyed_per_session_allows_independent_tracking() {
        // If a peer sends packets under two different sessions (unusual but
        // possible under rapid restart + out-of-order delivery), the latest_tick
        // map must track each session independently under the composite key.
        let mut latest_tick: HashMap<(u16, u64), u64> = HashMap::new();
        let server_id = 2u16;
        let session_a = 0xAA_u64;
        let session_b = 0xBB_u64;

        latest_tick.insert((server_id, session_a), 100);
        latest_tick.insert((server_id, session_b), 50);

        assert_eq!(latest_tick.get(&(server_id, session_a)), Some(&100));
        assert_eq!(latest_tick.get(&(server_id, session_b)), Some(&50));
    }
}
