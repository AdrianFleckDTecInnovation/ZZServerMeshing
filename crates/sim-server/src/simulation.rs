use crate::metrics::{MetricsCollector, TickTimings};
use crate::replication;
use crate::world::World;
use shared::entity::{EntityType, EntityId, ServerId};
use shared::protocol::{ClientMessage, PeerMessage};
use shared::tick::{TICK_DURATION_MS, TICK_RATE};
use glam::Vec3;
use rand::Rng;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, RwLock};
use uuid::Uuid;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// NPC direction change interval in ticks.
const NPC_DIRECTION_CHANGE_INTERVAL: u64 = (TICK_RATE as u64) * 3; // every 3 seconds

pub async fn run_game_loop(
    world: Arc<RwLock<World>>,
    mut input_rx: mpsc::UnboundedReceiver<ClientMessage>,
    udp_socket: Arc<UdpSocket>,
    metrics: Arc<Mutex<MetricsCollector>>,
) {
    let mut interval = tokio::time::interval(TICK_DURATION);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::info!("Game loop started at {} Hz", TICK_RATE);

    loop {
        interval.tick().await;
        let tick_start = Instant::now();

        // Phase 1: Drain all pending inputs
        let t0 = Instant::now();
        while let Ok(input) = input_rx.try_recv() {
            let mut w = world.write().await;
            process_input(&mut w, input);
        }
        let input_drain_dur = t0.elapsed();

        // Simulate + replicate + handle transfers
        let (sim_dur, transfer_dur, repl_dur, shadow_dur);
        {
            let mut w = world.write().await;

            // Phase 2: Simulate
            let t1 = Instant::now();
            simulate_tick(&mut w);
            sim_dur = t1.elapsed();

            // Phase 3: Process pending authority transfers
            let t2 = Instant::now();
            process_transfers(&mut w, &udp_socket).await;

            // Handle graceful drain
            if w.draining {
                process_drain(&mut w, &udp_socket).await;
                if w.entities.is_empty() {
                    tracing::info!("Drain complete — shutting down");
                    std::process::exit(0);
                }
            }
            transfer_dur = t2.elapsed();

            // Phase 4: Replicate authoritative entities to peers
            let t3 = Instant::now();
            replication::replicate_to_peers(&w, &udp_socket).await;
            repl_dur = t3.elapsed();

            // Phase 5: Clean up stale shadow entities
            let t4 = Instant::now();
            w.cleanup_shadows();
            shadow_dur = t4.elapsed();

            // Update metrics collector state
            let mut m = metrics.lock().await;
            m.current_tick = w.tick;
            m.entity_count = w.entities.len();
            m.shadow_count = w.shadow_entities.len();
            m.peer_count = w.peer_addrs.len();
            m.record(TickTimings {
                input_drain: input_drain_dur,
                simulation: sim_dur,
                transfers: transfer_dur,
                replication: repl_dur,
                shadow_cleanup: shadow_dur,
                total: tick_start.elapsed(),
            });
        }
    }
}

fn process_input(world: &mut World, input: ClientMessage) {
    match input {
        ClientMessage::SpawnEntity { position, entity_type } => {
            let pos = Vec3::new(position[0], position[1], position[2]);
            let etype = match entity_type.as_str() {
                "player" => EntityType::Player,
                _ => EntityType::Npc,
            };
            let id = world.spawn_entity(pos, etype);
            tracing::debug!("Spawned {:?} entity {}", etype, id);
        }
        ClientMessage::SpawnPlayer { player_id, position } => {
            if let Ok(uuid) = Uuid::parse_str(&player_id) {
                let pos = Vec3::new(position[0], position[1], position[2]);
                let id = world.spawn_player_with_id(uuid, pos);
                tracing::info!("Spawned player {}", id);
            }
        }
        ClientMessage::MoveInput { entity_id, direction } => {
            if let Ok(uuid) = Uuid::parse_str(&entity_id) {
                let eid = EntityId(uuid);
                let dir = Vec3::new(direction[0], direction[1], direction[2]);
                world.apply_move_input(&eid, dir);
            }
        }
        ClientMessage::RemoveEntity { entity_id } => {
            if let Ok(uuid) = Uuid::parse_str(&entity_id) {
                let eid = EntityId(uuid);
                world.remove_entity(&eid);
                tracing::info!("Removed entity {}", eid);
            }
        }
        ClientMessage::RequestSnapshot => {}
    }
}

fn simulate_tick(world: &mut World) {
    world.tick += 1;

    let tick = world.tick;
    let dt = TICK_DURATION.as_secs_f32();

    // Collect NPC IDs for behavior update
    let npc_ids: Vec<EntityId> = world
        .entities
        .iter()
        .filter(|(_, e)| e.entity_type == EntityType::Npc)
        .map(|(id, _)| *id)
        .collect();

    // Update NPC behaviors
    let mut rng = rand::thread_rng();
    for id in &npc_ids {
        if let Some(entity) = world.entities.get_mut(id) {
            let phase = (id.0.as_u128() % NPC_DIRECTION_CHANGE_INTERVAL as u128) as u64;
            if tick % NPC_DIRECTION_CHANGE_INTERVAL == phase {
                let angle = rng.gen_range(0.0..std::f32::consts::TAU);
                let speed = rng.gen_range(1.0..3.0);
                entity.velocity = Vec3::new(angle.cos() * speed, 0.0, angle.sin() * speed);
            }
        }
    }

    // Integrate positions
    for entity in world.entities.values_mut() {
        entity.position += entity.velocity * dt;
    }

    // Clamp to world bounds
    world.clamp_all_positions();

    if tick % (TICK_RATE as u64 * 5) == 0 {
        tracing::debug!(
            "Tick {} | auth:{} shadow:{} peers:{}",
            tick,
            world.entities.len(),
            world.shadow_entities.len(),
            world.peer_addrs.len(),
        );
    }
}

/// Process pending authority transfers from coordinator rebalance directives.
async fn process_transfers(world: &mut World, udp_socket: &UdpSocket) {
    let transfer = world.pending_transfers.take();
    let Some((_entity_ids, target_server)) = transfer else {
        return;
    };

    // Look up target server's UDP address
    let target_addr = match world.peer_addrs.get(&target_server) {
        Some(addr) => *addr,
        None => {
            tracing::warn!("Rebalance target S{target_server} not in peer list");
            return;
        }
    };

    // Pick NPC entities to transfer (never transfer players — disruptive).
    // Select entities that are spatially distant from the center of our cluster
    // (peripheral entities are cheapest to move).
    let mut npc_ids: Vec<(EntityId, f32)> = world
        .entities
        .iter()
        .filter(|(_, e)| e.entity_type == EntityType::Npc)
        .map(|(id, e)| (*id, e.position.length_squared()))
        .collect();

    // Sort by distance from origin (descending) — transfer peripheral entities first
    npc_ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Transfer up to 10% of our entities
    let transfer_count = (world.entities.len() / 10).max(1).min(npc_ids.len());

    let mut transferred = 0;
    for (id, _) in npc_ids.iter().take(transfer_count) {
        if let Some(mut entity) = world.entities.remove(id) {
            entity.authority_server = ServerId(target_server);

            let msg = PeerMessage::AuthorityTransfer {
                tick: world.tick,
                entity_id: *id,
                from_server: world.server_id,
                to_server: ServerId(target_server),
                full_state: entity,
            };

            if let Ok(bytes) = bincode::serialize(&msg) {
                if let Err(e) = udp_socket.send_to(&bytes, target_addr).await {
                    tracing::warn!("Failed to send transfer to S{target_server}: {e}");
                } else {
                    transferred += 1;
                }
            }
        }
    }

    if transferred > 0 {
        tracing::info!(
            "Transferred {transferred} entities to S{target_server} ({} remaining)",
            world.entities.len()
        );
    }
}

/// Drain all entities to target servers (round-robin). Called when draining.
async fn process_drain(world: &mut World, udp_socket: &UdpSocket) {
    let targets = match &world.drain_targets {
        Some(t) if !t.is_empty() => t.clone(),
        _ => return,
    };

    let entity_ids: Vec<EntityId> = world.entities.keys().copied().collect();
    let mut transferred = 0;

    for (i, id) in entity_ids.iter().enumerate() {
        let target_server = targets[i % targets.len()];
        let target_addr = match world.peer_addrs.get(&target_server) {
            Some(addr) => *addr,
            None => continue,
        };

        if let Some(mut entity) = world.entities.remove(id) {
            entity.authority_server = ServerId(target_server);
            let msg = PeerMessage::AuthorityTransfer {
                tick: world.tick,
                entity_id: *id,
                from_server: world.server_id,
                to_server: ServerId(target_server),
                full_state: entity,
            };
            if let Ok(bytes) = bincode::serialize(&msg) {
                let _ = udp_socket.send_to(&bytes, target_addr).await;
                transferred += 1;
            }
        }
    }

    if transferred > 0 {
        tracing::info!("Drained {transferred} entities ({} remaining)", world.entities.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::{EntityType, ServerId};

    fn make_world(server_id: u16) -> World {
        World::new(ServerId(server_id))
    }

    // -- process_input tests --

    #[test]
    fn process_input_spawn_npc() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::SpawnEntity {
            position: [10.0, 0.0, -20.0],
            entity_type: "npc".to_string(),
        });
        assert_eq!(w.entities.len(), 1);
        let e = w.entities.values().next().unwrap();
        assert_eq!(e.entity_type, EntityType::Npc);
        assert!((e.position.x - 10.0).abs() < 0.01);
    }

    #[test]
    fn process_input_spawn_player() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::SpawnEntity {
            position: [0.0, 0.0, 0.0],
            entity_type: "player".to_string(),
        });
        assert_eq!(w.entities.len(), 1);
        let e = w.entities.values().next().unwrap();
        assert_eq!(e.entity_type, EntityType::Player);
    }

    #[test]
    fn process_input_spawn_unknown_type_defaults_to_npc() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::SpawnEntity {
            position: [0.0, 0.0, 0.0],
            entity_type: "unknown_thing".to_string(),
        });
        let e = w.entities.values().next().unwrap();
        assert_eq!(e.entity_type, EntityType::Npc);
    }

    #[test]
    fn process_input_spawn_player_with_id() {
        let mut w = make_world(1);
        let uuid = Uuid::new_v4();
        process_input(&mut w, ClientMessage::SpawnPlayer {
            player_id: uuid.to_string(),
            position: [5.0, 0.0, 5.0],
        });
        assert_eq!(w.entities.len(), 1);
        let e = w.entities.values().next().unwrap();
        assert_eq!(e.id.0, uuid);
        assert_eq!(e.entity_type, EntityType::Player);
    }

    #[test]
    fn process_input_spawn_player_invalid_uuid_ignored() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::SpawnPlayer {
            player_id: "not-a-uuid".to_string(),
            position: [0.0, 0.0, 0.0],
        });
        assert_eq!(w.entities.len(), 0);
    }

    #[test]
    fn process_input_move_input() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Player);
        process_input(&mut w, ClientMessage::MoveInput {
            entity_id: id.0.to_string(),
            direction: [1.0, 0.0, 0.0],
        });
        let e = &w.entities[&id];
        assert!(e.velocity.x > 0.0);
    }

    #[test]
    fn process_input_move_input_invalid_uuid_ignored() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::MoveInput {
            entity_id: "bad".to_string(),
            direction: [1.0, 0.0, 0.0],
        });
        // No panic
    }

    #[test]
    fn process_input_remove_entity() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        assert_eq!(w.entities.len(), 1);
        process_input(&mut w, ClientMessage::RemoveEntity {
            entity_id: id.0.to_string(),
        });
        assert_eq!(w.entities.len(), 0);
    }

    #[test]
    fn process_input_remove_invalid_uuid_ignored() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::RemoveEntity {
            entity_id: "garbage".to_string(),
        });
        // No panic
    }

    #[test]
    fn process_input_request_snapshot_is_noop() {
        let mut w = make_world(1);
        process_input(&mut w, ClientMessage::RequestSnapshot);
        assert_eq!(w.entities.len(), 0);
    }

    // -- simulate_tick tests --

    #[test]
    fn simulate_tick_advances_tick() {
        let mut w = make_world(1);
        assert_eq!(w.tick, 0);
        simulate_tick(&mut w);
        assert_eq!(w.tick, 1);
        simulate_tick(&mut w);
        assert_eq!(w.tick, 2);
    }

    #[test]
    fn simulate_tick_integrates_position() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Player);
        w.apply_move_input(&id, Vec3::new(1.0, 0.0, 0.0)); // velocity = 5.0 * normalized
        let old_pos = w.entities[&id].position;

        simulate_tick(&mut w);

        let new_pos = w.entities[&id].position;
        // Position should have moved in +X direction
        assert!(new_pos.x > old_pos.x);
    }

    #[test]
    fn simulate_tick_clamps_to_world_bounds() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::new(499.0, 0.0, 0.0), EntityType::Npc);
        // Give a large velocity pushing past the boundary
        w.entities.get_mut(&id).unwrap().velocity = Vec3::new(100.0, 0.0, 0.0);

        // Run multiple ticks to push past boundary
        for _ in 0..100 {
            simulate_tick(&mut w);
        }

        assert!(w.entities[&id].position.x <= 500.0);
    }

    #[test]
    fn simulate_tick_npcs_eventually_change_direction() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);

        // Run enough ticks that the NPC should change direction at least once
        // Direction changes every 3 seconds (60 ticks at 20Hz)
        let mut velocities = Vec::new();
        for _ in 0..80 {
            simulate_tick(&mut w);
            velocities.push(w.entities[&id].velocity);
        }

        // Velocity should have changed at some point (NPC gets a random direction)
        // Since the phase depends on UUID, we can't predict when, but after 80 ticks
        // covering more than one full NPC_DIRECTION_CHANGE_INTERVAL, at least one change
        let first_nonzero = velocities.iter().find(|v| v.length() > 0.01);
        assert!(first_nonzero.is_some(), "NPC should have gained velocity after direction change");
    }

    #[test]
    fn simulate_tick_empty_world() {
        let mut w = make_world(1);
        simulate_tick(&mut w); // should not panic
        assert_eq!(w.tick, 1);
    }
}

