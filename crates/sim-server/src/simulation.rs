use crate::replication;
use crate::world::World;
use shared::entity::{EntityType, EntityId, ServerId};
use shared::protocol::{ClientMessage, PeerMessage};
use shared::tick::{TICK_DURATION_MS, TICK_RATE};
use glam::Vec3;
use rand::Rng;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// NPC direction change interval in ticks.
const NPC_DIRECTION_CHANGE_INTERVAL: u64 = (TICK_RATE as u64) * 3; // every 3 seconds

pub async fn run_game_loop(
    world: Arc<RwLock<World>>,
    mut input_rx: mpsc::UnboundedReceiver<ClientMessage>,
    udp_socket: Arc<UdpSocket>,
) {
    let mut interval = tokio::time::interval(TICK_DURATION);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::info!("Game loop started at {} Hz", TICK_RATE);

    loop {
        interval.tick().await;

        // Drain all pending inputs
        while let Ok(input) = input_rx.try_recv() {
            let mut w = world.write().await;
            process_input(&mut w, input);
        }

        // Simulate + replicate + handle transfers
        {
            let mut w = world.write().await;
            simulate_tick(&mut w);

            // Process pending authority transfers
            process_transfers(&mut w, &udp_socket).await;

            // Replicate authoritative entities to peers
            replication::replicate_to_peers(&w, &udp_socket).await;

            // Clean up stale shadow entities
            w.cleanup_shadows();
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
