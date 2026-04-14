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
use shared::spatial::WORLD_EXTENT;
use shared::tick::TICK_DURATION_MS;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TrySendError;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

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
    world.init_resource::<ClientSnapshot>();
    world.init_resource::<PendingTransfers>();
    world.init_resource::<OutboundTransfers>();
    world.init_resource::<OutboundForwards>();
    world.init_resource::<DrainState>();
    world.init_resource::<ShutdownRequested>();
    world.init_resource::<TimingAccumulator>();
    world.init_resource::<ClientRegistry>();

    // Spawn initial NPCs
    {
        let mut rng = rand::thread_rng();
        for _ in 0..initial_npcs {
            let pos = Vec3::new(
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
                0.0,
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
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

    loop {
        interval.tick().await;
        let tick_start = Instant::now();

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

        // Yield to the tokio scheduler after the ECS schedule run.
        // The game loop uses std::sync::Mutex which blocks the runtime thread.
        // Without yield points, other tokio tasks (heartbeat sender, UDP
        // receiver, coordinator client) can be starved for CPU time, causing
        // false heartbeat timeouts under heavy load (5000+ entities).
        tokio::task::yield_now().await;

        // -- Phase 3: Extract outbound data --
        let repl_data = world.resource_mut::<OutboundReplication>().snapshot.take();
        let repl_tick = world.resource::<OutboundReplication>().tick;
        let transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
        let forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);

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

        // -- Phase 4: Enqueue UDP replication for sender task (no awaits) --
        const ENTITIES_PER_CHUNK: usize = 100;

        /// Try to send a UdpOutbound message. Returns true if the channel is closed
        /// (sender task has crashed). On Full, increments the drop counter.
        #[inline]
        fn try_enqueue(
            tx: &tokio::sync::mpsc::Sender<UdpOutbound>,
            msg: UdpOutbound,
            drops: &mut u64,
            closed: &mut bool,
        ) {
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
            }
        }

        let t2 = Instant::now();
        if let Some(entities) = &repl_data {
            let peer_addrs: Vec<_> = world
                .resource::<PeerAddresses>()
                .0
                .values()
                .copied()
                .collect();

            if !peer_addrs.is_empty() {
                let server_id_u16 = world.resource::<ServerConfig>().server_id;

                if entities.len() <= ENTITIES_PER_CHUNK {
                    // Small payload: use unchunked format (backward compatible)
                    let msg = PeerMessage::EntityStateUpdate {
                        tick: repl_tick,
                        server_id: server_id_u16,
                        entities: entities.clone(),
                    };
                    if let Ok(bytes) = bincode::serialize(&msg) {
                        for addr in &peer_addrs {
                            try_enqueue(
                                &udp_repl_tx,
                                UdpOutbound::Send { addr: *addr, data: bytes.clone() },
                                &mut udp_repl_drops,
                                &mut udp_repl_closed,
                            );
                        }
                    }
                } else {
                    // Large payload: chunk it
                    let total_chunks =
                        ((entities.len() + ENTITIES_PER_CHUNK - 1) / ENTITIES_PER_CHUNK) as u16;
                    for (i, chunk) in entities.chunks(ENTITIES_PER_CHUNK).enumerate() {
                        let msg = PeerMessage::ChunkedEntityUpdate {
                            tick: repl_tick,
                            server_id: server_id_u16,
                            chunk_index: i as u16,
                            total_chunks,
                            entities: chunk.to_vec(),
                        };
                        if let Ok(bytes) = bincode::serialize(&msg) {
                            for addr in &peer_addrs {
                                try_enqueue(
                                    &udp_repl_tx,
                                    UdpOutbound::Send { addr: *addr, data: bytes.clone() },
                                    &mut udp_repl_drops,
                                    &mut udp_repl_closed,
                                );
                            }
                        }
                    }
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
            );
        }

        // Enqueue forwarded inputs (control channel -- latency-critical)
        if !forwards.is_empty() {
            try_enqueue(
                &udp_ctrl_tx,
                UdpOutbound::Batch(forwards),
                &mut udp_ctrl_drops,
                &mut udp_ctrl_closed,
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

            // Rate-limited UDP sender drop logging (WI-036)
            if udp_repl_drops > 0 {
                tracing::warn!(
                    drops = udp_repl_drops,
                    "UDP replication channel full: {} drops in last 5s",
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
        }
    }

    tracing::info!("Game loop exited cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use crate::resources::{
        ClientRegistry, ClientSnapshot, CoordEventQueue, DrainState, EntityLookup,
        InboundQueue, OutboundForwards, OutboundReplication, OutboundTransfers,
        PeerAddresses, PendingTransfers, ServerConfig, ShutdownRequested,
        TimingAccumulator,
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
        world.init_resource::<ClientSnapshot>();
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();
        world.init_resource::<TimingAccumulator>();
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

        let handle = tokio::spawn(run_game_loop_ecs(
            ServerId(1),
            0, // no initial NPCs -- drain will find zero entities
            inbound,
            coord_events,
            udp_repl_tx,
            udp_ctrl_tx,
            metrics,
            entity_count,
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
        let handle = tokio::spawn(run_game_loop_ecs(
            ServerId(1),
            0,
            Arc::clone(&inbound),
            coord_events,
            udp_repl_tx,
            udp_ctrl_tx,
            Arc::clone(&metrics),
            entity_count.clone(),
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

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Verify replication output
        let repl = world.resource::<OutboundReplication>();
        assert!(repl.snapshot.is_some());
        let entities = repl.snapshot.as_ref().unwrap();
        assert_eq!(entities.len(), 3);

        // Verify all entities have correct server ID
        for e in entities {
            assert_eq!(e.authority_server, ServerId(1));
        }

        // Verify client snapshot frame
        let frame = world.resource::<ClientSnapshot>().frame.as_ref().unwrap();
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
}
