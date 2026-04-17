use crate::components::*;
use crate::conversion::spawn_from_entity_state_direct;
use crate::metrics::{MetricsCollector, TickTimings};
use crate::resources::*;
use crate::schedule::build_schedule;
use bevy_ecs::prelude::*;
use glam::Vec3;
use rand::Rng;
use shared::entity::{EntityState, ServerId};
use shared::protocol::PeerMessage;
use shared::tick::TICK_DURATION_MS;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TrySendError;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// Serialize a PeerMessage with protocol version prefix (2 bytes LE).
/// All UDP datagrams use this format so the receiver can reject version mismatches.
pub fn serialize_versioned(msg: &PeerMessage) -> Option<Vec<u8>> {
    let payload = bincode::serialize(msg).ok()?;
    let mut buf = Vec::with_capacity(2 + payload.len());
    buf.extend_from_slice(&shared::PROTOCOL_VERSION.to_le_bytes());
    buf.extend_from_slice(&payload);
    Some(buf)
}

/// Capacity for the UDP replication channel. Bulk entity-state updates are
/// best-effort: drops lose one tick of shadow freshness but are harmless.
/// 256 slots gives ~12 ticks of headroom at 20 Hz with 2 peers.
pub const UDP_REPLICATION_CHANNEL_SIZE: usize = 256;

/// Capacity for the UDP control channel. Carries latency-critical messages:
/// authority transfers, forwarded inputs, forwarded removes, transfer ACKs.
/// Drops here cause player-visible glitches, so the channel is kept small
/// and drained with priority.
pub const UDP_CONTROL_CHANNEL_SIZE: usize = 64;

/// Message from the game loop to the dedicated UDP sender task.
pub enum UdpOutbound {
    /// Single datagram to send to a specific peer.
    Send { addr: SocketAddr, data: Vec<u8> },
    /// Batch of datagrams to send.
    Batch(Vec<(SocketAddr, Vec<u8>)>),
}

/// Send a single `UdpOutbound` message over the socket.
async fn send_outbound(udp_socket: &UdpSocket, msg: UdpOutbound) {
    match msg {
        UdpOutbound::Send { addr, data } => {
            if let Err(e) = udp_socket.send_to(&data, addr).await {
                tracing::warn!("UDP send to {addr} failed: {e}");
            }
        }
        UdpOutbound::Batch(items) => {
            for (addr, data) in items {
                if let Err(e) = udp_socket.send_to(&data, addr).await {
                    tracing::warn!("UDP send to {addr} failed: {e}");
                }
            }
        }
    }
}

/// Dedicated task that sends UDP datagrams from two channels.
/// Control messages (authority transfers, forwarded inputs) are drained
/// with priority over replication messages (bulk entity state updates).
/// Runs until both channel senders are dropped (game loop exits).
pub async fn run_udp_sender(
    udp_socket: Arc<UdpSocket>,
    mut ctrl_rx: tokio::sync::mpsc::Receiver<UdpOutbound>,
    mut repl_rx: tokio::sync::mpsc::Receiver<UdpOutbound>,
) {
    loop {
        // Drain all pending control messages first (biased select).
        // Then fall through to wait on either channel.
        tokio::select! {
            biased;
            msg = ctrl_rx.recv() => {
                match msg {
                    Some(m) => send_outbound(&udp_socket, m).await,
                    None => {
                        // Control channel closed. Drain remaining replication.
                        while let Some(m) = repl_rx.recv().await {
                            send_outbound(&udp_socket, m).await;
                        }
                        break;
                    }
                }
            }
            msg = repl_rx.recv() => {
                match msg {
                    Some(m) => send_outbound(&udp_socket, m).await,
                    None => {
                        // Replication channel closed. Drain remaining control.
                        while let Some(m) = ctrl_rx.recv().await {
                            send_outbound(&udp_socket, m).await;
                        }
                        break;
                    }
                }
            }
        }
    }
    tracing::info!("UDP sender task exiting (channels closed)");
}

/// Shared inbound buffer for async tasks to push events into.
pub struct SharedInbound {
    pub queue: std::sync::Mutex<InboundQueue>,
}

/// Shared coordinator event buffer.
pub struct SharedCoordEvents {
    pub queue: std::sync::Mutex<CoordEventQueue>,
}

/// Shared outbound buffer for ECS -> coordinator messages (e.g., TransferResult).
pub struct SharedCoordOutbound {
    pub queue: std::sync::Mutex<Vec<shared::protocol::CoordClientMsg>>,
}

/// Per-peer network health statistics accumulated by the UDP receiver.
/// Read and reset by the game loop for metrics reporting.
pub struct PeerHealthAccumulator {
    /// Latest tick received from each peer.
    pub latest_tick: std::collections::HashMap<u16, u64>,
    /// Packets received from each peer since last reset.
    pub packets_received: std::collections::HashMap<u16, u32>,
    /// Chunk reassembly evictions per peer since last reset.
    pub chunk_evictions: std::collections::HashMap<u16, u32>,
}

impl Default for PeerHealthAccumulator {
    fn default() -> Self {
        Self {
            latest_tick: std::collections::HashMap::new(),
            packets_received: std::collections::HashMap::new(),
            chunk_evictions: std::collections::HashMap::new(),
        }
    }
}

/// Thread-safe wrapper for PeerHealthAccumulator.
/// Shared between the UDP receiver task (writes) and the game loop (reads/resets).
pub struct SharedPeerHealth {
    pub stats: std::sync::Mutex<PeerHealthAccumulator>,
}

/// Run the new ECS-based game loop.
pub async fn run_game_loop_ecs(
    server_id: ServerId,
    initial_npcs: u32,
    inbound: Arc<SharedInbound>,
    coord_events: Arc<SharedCoordEvents>,
    udp_repl_tx: tokio::sync::mpsc::Sender<UdpOutbound>,
    udp_ctrl_tx: tokio::sync::mpsc::Sender<UdpOutbound>,
    metrics: Arc<std::sync::Mutex<MetricsCollector>>,
    entity_count_atomic: Arc<AtomicU32>,
    weighted_load_atomic: Arc<AtomicU32>,
    coord_outbound: Arc<SharedCoordOutbound>,
    peer_health: Arc<SharedPeerHealth>,
) {
    // -- Create ECS World --
    let mut world = bevy_ecs::world::World::default();

    // Register all resources
    world.insert_resource(ServerConfig { server_id });
    world.init_resource::<TickCounter>();
    world.init_resource::<PeerAddresses>();
    world.init_resource::<EntityLookup>();
    world.init_resource::<InboundQueue>();
    world.init_resource::<CoordEventQueue>();
    world.init_resource::<OutboundReplication>();
    world.init_resource::<PendingTransfers>();
    world.init_resource::<OutboundTransfers>();
    world.init_resource::<OutboundForwards>();
    world.init_resource::<DrainState>();
    world.init_resource::<ShutdownRequested>();
    world.init_resource::<ClientRegistry>();
    world.init_resource::<OutboundCoordMessages>();
    world.init_resource::<EntityOwnership>();

    // Spawn initial NPCs. Use a capped range (±800) so NPCs cluster near the
    // origin where players spawn. With AoI filtering (500 unit radius), spreading
    // NPCs across the full WORLD_EXTENT (±5000) would leave <2% visible to any client.
    {
        let mut rng = rand::thread_rng();
        let spawn_range = 400.0_f32;
        for _ in 0..initial_npcs {
            let pos = Vec3::new(
                rng.gen_range(-spawn_range..spawn_range),
                0.0,
                rng.gen_range(-spawn_range..spawn_range),
            );
            let state = EntityState::new_npc(pos, server_id);
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }
    }
    tracing::info!("ECS World initialized with {} NPCs", initial_npcs);

    // Build the schedule
    let mut schedule = build_schedule();

    // Tick loop
    let mut interval = tokio::time::interval(TICK_DURATION);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::info!("ECS game loop started at {} Hz", shared::tick::TICK_RATE);

    // Rate-limited drop counters for UDP sender channels (WI-036)
    let mut udp_repl_drops: u64 = 0;
    let mut udp_ctrl_drops: u64 = 0;
    let mut udp_repl_closed = false;
    let mut udp_ctrl_closed = false;
    // Outbound UDP byte counter, reset each tick (WI-101)
    let mut udp_bytes_out: u64 = 0;

    loop {
        interval.tick().await;
        let tick_start = Instant::now();
        // Per-tick tracing span with server_id and tick for structured log correlation.
        // Scope is limited to the synchronous ECS phases (before any await points)
        // because entered spans are not Send across await boundaries.
        let current_tick = world.resource::<TickCounter>().0;
        let tick_span = tracing::info_span!("tick", server = server_id.0, tick = current_tick);
        let _tick_guard = tick_span.enter();

        // -- Phase 1: Swap inbound data from async buffers into ECS resources --
        let t0 = Instant::now();
        {
            let mut iq = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
            // Swap: move async buffer contents into ECS resource, leave empty buffer behind
            let peer_updates = std::mem::take(&mut iq.peer_updates);
            let authority_transfers = std::mem::take(&mut iq.authority_transfers);
            let forwarded_inputs = std::mem::take(&mut iq.forwarded_inputs);
            let client_inputs = std::mem::take(&mut iq.client_inputs);
            let forwarded_removes = std::mem::take(&mut iq.forwarded_removes);
            let register_clients = std::mem::take(&mut iq.register_clients);
            let unregister_clients = std::mem::take(&mut iq.unregister_clients);
            let client_player_assignments = std::mem::take(&mut iq.client_player_assignments);
            let force_full_snapshot_clients = std::mem::take(&mut iq.force_full_snapshot_clients);
            let transfer_acks = std::mem::take(&mut iq.transfer_acks);
            let mut ecs_queue = world.resource_mut::<InboundQueue>();
            ecs_queue.peer_updates = peer_updates;
            ecs_queue.authority_transfers = authority_transfers;
            ecs_queue.forwarded_inputs = forwarded_inputs;
            ecs_queue.client_inputs = client_inputs;
            ecs_queue.forwarded_removes = forwarded_removes;
            ecs_queue.register_clients = register_clients;
            ecs_queue.unregister_clients = unregister_clients;
            ecs_queue.client_player_assignments = client_player_assignments;
            ecs_queue.force_full_snapshot_clients = force_full_snapshot_clients;
            ecs_queue.transfer_acks = transfer_acks;
        }
        {
            let mut cq = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
            let peer_list_updates = std::mem::take(&mut cq.peer_list_updates);
            let rebalance_directives = std::mem::take(&mut cq.rebalance_directives);
            let drain_directives = std::mem::take(&mut cq.drain_directives);
            let recover_events = std::mem::take(&mut cq.recover_events);

            let mut ecs_queue = world.resource_mut::<CoordEventQueue>();
            ecs_queue.peer_list_updates = peer_list_updates;
            ecs_queue.rebalance_directives = rebalance_directives;
            ecs_queue.drain_directives = drain_directives;
            ecs_queue.recover_events = recover_events;
        }
        let input_drain_dur = t0.elapsed();

        // -- Phase 2: Increment tick and run all ECS systems --
        let t1 = Instant::now();
        world.resource_mut::<TickCounter>().0 += 1;
        schedule.run(&mut world);
        let sim_dur = t1.elapsed();

        // Drop the tracing span guard before yielding (entered spans are not Send)
        drop(_tick_guard);

        // Yield to the tokio scheduler after the ECS schedule run.
        // The game loop uses std::sync::Mutex which blocks the runtime thread.
        // Without yield points, other tokio tasks (heartbeat sender, UDP
        // receiver, coordinator client) can be starved for CPU time, causing
        // false heartbeat timeouts under heavy load (5000+ entities).
        tokio::task::yield_now().await;

        // -- Phase 3: Extract outbound data --
        let repl_full = world.resource_mut::<OutboundReplication>().full_snapshot.take();
        let repl_per_peer = std::mem::take(&mut world.resource_mut::<OutboundReplication>().per_peer);
        let repl_tick = world.resource::<OutboundReplication>().tick;
        let repl_hot_count = world.resource::<OutboundReplication>().hot_count;
        let repl_cold_count = world.resource::<OutboundReplication>().cold_count;
        let transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
        let forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);
        let coord_msgs = std::mem::take(&mut world.resource_mut::<OutboundCoordMessages>().messages);

        // Push coordinator-bound messages to the shared outbound queue
        if !coord_msgs.is_empty() {
            let mut q = coord_outbound.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.extend(coord_msgs);
        }

        // Count entities for heartbeat.
        // Exclude TransferOutbound entities: they are frozen and no longer consume
        // tick budget on this server. Including them double-counts (target also reports
        // them) and skews coordinator rebalance decisions.
        let auth_count = world
            .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
            .iter(&world)
            .count();
        let shadow_count = world
            .query_filtered::<(), With<Shadow>>()
            .iter(&world)
            .count();
        let peer_count = world.resource::<PeerAddresses>().0.len();
        let client_count = world.resource::<ClientRegistry>().clients.len();
        entity_count_atomic.store(auth_count as u32, Ordering::Relaxed);

        // Weighted load: players cost 3x, NPCs cost 1x.
        // Used by coordinator for smarter rebalance decisions.
        let mut weighted = 0u32;
        for kind in world
            .query_filtered::<&EntityKind, (With<Authoritative>, Without<TransferOutbound>)>()
            .iter(&world)
        {
            weighted += match kind.0 {
                shared::entity::EntityType::Player => 3,
                shared::entity::EntityType::Npc => 1,
            };
        }
        weighted_load_atomic.store(weighted, Ordering::Relaxed);

        // -- Phase 4: Enqueue UDP replication for sender task (no awaits) --
        // Keep each UDP packet under ~1400 bytes to avoid IP fragmentation.
        // Each entity is ~47 bytes (bincode), plus ~60 bytes message overhead.
        // (1400 - 60) / 47 ≈ 28 entities per chunk.
        const ENTITIES_PER_CHUNK: usize = 28;

        /// Try to send a UdpOutbound message. On Full, increments the drop counter.
        /// Tracks outbound byte count in `bytes_out`.
        #[inline]
        fn try_enqueue(
            tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
            msg: UdpOutbound,
            drops: &mut u64,
            closed: &mut bool,
            bytes_out: &mut u64,
        ) {
            // Count bytes before sending (whether or not it drops)
            let msg_bytes = match &msg {
                UdpOutbound::Send { data, .. } => data.len() as u64,
                UdpOutbound::Batch(items) => items.iter().map(|(_, d)| d.len() as u64).sum(),
            };
            if let Err(e) = tx.try_send(msg) {
                match e {
                    TrySendError::Full(_) => *drops += 1,
                    TrySendError::Closed(_) => {
                        if !*closed {
                            tracing::error!("UDP sender task has crashed, replication disabled");
                            *closed = true;
                        }
                    }
                }
            } else {
                // Only count bytes that were actually enqueued (not dropped)
                *bytes_out += msg_bytes;
            }
        }

        /// Serialize and enqueue entity replication to a single peer address.
        /// Handles chunking when entity count exceeds ENTITIES_PER_CHUNK.
        /// Accumulates enqueued byte count in `bytes_out`.
        fn enqueue_entities_to_peer(
            entities: &[EntityState],
            tick: u64,
            server_id: ServerId,
            addr: SocketAddr,
            tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
            drops: &mut u64,
            closed: &mut bool,
            bytes_out: &mut u64,
        ) {
            if entities.is_empty() {
                return;
            }
            if entities.len() <= ENTITIES_PER_CHUNK {
                let msg = PeerMessage::EntityStateUpdate {
                    tick,
                    server_id,
                    entities: entities.to_vec(),
                };
                if let Some(bytes) = serialize_versioned(&msg) {
                    try_enqueue(tx, UdpOutbound::Send { addr, data: bytes }, drops, closed, bytes_out);
                }
            } else {
                let total_chunks =
                    ((entities.len() + ENTITIES_PER_CHUNK - 1) / ENTITIES_PER_CHUNK) as u16;
                for (i, chunk) in entities.chunks(ENTITIES_PER_CHUNK).enumerate() {
                    let msg = PeerMessage::ChunkedEntityUpdate {
                        tick,
                        server_id,
                        chunk_index: i as u16,
                        total_chunks,
                        entities: chunk.to_vec(),
                    };
                    if let Some(bytes) = serialize_versioned(&msg) {
                        try_enqueue(tx, UdpOutbound::Send { addr, data: bytes }, drops, closed, bytes_out);
                    }
                }
            }
        }

        let t2 = Instant::now();
        let server_id_u16 = world.resource::<ServerConfig>().server_id;
        udp_bytes_out = 0;

        if let Some(entities) = &repl_full {
            // Full snapshot fallback: broadcast to all peers when filtering
            // provides no benefit (e.g., no shadows to compute bounds from).
            let peer_addrs: Vec<_> = world
                .resource::<PeerAddresses>()
                .0
                .values()
                .copied()
                .collect();
            for addr in &peer_addrs {
                enqueue_entities_to_peer(
                    entities,
                    repl_tick,
                    server_id_u16,
                    *addr,
                    &udp_repl_tx,
                    &mut udp_repl_drops,
                    &mut udp_repl_closed,
                    &mut udp_bytes_out,
                );
            }
        } else if !repl_per_peer.is_empty() {
            // Filtered replication: always attempt to send. Cold entities are
            // phase-spread (~25% per tick), so per-tick payload is bounded.
            // The try_send drop mechanism handles channel saturation naturally.
            // No boolean backpressure gate needed — phase spreading already
            // limits bandwidth, and blocking all replication would starve
            // shadows with no recovery path (audit #058, #066).
            let peer_addrs_map = &world.resource::<PeerAddresses>().0;
            for (peer_id, entities) in &repl_per_peer {
                if let Some(&addr) = peer_addrs_map.get(peer_id) {
                    enqueue_entities_to_peer(
                        entities,
                        repl_tick,
                        server_id_u16,
                        addr,
                        &udp_repl_tx,
                        &mut udp_repl_drops,
                        &mut udp_repl_closed,
                        &mut udp_bytes_out,
                    );
                }
            }
        }

        // Enqueue authority transfers (control channel -- latency-critical)
        if !transfers.is_empty() {
            try_enqueue(
                &udp_ctrl_tx,
                UdpOutbound::Batch(transfers),
                &mut udp_ctrl_drops,
                &mut udp_ctrl_closed,
                &mut udp_bytes_out,
            );
        }

        // Enqueue forwarded inputs (control channel -- latency-critical)
        if !forwards.is_empty() {
            try_enqueue(
                &udp_ctrl_tx,
                UdpOutbound::Batch(forwards),
                &mut udp_ctrl_drops,
                &mut udp_ctrl_closed,
                &mut udp_bytes_out,
            );
        }
        let transfer_dur = t2.elapsed();

        // Second yield point: after enqueuing all UDP datagrams.
        // This is the second-longest phase under heavy entity counts.
        tokio::task::yield_now().await;

        // -- Phase 5: Per-client frames are sent by prepare_client_snapshot --
        // (frames are pushed directly into per-client mpsc channels during the schedule run)
        let repl_dur = std::time::Duration::ZERO;

        // -- Phase 6: Record metrics --
        let tick_val = world.resource::<TickCounter>().0;
        {
            let mut m = metrics.lock().unwrap_or_else(|e| e.into_inner());
            m.current_tick = tick_val;
            m.entity_count = auth_count;
            m.shadow_count = shadow_count;
            m.peer_count = peer_count;
            m.connected_clients = client_count;
            // Replication filtering metrics
            if repl_full.is_some() {
                m.replication_mode = "full".to_string();
                m.replication_full_count = repl_full.as_ref().map_or(0, |e| e.len());
                m.replication_sent_count = repl_full.as_ref().map_or(0, |e| e.len()) * peer_count;
                m.replication_hot_count = 0;
                m.replication_cold_count = 0;
            } else {
                m.replication_mode = "tiered".to_string();
                m.replication_full_count = auth_count;
                m.replication_sent_count = repl_per_peer.values().map(|v| v.len()).sum();
                m.replication_hot_count = repl_hot_count;
                m.replication_cold_count = repl_cold_count;
            }
            // Network saturation telemetry (WI-101, WI-105)
            m.udp_repl_drops = udp_repl_drops;
            m.udp_ctrl_drops = udp_ctrl_drops;
            m.udp_bytes_out = udp_bytes_out;
            // Per-peer inbound health from receiver (WI-105)
            {
                let active_peers: std::collections::HashSet<u16> =
                    world.resource::<PeerAddresses>().0.keys().copied().collect();
                let ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
                let mut health_map = std::collections::HashMap::new();
                for &pid in &active_peers {
                    let latest = ph.latest_tick.get(&pid).copied().unwrap_or(0);
                    let staleness = if latest > 0 {
                        tick_val.saturating_sub(latest)
                    } else {
                        u64::MAX
                    };
                    health_map.insert(pid, crate::metrics::PeerHealthSnapshot {
                        latest_tick: latest,
                        staleness_ticks: staleness,
                        packets_received: ph.packets_received.get(&pid).copied().unwrap_or(0),
                        chunk_evictions: ph.chunk_evictions.get(&pid).copied().unwrap_or(0),
                    });
                }
                m.peer_health = health_map;
            }
            m.record(TickTimings {
                input_drain: input_drain_dur,
                simulation: sim_dur,
                transfers: transfer_dur,
                replication: repl_dur,
                shadow_cleanup: std::time::Duration::ZERO, // included in sim_dur now
                total: tick_start.elapsed(),
            });
        }

        // -- Phase 7: Check shutdown --
        if world.resource::<ShutdownRequested>().0 {
            tracing::info!("Drain complete -- shutting down");
            break;
        }

        // Debug logging every 5 seconds
        if tick_val % (shared::tick::TICK_RATE as u64 * 5) == 0 {
            tracing::debug!(
                "Tick {} | auth:{} shadow:{} peers:{} clients:{}",
                tick_val,
                auth_count,
                shadow_count,
                peer_count,
                client_count,
            );

            // Rate-limited UDP sender drop logging
            if udp_repl_drops > 0 {
                tracing::warn!(
                    drops = udp_repl_drops,
                    "UDP replication channel saturated: {} drops in last 5s",
                    udp_repl_drops
                );
                udp_repl_drops = 0;
            }
            if udp_ctrl_drops > 0 {
                tracing::warn!(
                    drops = udp_ctrl_drops,
                    "UDP control channel full: {} drops in last 5s (forwarded inputs/transfers may be lost)",
                    udp_ctrl_drops
                );
                udp_ctrl_drops = 0;
            }
            // Reset per-peer health counters every 5s reporting window (WI-105)
            {
                let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
                ph.packets_received.clear();
                ph.chunk_evictions.clear();
                // Don't clear latest_tick -- it's a high-water mark, not a counter
            }
        }
    }

    tracing::info!("Game loop exited cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use crate::resources::{
        ClientRegistry, CoordEventQueue, DrainState, EntityLookup,
        InboundQueue, OutboundForwards, OutboundReplication, OutboundTransfers,
        PeerAddresses, PendingTransfers, ServerConfig, ShutdownRequested,
    };
    use crate::schedule::build_schedule;
    use shared::entity::{EntityState, ServerId};
    use std::net::SocketAddr;

    fn setup_world(server_id: u16) -> bevy_ecs::world::World {
        let mut world = bevy_ecs::world::World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(server_id),
        });
        world.init_resource::<TickCounter>();
        world.init_resource::<PeerAddresses>();
        world.init_resource::<EntityLookup>();
        world.init_resource::<InboundQueue>();
        world.init_resource::<CoordEventQueue>();
        world.init_resource::<OutboundReplication>();
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();
        world.init_resource::<ClientRegistry>();
        world
    }

    // =========================================================================
    // #003: std::sync::Mutex tests -- prove SharedInbound/SharedCoordEvents
    // use std::sync::Mutex (not tokio::sync::Mutex)
    // =========================================================================

    #[test]
    fn shared_inbound_lockable_from_non_async_context() {
        // This test runs in a plain (non-async) #[test] function.
        // If SharedInbound used tokio::sync::Mutex, calling .lock() would require
        // .await and would not compile. The fact that this compiles and runs
        // proves it uses std::sync::Mutex.
        let inbound = SharedInbound {
            queue: std::sync::Mutex::new(InboundQueue::default()),
        };

        let mut q = inbound.queue.lock().unwrap();
        q.forwarded_inputs.push(crate::resources::ForwardedInput {
            entity_id: shared::entity::EntityId::new(),
            direction: [1.0, 0.0, 0.0],
            seq: 42,
        });
        assert_eq!(q.forwarded_inputs.len(), 1);
        assert_eq!(q.forwarded_inputs[0].seq, 42);
    }

    #[test]
    fn shared_coord_events_lockable_from_non_async_context() {
        // Same proof for SharedCoordEvents.
        let coord = SharedCoordEvents {
            queue: std::sync::Mutex::new(CoordEventQueue::default()),
        };

        let mut q = coord.queue.lock().unwrap();
        q.drain_directives.push(vec![2, 3]);
        assert_eq!(q.drain_directives.len(), 1);
        assert_eq!(q.drain_directives[0], vec![2, 3]);
    }

    #[test]
    fn shared_inbound_lock_poisoned_recovery() {
        // Verify the unwrap_or_else(|e| e.into_inner()) pattern works after poison.
        // std::sync::Mutex can become poisoned; tokio::sync::Mutex cannot.
        // This test proves both that we use std::sync::Mutex AND that the
        // poison-recovery pattern works correctly.
        let inbound = Arc::new(SharedInbound {
            queue: std::sync::Mutex::new(InboundQueue::default()),
        });

        // Poison the mutex by panicking while holding the lock
        let inbound_clone = Arc::clone(&inbound);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = inbound_clone.queue.lock().unwrap();
            panic!("intentional poison");
        }));
        assert!(result.is_err(), "panic should have been caught");

        // The mutex is now poisoned. lock() returns Err.
        assert!(inbound.queue.lock().is_err(), "mutex should be poisoned");

        // But the recovery pattern should still work
        let mut q = inbound
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        q.forwarded_inputs.push(crate::resources::ForwardedInput {
            entity_id: shared::entity::EntityId::new(),
            direction: [0.0, 0.0, 1.0],
            seq: 99,
        });
        assert_eq!(q.forwarded_inputs.len(), 1);
        assert_eq!(q.forwarded_inputs[0].seq, 99);
    }

    #[test]
    fn shared_inbound_concurrent_access() {
        // Verify multiple threads can use SharedInbound safely.
        // This would also compile with tokio::sync::Mutex (via block_on),
        // but combined with the non-async tests above, proves the migration.
        let inbound = Arc::new(SharedInbound {
            queue: std::sync::Mutex::new(InboundQueue::default()),
        });

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let inbound = Arc::clone(&inbound);
                std::thread::spawn(move || {
                    for seq in 0..10 {
                        let mut q = inbound.queue.lock().unwrap();
                        q.forwarded_inputs.push(crate::resources::ForwardedInput {
                            entity_id: shared::entity::EntityId::new(),
                            direction: [i as f32, 0.0, 0.0],
                            seq,
                        });
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let q = inbound.queue.lock().unwrap();
        assert_eq!(q.forwarded_inputs.len(), 40); // 4 threads x 10 items
    }

    // =========================================================================
    // #009: Clean exit -- ShutdownRequested causes break (not process::exit)
    // =========================================================================

    #[tokio::test]
    async fn shutdown_requested_causes_game_loop_to_return() {
        // If the old code used process::exit(0), this test would kill the
        // entire test process. The fact that the test completes normally
        // proves the game loop uses `break` instead.
        let inbound = Arc::new(SharedInbound {
            queue: std::sync::Mutex::new(InboundQueue::default()),
        });
        let coord_events = Arc::new(SharedCoordEvents {
            queue: std::sync::Mutex::new(CoordEventQueue::default()),
        });
        let metrics = Arc::new(std::sync::Mutex::new(
            crate::metrics::MetricsCollector::new(1),
        ));
        let entity_count = Arc::new(AtomicU32::new(0));

        // Create UDP sender channels (replication + control)
        let (udp_repl_tx, _udp_repl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_REPLICATION_CHANNEL_SIZE);
        let (udp_ctrl_tx, _udp_ctrl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);

        // Pre-set a drain directive so the loop exits after the first tick.
        // The drain system sets ShutdownRequested when drain is active and
        // no auth entities remain. With 0 initial NPCs, drain completes in 1 tick.
        // targets must be non-empty (process_drain returns early if empty).
        {
            let mut q = coord_events.queue.lock().unwrap();
            q.drain_directives.push(vec![2]); // target server 2 (no peer needed with 0 entities)
        }

        let weighted_load = Arc::new(AtomicU32::new(0));
        let coord_outbound = Arc::new(SharedCoordOutbound {
            queue: std::sync::Mutex::new(Vec::new()),
        });
        let peer_health = Arc::new(SharedPeerHealth {
            stats: std::sync::Mutex::new(PeerHealthAccumulator::default()),
        });
        let handle = tokio::spawn(run_game_loop_ecs(
            ServerId(1),
            0, // no initial NPCs -- drain will find zero entities
            inbound,
            coord_events,
            udp_repl_tx,
            udp_ctrl_tx,
            metrics,
            entity_count,
            weighted_load,
            coord_outbound,
            peer_health,
        ));

        // The game loop should exit cleanly (break, not process::exit).
        // If it used process::exit, this would never complete.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "game loop should return within 5s (proves break, not exit)"
        );
        let inner = result.unwrap();
        assert!(
            inner.is_ok(),
            "game loop should complete without panic"
        );
    }

    #[test]
    fn shutdown_requested_flag_in_schedule() {
        // Verify the schedule-based drain sets ShutdownRequested = true
        // when there are no entities and drain is active.
        let mut world = setup_world(1);

        // Activate drain with non-empty targets (process_drain returns early if targets empty)
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<DrainState>().started_tick = Some(1);
        world.resource_mut::<TickCounter>().0 = 2;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        assert!(
            world.resource::<ShutdownRequested>().0,
            "with drain active, non-empty targets, and 0 entities, ShutdownRequested should be true"
        );
    }

    // =========================================================================
    // #013: UDP sender task -- UdpOutbound enum and try_enqueue behavior
    // =========================================================================

    #[test]
    fn udp_outbound_send_variant() {
        let addr: SocketAddr = "127.0.0.1:9101".parse().unwrap();
        let msg = UdpOutbound::Send {
            addr,
            data: vec![1, 2, 3],
        };
        match msg {
            UdpOutbound::Send { addr: a, data: d } => {
                assert_eq!(a, addr);
                assert_eq!(d, vec![1, 2, 3]);
            }
            _ => panic!("expected Send variant"),
        }
    }

    #[test]
    fn udp_outbound_batch_variant() {
        let items = vec![
            ("127.0.0.1:9101".parse::<SocketAddr>().unwrap(), vec![1]),
            ("127.0.0.1:9102".parse::<SocketAddr>().unwrap(), vec![2]),
        ];
        let msg = UdpOutbound::Batch(items.clone());
        match msg {
            UdpOutbound::Batch(b) => {
                assert_eq!(b.len(), 2);
                assert_eq!(b[0].1, vec![1]);
                assert_eq!(b[1].1, vec![2]);
            }
            _ => panic!("expected Batch variant"),
        }
    }

    #[test]
    fn udp_replication_channel_size_is_256() {
        assert_eq!(UDP_REPLICATION_CHANNEL_SIZE, 256);
    }

    #[test]
    fn udp_control_channel_size_is_64() {
        assert_eq!(UDP_CONTROL_CHANNEL_SIZE, 64);
    }

    #[test]
    fn try_enqueue_success_on_open_channel() {
        // Create a channel with room and verify try_enqueue succeeds
        // (drops counter stays 0, closed stays false).
        let (tx, _rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);
        let mut drops: u64 = 0;
        let mut closed = false;

        let msg = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![42],
        };
        try_enqueue(&tx, msg, &mut drops, &mut closed);
        assert_eq!(drops, 0);
        assert!(!closed);
    }

    #[test]
    fn try_enqueue_full_channel_increments_drops() {
        // Create a channel with capacity 1, fill it, then verify the next
        // try_enqueue increments the drop counter.
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(1);

        let mut drops: u64 = 0;
        let mut closed = false;

        // First message fits
        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![1],
            },
            &mut drops,
            &mut closed,
        );
        assert_eq!(drops, 0);

        // Second message: channel full
        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![2],
            },
            &mut drops,
            &mut closed,
        );
        assert_eq!(drops, 1, "drop counter should increment when channel is full");
        assert!(!closed, "channel is full but not closed");

        // Third message: still full
        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![3],
            },
            &mut drops,
            &mut closed,
        );
        assert_eq!(drops, 2, "drop counter should increment again");
    }

    #[test]
    fn try_enqueue_closed_channel_sets_flag() {
        // Drop the receiver to close the channel, then verify try_enqueue
        // sets the closed flag.
        let (tx, rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        drop(rx); // close the channel

        let mut drops: u64 = 0;
        let mut closed = false;

        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![1],
            },
            &mut drops,
            &mut closed,
        );
        assert!(closed, "closed flag should be set when receiver is dropped");
        assert_eq!(drops, 0, "closed channel should not increment drops");
    }

    #[test]
    fn try_enqueue_closed_flag_only_set_once() {
        // Once the closed flag is set, subsequent calls should not log again
        // (the tracing::error only fires on the first detection).
        let (tx, rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        drop(rx);

        let mut drops: u64 = 0;
        let mut closed = false;

        // First call: sets closed
        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![1],
            },
            &mut drops,
            &mut closed,
        );
        assert!(closed);

        // Second call: closed is already true, so the inner block is skipped
        try_enqueue(
            &tx,
            UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![2],
            },
            &mut drops,
            &mut closed,
        );
        // Still closed, drops still 0
        assert!(closed);
        assert_eq!(drops, 0);
    }

    #[tokio::test]
    async fn udp_sender_task_processes_send_via_control() {
        // Verify the run_udp_sender task reads Send messages from the control
        // channel and actually sends them over the UDP socket.
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0").await.unwrap()
        );
        let local_addr = socket.local_addr().unwrap();

        // Receiver socket to verify the datagram arrives
        let recv_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let (ctrl_tx, ctrl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);
        let (_repl_tx, repl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_REPLICATION_CHANNEL_SIZE);

        let sender_handle =
            tokio::spawn(run_udp_sender(Arc::clone(&socket), ctrl_rx, repl_rx));

        // Send a message through the control channel
        ctrl_tx.send(UdpOutbound::Send {
            addr: recv_addr,
            data: vec![0xDE, 0xAD],
        })
        .await
        .unwrap();

        // Receive on the other socket
        let mut buf = [0u8; 64];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recv_socket.recv_from(&mut buf),
        )
        .await;
        assert!(result.is_ok(), "should receive datagram within 2s");
        let (len, from) = result.unwrap().unwrap();
        assert_eq!(&buf[..len], &[0xDE, 0xAD]);
        assert_eq!(from, local_addr);

        // Drop senders to close channels, which makes run_udp_sender exit
        drop(ctrl_tx);
        drop(_repl_tx);
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sender_handle,
        )
        .await;
    }

    #[tokio::test]
    async fn udp_sender_task_processes_batch_via_replication() {
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0").await.unwrap()
        );

        let recv_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let (_ctrl_tx, ctrl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);
        let (repl_tx, repl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_REPLICATION_CHANNEL_SIZE);

        let _sender_handle =
            tokio::spawn(run_udp_sender(Arc::clone(&socket), ctrl_rx, repl_rx));

        // Send a batch with 3 datagrams via the replication channel
        let batch = vec![
            (recv_addr, vec![1]),
            (recv_addr, vec![2]),
            (recv_addr, vec![3]),
        ];
        repl_tx.send(UdpOutbound::Batch(batch)).await.unwrap();

        // Receive all 3
        let mut received = Vec::new();
        for _ in 0..3 {
            let mut buf = [0u8; 64];
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                recv_socket.recv_from(&mut buf),
            )
            .await;
            assert!(result.is_ok());
            let (len, _) = result.unwrap().unwrap();
            received.push(buf[..len].to_vec());
        }

        received.sort();
        assert_eq!(received, vec![vec![1], vec![2], vec![3]]);

        drop(repl_tx);
        drop(_ctrl_tx);
    }

    #[tokio::test]
    async fn udp_sender_task_exits_on_both_channels_close() {
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0").await.unwrap()
        );

        let (ctrl_tx, ctrl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);
        let (repl_tx, repl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_REPLICATION_CHANNEL_SIZE);

        let sender_handle =
            tokio::spawn(run_udp_sender(Arc::clone(&socket), ctrl_rx, repl_rx));

        // Drop both senders to close both channels
        drop(ctrl_tx);
        drop(repl_tx);

        // The sender task should exit cleanly
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sender_handle,
        )
        .await;
        assert!(result.is_ok(), "sender task should exit when both channels close");
        assert!(result.unwrap().is_ok(), "sender task should exit without panic");
    }

    // =========================================================================
    // Integration: game loop produces correct replication after mutex swap
    // =========================================================================

    #[tokio::test]
    async fn game_loop_produces_replication_through_std_mutex() {
        // Full integration: push peer data through SharedInbound (std::sync::Mutex),
        // run the game loop for 1 tick, verify replication output is produced.
        let inbound = Arc::new(SharedInbound {
            queue: std::sync::Mutex::new(InboundQueue::default()),
        });
        let coord_events = Arc::new(SharedCoordEvents {
            queue: std::sync::Mutex::new(CoordEventQueue::default()),
        });
        let metrics = Arc::new(std::sync::Mutex::new(
            crate::metrics::MetricsCollector::new(1),
        ));
        let entity_count = Arc::new(AtomicU32::new(0));

        let (udp_repl_tx, mut udp_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_REPLICATION_CHANNEL_SIZE);
        let (udp_ctrl_tx, _udp_ctrl_rx) =
            tokio::sync::mpsc::channel::<UdpOutbound>(UDP_CONTROL_CHANNEL_SIZE);

        // Pre-drain: set a drain directive so the loop will exit after one tick.
        // targets must be non-empty (process_drain returns early if empty).
        {
            let mut q = coord_events.queue.lock().unwrap();
            q.drain_directives.push(vec![2]);
        }

        // Start with 0 NPCs so drain completes immediately (no entities to transfer).
        let weighted_load = Arc::new(AtomicU32::new(0));
        let coord_outbound = Arc::new(SharedCoordOutbound {
            queue: std::sync::Mutex::new(Vec::new()),
        });
        let peer_health = Arc::new(SharedPeerHealth {
            stats: std::sync::Mutex::new(PeerHealthAccumulator::default()),
        });
        let handle = tokio::spawn(run_game_loop_ecs(
            ServerId(1),
            0,
            Arc::clone(&inbound),
            coord_events,
            udp_repl_tx,
            udp_ctrl_tx,
            Arc::clone(&metrics),
            entity_count.clone(),
            weighted_load,
            coord_outbound,
            peer_health,
        ));

        // The loop should complete (drain with 0 entities = immediate shutdown).
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle,
        )
        .await;
        assert!(result.is_ok(), "game loop should complete within 5s");

        // Verify tick advanced in the metrics collector
        let m = metrics.lock().unwrap();
        assert!(m.current_tick > 0, "tick should have advanced");

        // The std::sync::Mutex was used to push drain events (above)
        // and to read metrics (here), proving the mutex swap is functional.
        udp_rx.close();
    }

    #[test]
    fn game_loop_replication_data_correct_after_schedule_tick() {
        // Verify that a manual schedule tick produces correct replication
        // data, confirming the ECS pipeline works end-to-end with std::sync::Mutex.
        let mut world = setup_world(1);

        // Spawn 3 auth NPCs
        for i in 0..3 {
            let pos = Vec3::new(i as f32 * 10.0, 0.0, 0.0);
            let state = EntityState::new_npc(pos, ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        // Register a test client so prepare_client_snapshot sends a frame
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::resources::ClientFrame>(8);
        let acked = Arc::new(std::sync::atomic::AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>().register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Verify replication output
        let repl = world.resource::<OutboundReplication>();
        assert!(repl.full_snapshot.is_some());
        let entities = repl.full_snapshot.as_ref().unwrap();
        assert_eq!(entities.len(), 3);

        // Verify all entities have correct server ID
        for e in entities {
            assert_eq!(e.authority_server, ServerId(1));
        }

        // Verify client snapshot frame via test client channel
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x01); // full snapshot
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 3);
    }

    // =========================================================================
    // try_enqueue helper -- extracted from game_loop for direct testing
    // =========================================================================

    // Re-implement try_enqueue here so tests can call it directly.
    // The production code defines it as `fn try_enqueue` inside run_game_loop_ecs.
    // We replicate the exact same logic to verify behavior.
    #[inline]
    fn try_enqueue(
        tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
        msg: UdpOutbound,
        drops: &mut u64,
        closed: &mut bool,
    ) {
        // Test version: no bytes_out tracking (matches production logic sans telemetry)
        if let Err(e) = tx.try_send(msg) {
            match e {
                TrySendError::Full(_) => *drops += 1,
                TrySendError::Closed(_) => {
                    if !*closed {
                        *closed = true;
                    }
                }
            }
        }
    }

    // =========================================================================
    // WI-102: PeerHealthAccumulator unit tests
    // =========================================================================

    #[test]
    fn peer_health_accumulator_default() {
        let acc = PeerHealthAccumulator::default();
        assert!(acc.latest_tick.is_empty());
        assert!(acc.packets_received.is_empty());
        assert!(acc.chunk_evictions.is_empty());
    }

    #[test]
    fn peer_health_accumulator_increment() {
        let mut acc = PeerHealthAccumulator::default();
        acc.latest_tick.insert(2, 100);
        *acc.packets_received.entry(2).or_insert(0) += 1;
        *acc.packets_received.entry(2).or_insert(0) += 1;
        assert_eq!(acc.latest_tick[&2], 100);
        assert_eq!(acc.packets_received[&2], 2);
    }

    #[test]
    fn peer_health_accumulator_multiple_peers() {
        let mut acc = PeerHealthAccumulator::default();
        acc.latest_tick.insert(2, 50);
        acc.latest_tick.insert(3, 48);
        *acc.packets_received.entry(2).or_insert(0) += 5;
        *acc.packets_received.entry(3).or_insert(0) += 3;
        *acc.chunk_evictions.entry(3).or_insert(0) += 1;

        assert_eq!(acc.latest_tick[&2], 50);
        assert_eq!(acc.latest_tick[&3], 48);
        assert_eq!(acc.packets_received[&2], 5);
        assert_eq!(acc.packets_received[&3], 3);
        assert_eq!(acc.chunk_evictions.get(&2), None);
        assert_eq!(acc.chunk_evictions[&3], 1);
    }

    #[test]
    fn shared_peer_health_lockable_from_non_async_context() {
        // Proves SharedPeerHealth uses std::sync::Mutex (not tokio).
        let sph = SharedPeerHealth {
            stats: std::sync::Mutex::new(PeerHealthAccumulator::default()),
        };
        let mut acc = sph.stats.lock().unwrap();
        acc.latest_tick.insert(2, 42);
        assert_eq!(acc.latest_tick[&2], 42);
    }

    #[test]
    fn peer_health_accumulator_latest_tick_only_increases() {
        // The receiver uses `if tick > *lt { *lt = tick }` -- verify that
        // later ticks overwrite earlier ones but not vice versa.
        let mut acc = PeerHealthAccumulator::default();
        let lt = acc.latest_tick.entry(2).or_insert(0);
        // Tick 50 arrives
        if 50 > *lt { *lt = 50; }
        assert_eq!(acc.latest_tick[&2], 50);
        // Tick 30 arrives (out of order) -- should NOT overwrite
        let lt = acc.latest_tick.entry(2).or_insert(0);
        if 30 > *lt { *lt = 30; }
        assert_eq!(acc.latest_tick[&2], 50, "later OOO tick should not reduce latest_tick");
        // Tick 100 arrives -- should overwrite
        let lt = acc.latest_tick.entry(2).or_insert(0);
        if 100 > *lt { *lt = 100; }
        assert_eq!(acc.latest_tick[&2], 100);
    }

    #[test]
    fn peer_health_accumulator_clear_resets_counters_not_ticks() {
        // The game loop clears packets_received and chunk_evictions every 5s
        // but keeps latest_tick (high-water mark). Verify this pattern works.
        let mut acc = PeerHealthAccumulator::default();
        acc.latest_tick.insert(2, 500);
        *acc.packets_received.entry(2).or_insert(0) += 10;
        *acc.chunk_evictions.entry(2).or_insert(0) += 3;

        // Simulate 5s reset (what game loop does)
        acc.packets_received.clear();
        acc.chunk_evictions.clear();
        // Do NOT clear latest_tick

        assert_eq!(acc.latest_tick[&2], 500, "latest_tick should survive reset");
        assert!(acc.packets_received.is_empty(), "packets_received should be cleared");
        assert!(acc.chunk_evictions.is_empty(), "chunk_evictions should be cleared");
    }

    #[test]
    fn peer_health_accumulator_independent_peers() {
        // Incrementing peer 2 should not affect peer 3.
        let mut acc = PeerHealthAccumulator::default();
        *acc.packets_received.entry(2).or_insert(0) += 1;
        *acc.packets_received.entry(3).or_insert(0) += 5;
        *acc.chunk_evictions.entry(2).or_insert(0) += 2;

        assert_eq!(acc.packets_received[&2], 1);
        assert_eq!(acc.packets_received[&3], 5);
        assert_eq!(acc.chunk_evictions[&2], 2);
        assert_eq!(acc.chunk_evictions.get(&3), None);
    }

    // =========================================================================
    // WI-101: try_enqueue with bytes_out tracking
    // =========================================================================

    // Re-implement the production try_enqueue (with bytes_out) for testing.
    fn try_enqueue_with_bytes(
        tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
        msg: UdpOutbound,
        drops: &mut u64,
        closed: &mut bool,
        bytes_out: &mut u64,
    ) {
        let msg_bytes = match &msg {
            UdpOutbound::Send { data, .. } => data.len() as u64,
            UdpOutbound::Batch(items) => items.iter().map(|(_, d)| d.len() as u64).sum(),
        };
        if let Err(e) = tx.try_send(msg) {
            match e {
                TrySendError::Full(_) => *drops += 1,
                TrySendError::Closed(_) => {
                    if !*closed {
                        *closed = true;
                    }
                }
            }
        } else {
            *bytes_out += msg_bytes;
        }
    }

    #[test]
    fn try_enqueue_bytes_out_counted_on_success() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        let msg = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![0u8; 100],
        };
        try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 100, "successful send should count bytes");
        assert_eq!(drops, 0);
    }

    #[test]
    fn try_enqueue_bytes_out_not_counted_on_drop() {
        // Channel capacity 1, fill it, then verify bytes are NOT counted on drop.
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(1);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        // First message fits
        let msg1 = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![0u8; 50],
        };
        try_enqueue_with_bytes(&tx, msg1, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 50);

        // Second message: channel full, should NOT count bytes
        let msg2 = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![0u8; 200],
        };
        try_enqueue_with_bytes(&tx, msg2, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 50, "dropped message bytes should NOT be counted");
        assert_eq!(drops, 1);
    }

    #[test]
    fn try_enqueue_bytes_out_not_counted_on_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        drop(rx);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        let msg = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![0u8; 75],
        };
        try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 0, "closed channel should NOT count bytes");
        assert!(closed);
    }

    #[test]
    fn try_enqueue_bytes_out_batch_sums_all_items() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        let items = vec![
            ("127.0.0.1:9101".parse::<SocketAddr>().unwrap(), vec![0u8; 30]),
            ("127.0.0.1:9102".parse::<SocketAddr>().unwrap(), vec![0u8; 70]),
            ("127.0.0.1:9103".parse::<SocketAddr>().unwrap(), vec![0u8; 100]),
        ];
        let msg = UdpOutbound::Batch(items);
        try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 200, "batch bytes should be sum of all items (30+70+100)");
    }

    #[test]
    fn try_enqueue_bytes_out_accumulates_across_calls() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        for _ in 0..5 {
            let msg = UdpOutbound::Send {
                addr: "127.0.0.1:9101".parse().unwrap(),
                data: vec![0u8; 40],
            };
            try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        }
        assert_eq!(bytes_out, 200, "5 sends of 40 bytes = 200 total");
    }

    #[test]
    fn try_enqueue_bytes_out_empty_send() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        let msg = UdpOutbound::Send {
            addr: "127.0.0.1:9101".parse().unwrap(),
            data: vec![],
        };
        try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 0, "empty payload should count 0 bytes");
    }

    #[test]
    fn try_enqueue_bytes_out_empty_batch() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UdpOutbound>(8);
        let mut drops: u64 = 0;
        let mut closed = false;
        let mut bytes_out: u64 = 0;

        let msg = UdpOutbound::Batch(vec![]);
        try_enqueue_with_bytes(&tx, msg, &mut drops, &mut closed, &mut bytes_out);
        assert_eq!(bytes_out, 0, "empty batch should count 0 bytes");
    }
}
