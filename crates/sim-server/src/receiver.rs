use crate::game_loop::{SharedInbound, SharedPeerHealth};
use crate::resources::{
    ForwardedInput, ForwardedRemove, InboundTransfer, InboundTransferAck, PeerSessionReset,
    PeerUpdate,
};
use shared::entity::{EntityState, ServerId};
use shared::protocol::PeerMessage;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
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

/// WI-211: Capacity of the per-peer session history LRU. A straggler packet
/// from any of the last `SESSION_HISTORY_CAPACITY` fully-superseded sessions
/// is silently dropped instead of triggering a spurious reset-and-flip loop
/// under WiFi reorder during a rolling deploy. Four entries is enough to
/// cover N in-flight cross-version rolling-deploy windows; the per-peer map
/// cost is tiny.
const SESSION_HISTORY_CAPACITY: usize = 4;

/// Observe the `(server_id, session_id)` pair on an incoming PeerMessage.
///
/// Return value:
/// - `true`: proceed with packet processing.
/// - `false`: this packet is a straggler from a fully-superseded session
///   (WI-211). The caller MUST skip processing (no peer_updates push, no
///   ACK, etc.). A straggler counter in `peer_health.stragglers_dropped`
///   is incremented for observability.
///
/// If this is the first packet from `server_id` or the session matches the
/// stored one, the call is a cheap insert/no-op and returns `true`. If the
/// incoming `session_id` differs from the stored session for `server_id`,
/// that indicates a peer restart: we drop all reassembly entries belonging
/// to the old session, remove the old `(server_id, old_session_id)` entry
/// from `latest_tick`, record the session change time in
/// `peer_health.session_changed_at_tick` (WI-212), and push a
/// [`PeerSessionReset`] event onto the inbound queue so downstream ECS
/// systems can react (WI-135 consumes this to reset shadow `last_peer_tick`;
/// design 015).
///
/// WI-212 change: do NOT clear `packets_received`, `chunk_evictions`, or
/// `latest_tick` in `peer_health` on session change. Operators need those
/// counters to reflect a continuous series across a restart window rather
/// than zeroing out. The `session_changed_at_tick` map provides the "when
/// did this peer restart" signal to /metrics consumers.
///
/// WI-211 change: maintain a per-peer LRU of recently-seen sessions in
/// `session_history`. If the incoming session_id is already in the history
/// for this peer, it is a straggler from a superseded session — drop it
/// without flipping `session_of_peer`. This prevents session thrash under
/// WiFi reorder during a rolling deploy.
#[allow(clippy::too_many_arguments)]
fn note_session(
    server_id: u16,
    session_id: u64,
    session_of_peer: &mut HashMap<u16, u64>,
    session_history: &mut HashMap<u16, VecDeque<u64>>,
    reassembly: &mut HashMap<(u16, u64, u64), ChunkReassembly>,
    latest_tick: &mut HashMap<(u16, u64), u64>,
    peer_health: &Arc<SharedPeerHealth>,
    inbound: &Arc<SharedInbound>,
    local_tick: &AtomicU64,
) -> bool {
    match session_of_peer.get(&server_id).copied() {
        Some(prev) if prev != session_id => {
            // WI-211: check if the incoming session is already in this peer's
            // history — that means a superseded session sent a straggler UDP
            // packet after a restart boundary. Dropping the packet here avoids
            // a spurious reset-and-flip loop that destroys freshly-established
            // shadow state.
            if let Some(history) = session_history.get(&server_id) {
                if history.contains(&session_id) {
                    // Straggler detected. Increment counter, log at debug
                    // (very verbose under thrash), return false so callers
                    // skip downstream processing.
                    {
                        let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                        *ph.stragglers_dropped.entry(server_id).or_insert(0) += 1;
                    }
                    tracing::debug!(
                        server_id,
                        stale_session_id = format_args!("0x{:016x}", session_id),
                        current_session_id = format_args!("0x{:016x}", prev),
                        "dropping straggler packet from superseded session"
                    );
                    return false;
                }
            }

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
            // WI-212: preserve `packets_received`, `chunk_evictions`,
            // `stragglers_dropped` across session changes — those are
            // activity counters that operators want continuous across a
            // restart window. `latest_tick` IS reset to 0 because it
            // specifically tracks "highest tick seen from this peer" and
            // a restarted peer begins at tick 0; preserving the old
            // high-water mark would make `staleness_ticks` (= our tick -
            // latest_tick) perpetually report "700 ticks behind" after a
            // restart, confusing the operator-facing rate.
            //
            // `session_changed_at_tick` provides the "when did restart
            // happen" signal separately so operators can still see the
            // boundary.
            {
                let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                ph.peer_sessions.insert(server_id, session_id);
                ph.session_changed_at_tick
                    .insert(server_id, local_tick.load(Ordering::Relaxed));
                ph.latest_tick.remove(&server_id);
            }
            // WI-211: push the OUTGOING (superseded) session onto the
            // per-peer history ring. Trim to capacity from the back.
            {
                let history = session_history.entry(server_id).or_default();
                history.push_front(prev);
                while history.len() > SESSION_HISTORY_CAPACITY {
                    history.pop_back();
                }
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
            true
        }
        Some(_) => {
            // Same session as before — nothing to do. peer_sessions was
            // already populated by the first-sight path below.
            true
        }
        None => {
            // First time we've seen this server — record the session in both
            // the receiver-local map and the shared peer_health snapshot so
            // /metrics exposes the current session id (WI-136).
            // Do NOT emit a reset event: there is no prior state to clear.
            session_of_peer.insert(server_id, session_id);
            let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
            ph.peer_sessions.insert(server_id, session_id);
            true
        }
    }
}

/// Listen for incoming UDP packets from peer servers and push events to the shared inbound queue.
///
/// WI-212: `local_tick` is a shared atomic that the game loop writes on every
/// tick. The receiver reads it on session change to record "at what local tick
/// did this peer restart" for `/metrics` continuity.
///
/// WI-213: `active_peers` is a shared set populated by the game loop on every
/// `PeerList` update. The receiver reads it during its periodic sweep and
/// prunes entries from `session_of_peer`, `session_history`, and `latest_tick`
/// whose server_id has departed the mesh. Empty before the first PeerList;
/// prune is gated on `active_peers.len() > 0` so a fresh sim-server doesn't
/// wipe its receiver state on the first sweep.
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    inbound: Arc<SharedInbound>,
    peer_health: Arc<SharedPeerHealth>,
    local_tick: Arc<AtomicU64>,
    active_peers: Arc<RwLock<HashSet<u16>>>,
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
    // WI-211: per-peer LRU of recently-superseded session_ids. Packets
    // arriving with a session_id in this history are silently dropped
    // (they are stragglers after a restart boundary) rather than
    // triggering a spurious reset-and-flip cycle under WiFi reorder.
    let mut session_history: HashMap<u16, VecDeque<u64>> = HashMap::new();

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

            // WI-213: prune receiver state for departed peers. The game loop
            // publishes the set of currently-active peer server_ids into
            // `active_peers` on every `PeerList` update. Any entry in the
            // receiver's per-peer state whose server_id is no longer in that
            // set has been superseded by a crash/departure and can be dropped.
            //
            // Gate: only prune when `active_peers` is non-empty. Before the
            // first PeerList arrives, the set is empty and pruning would wipe
            // everything on a fresh sim-server. This mirrors the "coordinator
            // not yet reached" state of normal mesh bootstrap.
            let active_snapshot: HashSet<u16> = {
                let guard = active_peers.read().unwrap_or_else(|e| e.into_inner());
                guard.clone()
            };
            if !active_snapshot.is_empty() {
                let session_before = session_of_peer.len();
                session_of_peer.retain(|sid, _| active_snapshot.contains(sid));
                let session_pruned = session_before - session_of_peer.len();

                let history_before = session_history.len();
                session_history.retain(|sid, _| active_snapshot.contains(sid));
                let history_pruned = history_before - session_history.len();

                let latest_before = latest_tick.len();
                latest_tick.retain(|(sid, _), _| active_snapshot.contains(sid));
                let latest_pruned = latest_before - latest_tick.len();

                // Also prune the shared peer_health maps so /metrics reflects
                // departed peers cleanly. Leaving stale entries behind would
                // make `latest_tick`, `session_id`, and the new WI-211/212
                // fields appear as "peer 2 is 10 000 ticks behind" long after
                // peer 2 has left the mesh.
                let health_pruned = {
                    let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                    let before = ph.latest_tick.len()
                        + ph.packets_received.len()
                        + ph.chunk_evictions.len()
                        + ph.peer_sessions.len()
                        + ph.stragglers_dropped.len()
                        + ph.session_changed_at_tick.len();
                    ph.latest_tick.retain(|sid, _| active_snapshot.contains(sid));
                    ph.packets_received
                        .retain(|sid, _| active_snapshot.contains(sid));
                    ph.chunk_evictions
                        .retain(|sid, _| active_snapshot.contains(sid));
                    ph.peer_sessions
                        .retain(|sid, _| active_snapshot.contains(sid));
                    ph.stragglers_dropped
                        .retain(|sid, _| active_snapshot.contains(sid));
                    ph.session_changed_at_tick
                        .retain(|sid, _| active_snapshot.contains(sid));
                    let after = ph.latest_tick.len()
                        + ph.packets_received.len()
                        + ph.chunk_evictions.len()
                        + ph.peer_sessions.len()
                        + ph.stragglers_dropped.len()
                        + ph.session_changed_at_tick.len();
                    before - after
                };

                let total = session_pruned + history_pruned + latest_pruned + health_pruned;
                if total > 0 {
                    tracing::info!(
                        session_of_peer_pruned = session_pruned,
                        session_history_pruned = history_pruned,
                        latest_tick_pruned = latest_pruned,
                        peer_health_pruned = health_pruned,
                        active_peers = active_snapshot.len(),
                        "Pruned receiver state for departed peers"
                    );
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
                if !note_session(
                    server_id.0,
                    session_id,
                    &mut session_of_peer,
                    &mut session_history,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                    &local_tick,
                ) {
                    // WI-211: straggler from a superseded session. Drop entirely.
                    continue;
                }
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
                if !note_session(
                    from_server.0,
                    session_id,
                    &mut session_of_peer,
                    &mut session_history,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                    &local_tick,
                ) {
                    // WI-211: straggler. Drop the transfer.
                    continue;
                }
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
                if !note_session(
                    server_id.0,
                    session_id,
                    &mut session_of_peer,
                    &mut session_history,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                    &local_tick,
                ) {
                    // WI-211: straggler chunk from a superseded session. Drop
                    // without adding to the reassembly map so stale chunks
                    // don't accumulate against eviction thresholds.
                    continue;
                }

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
    type SessionHistory = HashMap<u16, VecDeque<u64>>;
    type ReassemblyMap = HashMap<(u16, u64, u64), ChunkReassembly>;
    type LatestTickMap = HashMap<(u16, u64), u64>;
    type TestState = (
        SessionOfPeer,
        SessionHistory,
        ReassemblyMap,
        LatestTickMap,
        Arc<SharedPeerHealth>,
        Arc<SharedInbound>,
        AtomicU64,
    );

    fn test_state() -> TestState {
        let session_of_peer: HashMap<u16, u64> = HashMap::new();
        let session_history: HashMap<u16, VecDeque<u64>> = HashMap::new();
        let reassembly: HashMap<(u16, u64, u64), ChunkReassembly> = HashMap::new();
        let latest_tick: HashMap<(u16, u64), u64> = HashMap::new();
        let peer_health: Arc<SharedPeerHealth> =
            Arc::new(Mutex::new(PeerHealthAccumulator::default()));
        let inbound: Arc<SharedInbound> = Arc::new(Mutex::new(InboundQueue::default()));
        let local_tick = AtomicU64::new(0);
        (
            session_of_peer,
            session_history,
            reassembly,
            latest_tick,
            peer_health,
            inbound,
            local_tick,
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
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;
        let session_a = 0xAAAA_AAAA_AAAA_AAAA_u64;

        // Tick 100: two chunks
        for tick in [100u64, 101] {
            for chunk_index in [0u16, 1u16] {
                let proceed = note_session(
                    server_id,
                    session_a,
                    &mut session_of_peer,
                    &mut session_history,
                    &mut reassembly,
                    &mut latest_tick,
                    &peer_health,
                    &inbound,
                    &local_tick,
                );
                assert!(proceed, "single-session packets must proceed");
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
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
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
        let proceed = note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        assert!(proceed, "genuine session change must proceed");

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
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;
        let session_a = 0x1111_1111_1111_1111_u64;
        let session_b = 0x2222_2222_2222_2222_u64;

        // First packet ever from this peer: session A. Should NOT emit a reset.
        note_session(
            server_id,
            session_a,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
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
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        assert!(inbound.lock().unwrap().peer_session_resets.is_empty());

        // Session change A -> B: reset event emitted.
        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
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
                &mut session_history,
                &mut reassembly,
                &mut latest_tick,
                &peer_health,
                &inbound,
                &local_tick,
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
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();

        // Arbitrary session_id on first sight of server 7.
        note_session(
            7,
            0xDEAD_BEEF_CAFE_BABE,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
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

    // =========================================================================
    // WI-212: peer_health counters must survive session changes
    // =========================================================================

    #[test]
    fn session_change_does_not_zero_packets_received() {
        // WI-212 regression: before this fix, a session change zeroed
        // `packets_received` and `chunk_evictions` for the peer, making
        // /metrics report "no data" on restart. Counters must now survive
        // a session change. (`latest_tick` IS reset, because it's
        // specifically the current-session high-water mark — a restarted
        // peer begins at tick 0.)
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;
        let session_a = 0xAA_u64;
        let session_b = 0xBB_u64;

        session_of_peer.insert(server_id, session_a);
        {
            let mut ph = peer_health.lock().unwrap();
            ph.latest_tick.insert(server_id, 14_000);
            ph.packets_received.insert(server_id, 100);
            ph.chunk_evictions.insert(server_id, 7);
            // Also seed server 3 to ensure isolation.
            ph.latest_tick.insert(3, 50);
            ph.packets_received.insert(3, 3);
        }

        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );

        let ph = peer_health.lock().unwrap();
        // packets_received and chunk_evictions preserved (WI-212 goal).
        assert_eq!(
            ph.packets_received.get(&server_id),
            Some(&100),
            "packets_received must persist across session change (WI-212)"
        );
        assert_eq!(
            ph.chunk_evictions.get(&server_id),
            Some(&7),
            "chunk_evictions must persist across session change (WI-212)"
        );
        // latest_tick is RESET — a restarted peer starts at tick 0 and the
        // old high-water mark would make staleness_ticks misleading.
        assert_eq!(
            ph.latest_tick.get(&server_id),
            None,
            "latest_tick must be cleared on session change so staleness is meaningful"
        );
        // Server 3's counters untouched.
        assert_eq!(ph.latest_tick.get(&3), Some(&50));
        assert_eq!(ph.packets_received.get(&3), Some(&3));
    }

    #[test]
    fn session_change_records_at_tick() {
        // WI-212: local_tick value at the moment of session change is
        // recorded in `peer_health.session_changed_at_tick[sid]`.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;
        let session_a = 0xAA_u64;
        let session_b = 0xBB_u64;

        session_of_peer.insert(server_id, session_a);
        local_tick.store(42, Ordering::Relaxed);

        note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );

        let ph = peer_health.lock().unwrap();
        assert_eq!(
            ph.session_changed_at_tick.get(&server_id),
            Some(&42),
            "expected session_changed_at_tick to be stamped with local_tick=42"
        );
    }

    #[test]
    fn session_changed_at_tick_updates_on_each_change() {
        // Multiple session changes advance the recorded tick.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;

        session_of_peer.insert(server_id, 0xA);
        local_tick.store(10, Ordering::Relaxed);
        note_session(
            server_id,
            0xB,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        local_tick.store(20, Ordering::Relaxed);
        note_session(
            server_id,
            0xC,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );

        let ph = peer_health.lock().unwrap();
        assert_eq!(
            ph.session_changed_at_tick.get(&server_id),
            Some(&20),
            "second change should overwrite to tick=20"
        );
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

    // =========================================================================
    // WI-211: Session straggler LRU
    // =========================================================================

    #[test]
    fn session_straggler_in_history_is_dropped() {
        // Seed history with session A (current), bump to session B (current
        // now; A in history), then send a packet with session A. Assert no
        // PeerSessionReset event is pushed and the current session stays B.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;
        let session_a = 0xAA_u64;
        let session_b = 0xBB_u64;

        // Session A is current.
        session_of_peer.insert(server_id, session_a);

        // Move from A to B: pushes A into history.
        let proceed_b = note_session(
            server_id,
            session_b,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        assert!(proceed_b, "genuine session change to B must proceed");

        // Sanity: 1 reset event from the A->B change.
        assert_eq!(inbound.lock().unwrap().peer_session_resets.len(), 1);
        // Sanity: history contains A (the superseded session).
        assert_eq!(session_history.get(&server_id).unwrap().len(), 1);
        assert_eq!(session_history.get(&server_id).unwrap().front(), Some(&session_a));

        // Now a straggler with session A arrives. It must NOT flip the session
        // and must NOT push a new reset.
        let proceed_a = note_session(
            server_id,
            session_a,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        assert!(!proceed_a, "straggler must return false so callers skip processing");

        // Session is still B.
        assert_eq!(session_of_peer.get(&server_id), Some(&session_b));
        // Still only one reset event total.
        assert_eq!(inbound.lock().unwrap().peer_session_resets.len(), 1);
        // Straggler counter incremented.
        let ph = peer_health.lock().unwrap();
        assert_eq!(ph.stragglers_dropped.get(&server_id), Some(&1));
    }

    #[test]
    fn session_new_change_overwrites_history() {
        // Seed A, move to B, move to C. Assert history has [B, A] (newest
        // in front).
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;

        session_of_peer.insert(server_id, 0xA);
        note_session(
            server_id,
            0xB,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        note_session(
            server_id,
            0xC,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );

        let h = session_history.get(&server_id).unwrap();
        // Newest superseded session at the front: [B, A]
        let as_vec: Vec<u64> = h.iter().copied().collect();
        assert_eq!(as_vec, vec![0xB, 0xA]);
    }

    #[test]
    fn session_history_capacity_enforced() {
        // Cycle 6 sessions; assert history has only SESSION_HISTORY_CAPACITY
        // entries.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;

        session_of_peer.insert(server_id, 0x1);
        for next in [0x2u64, 0x3, 0x4, 0x5, 0x6, 0x7] {
            note_session(
                server_id,
                next,
                &mut session_of_peer,
                &mut session_history,
                &mut reassembly,
                &mut latest_tick,
                &peer_health,
                &inbound,
                &local_tick,
            );
        }

        let h = session_history.get(&server_id).unwrap();
        assert_eq!(
            h.len(),
            SESSION_HISTORY_CAPACITY,
            "history size must be capped at capacity"
        );
        // The oldest session (0x1) was evicted; the freshest superseded
        // sessions remain at the front.
        let as_vec: Vec<u64> = h.iter().copied().collect();
        assert_eq!(as_vec, vec![0x6, 0x5, 0x4, 0x3]);
    }

    #[test]
    fn session_stragglers_counter_increments() {
        // Each dropped straggler increments the counter.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();
        let server_id = 2u16;

        session_of_peer.insert(server_id, 0xA);
        // Move to B.
        note_session(
            server_id,
            0xB,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        // Fire three stragglers with session A.
        for _ in 0..3 {
            let proceed = note_session(
                server_id,
                0xA,
                &mut session_of_peer,
                &mut session_history,
                &mut reassembly,
                &mut latest_tick,
                &peer_health,
                &inbound,
                &local_tick,
            );
            assert!(!proceed);
        }

        let ph = peer_health.lock().unwrap();
        assert_eq!(ph.stragglers_dropped.get(&server_id), Some(&3));
    }

    #[test]
    fn session_straggler_does_not_drop_other_peers_counters() {
        // A straggler for server 2 must not zero or affect server 3's
        // peer_health entries.
        let (
            mut session_of_peer,
            mut session_history,
            mut reassembly,
            mut latest_tick,
            peer_health,
            inbound,
            local_tick,
        ) = test_state();

        session_of_peer.insert(2, 0xA);
        session_of_peer.insert(3, 0xC);
        {
            let mut ph = peer_health.lock().unwrap();
            ph.packets_received.insert(3, 99);
        }

        note_session(
            2,
            0xB,
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );
        note_session(
            2,
            0xA, // straggler from superseded session
            &mut session_of_peer,
            &mut session_history,
            &mut reassembly,
            &mut latest_tick,
            &peer_health,
            &inbound,
            &local_tick,
        );

        let ph = peer_health.lock().unwrap();
        assert_eq!(ph.packets_received.get(&3), Some(&99));
        assert_eq!(ph.stragglers_dropped.get(&2), Some(&1));
    }

    // =========================================================================
    // WI-213: Receiver state pruning for departed peers
    // =========================================================================

    /// Shared helper that mirrors the prune logic inside the receiver's
    /// `GLOBAL_SWEEP_INTERVAL` branch. Keeping this in a function lets unit
    /// tests drive it without spinning up a real UDP socket.
    fn prune_departed(
        active_peers: &HashSet<u16>,
        session_of_peer: &mut HashMap<u16, u64>,
        session_history: &mut HashMap<u16, VecDeque<u64>>,
        latest_tick: &mut HashMap<(u16, u64), u64>,
    ) -> usize {
        if active_peers.is_empty() {
            return 0;
        }
        let before = session_of_peer.len() + session_history.len() + latest_tick.len();
        session_of_peer.retain(|sid, _| active_peers.contains(sid));
        session_history.retain(|sid, _| active_peers.contains(sid));
        latest_tick.retain(|(sid, _), _| active_peers.contains(sid));
        let after = session_of_peer.len() + session_history.len() + latest_tick.len();
        before - after
    }

    #[test]
    fn receiver_prunes_state_for_departed_peer() {
        // Seed session_of_peer, session_history, and latest_tick with sids
        // {1, 2, 3}. Set active_peers to {1, 2}. Sweep. Assert sid 3 gone
        // from all three maps.
        let mut session_of_peer: HashMap<u16, u64> = HashMap::new();
        let mut session_history: HashMap<u16, VecDeque<u64>> = HashMap::new();
        let mut latest_tick: HashMap<(u16, u64), u64> = HashMap::new();

        session_of_peer.insert(1, 0xA);
        session_of_peer.insert(2, 0xB);
        session_of_peer.insert(3, 0xC);

        session_history.insert(1, VecDeque::from([0x1]));
        session_history.insert(2, VecDeque::from([0x2]));
        session_history.insert(3, VecDeque::from([0x3]));

        latest_tick.insert((1, 0xA), 100);
        latest_tick.insert((2, 0xB), 200);
        latest_tick.insert((3, 0xC), 300);

        let active: HashSet<u16> = [1u16, 2].into_iter().collect();
        let pruned = prune_departed(
            &active,
            &mut session_of_peer,
            &mut session_history,
            &mut latest_tick,
        );
        assert!(pruned > 0, "expected at least one pruned entry");

        assert!(!session_of_peer.contains_key(&3));
        assert!(!session_history.contains_key(&3));
        assert!(!latest_tick.contains_key(&(3, 0xC)));

        assert!(session_of_peer.contains_key(&1));
        assert!(session_of_peer.contains_key(&2));
    }

    #[test]
    fn receiver_does_not_prune_before_first_peer_list() {
        // Seed session_of_peer with {1, 2}, active_peers empty. Sweep. Assert
        // maps unchanged (the "no peer list yet" bootstrap case).
        let mut session_of_peer: HashMap<u16, u64> = HashMap::new();
        let mut session_history: HashMap<u16, VecDeque<u64>> = HashMap::new();
        let mut latest_tick: HashMap<(u16, u64), u64> = HashMap::new();

        session_of_peer.insert(1, 0xA);
        session_of_peer.insert(2, 0xB);
        latest_tick.insert((1, 0xA), 100);
        latest_tick.insert((2, 0xB), 200);

        let active: HashSet<u16> = HashSet::new();
        let pruned = prune_departed(
            &active,
            &mut session_of_peer,
            &mut session_history,
            &mut latest_tick,
        );
        assert_eq!(pruned, 0, "empty active_peers must not prune anything");

        assert_eq!(session_of_peer.len(), 2);
        assert_eq!(latest_tick.len(), 2);
    }
}
