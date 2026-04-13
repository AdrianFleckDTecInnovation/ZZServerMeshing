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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, Mutex};

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// Shared inbound buffer for async tasks to push events into.
pub struct SharedInbound {
    pub queue: tokio::sync::Mutex<InboundQueue>,
}

/// Shared coordinator event buffer.
pub struct SharedCoordEvents {
    pub queue: tokio::sync::Mutex<CoordEventQueue>,
}

/// Run the new ECS-based game loop.
pub async fn run_game_loop_ecs(
    server_id: ServerId,
    initial_npcs: u32,
    inbound: Arc<SharedInbound>,
    coord_events: Arc<SharedCoordEvents>,
    udp_socket: Arc<UdpSocket>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    metrics: Arc<Mutex<MetricsCollector>>,
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

    loop {
        interval.tick().await;
        let tick_start = Instant::now();

        // -- Phase 1: Swap inbound data from async buffers into ECS resources --
        let t0 = Instant::now();
        {
            let mut iq = inbound.queue.lock().await;
            let ecs_queue = world.resource_mut::<InboundQueue>();
            // Swap: move async buffer contents into ECS resource, leave empty buffer behind
            let peer_updates = std::mem::take(&mut iq.peer_updates);
            let authority_transfers = std::mem::take(&mut iq.authority_transfers);
            let forwarded_inputs = std::mem::take(&mut iq.forwarded_inputs);
            let client_inputs = std::mem::take(&mut iq.client_inputs);
            let forwarded_removes = std::mem::take(&mut iq.forwarded_removes);
            drop(ecs_queue);

            let mut ecs_queue = world.resource_mut::<InboundQueue>();
            ecs_queue.peer_updates = peer_updates;
            ecs_queue.authority_transfers = authority_transfers;
            ecs_queue.forwarded_inputs = forwarded_inputs;
            ecs_queue.client_inputs = client_inputs;
            ecs_queue.forwarded_removes = forwarded_removes;
        }
        {
            let mut cq = coord_events.queue.lock().await;
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

        // -- Phase 3: Extract outbound data --
        let repl_data = world.resource_mut::<OutboundReplication>().snapshot.take();
        let repl_tick = world.resource::<OutboundReplication>().tick;
        let client_frame = world.resource_mut::<ClientSnapshot>().frame.take();
        let transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
        let forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);

        // Count entities for heartbeat
        let auth_count = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        let shadow_count = world
            .query_filtered::<(), With<Shadow>>()
            .iter(&world)
            .count();
        let peer_count = world.resource::<PeerAddresses>().0.len();
        entity_count_atomic.store(auth_count as u32, Ordering::Relaxed);

        // -- Phase 4: Send UDP replication to peers --
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
                let msg = PeerMessage::EntityStateUpdate {
                    tick: repl_tick,
                    server_id: server_id_u16,
                    entities: entities.clone(),
                };

                if let Ok(bytes) = bincode::serialize(&msg) {
                    if bytes.len() <= 60_000 {
                        for addr in &peer_addrs {
                            if let Err(e) = udp_socket.send_to(&bytes, addr).await {
                                tracing::warn!("Failed to send to peer {addr}: {e}");
                            }
                        }
                    } else {
                        tracing::warn!(
                            "Replication packet too large ({} bytes), {} entities",
                            bytes.len(),
                            entities.len()
                        );
                    }
                }
            }
        }

        // Send authority transfers via UDP
        for (addr, bytes) in &transfers {
            if let Err(e) = udp_socket.send_to(bytes, addr).await {
                tracing::warn!("Failed to send transfer: {e}");
            }
        }

        // Send forwarded inputs via UDP
        for (addr, bytes) in &forwards {
            if let Err(e) = udp_socket.send_to(bytes, addr).await {
                tracing::warn!("Failed to forward input: {e}");
            }
        }
        let transfer_dur = t2.elapsed();

        // -- Phase 5: Broadcast client frame --
        let t3 = Instant::now();
        if let Some(frame) = client_frame {
            let _ = broadcast_tx.send(frame);
        }
        let repl_dur = t3.elapsed();

        // -- Phase 6: Record metrics --
        let tick_val = world.resource::<TickCounter>().0;
        {
            let mut m = metrics.lock().await;
            m.current_tick = tick_val;
            m.entity_count = auth_count;
            m.shadow_count = shadow_count;
            m.peer_count = peer_count;
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
            std::process::exit(0);
        }

        // Debug logging every 5 seconds
        if tick_val % (shared::tick::TICK_RATE as u64 * 5) == 0 {
            tracing::debug!(
                "Tick {} | auth:{} shadow:{} peers:{}",
                tick_val,
                auth_count,
                shadow_count,
                peer_count,
            );
        }
    }
}
