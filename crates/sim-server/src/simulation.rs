use crate::world::World;
use shared::entity::{EntityType, EntityId};
use shared::protocol::ClientMessage;
use shared::tick::{TICK_DURATION_MS, TICK_RATE};
use glam::Vec3;
use rand::Rng;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// NPC direction change interval in ticks.
const NPC_DIRECTION_CHANGE_INTERVAL: u64 = (TICK_RATE as u64) * 3; // every 3 seconds

pub async fn run_game_loop(
    world: Arc<RwLock<World>>,
    mut input_rx: mpsc::UnboundedReceiver<ClientMessage>,
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

        // Simulate one tick
        let mut w = world.write().await;
        simulate_tick(&mut w);
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
        ClientMessage::MoveInput { entity_id, direction } => {
            if let Ok(uuid) = Uuid::parse_str(&entity_id) {
                let eid = EntityId(uuid);
                let dir = Vec3::new(direction[0], direction[1], direction[2]);
                world.apply_move_input(&eid, dir);
            }
        }
        ClientMessage::RequestSnapshot => {
            // Handled by the network layer, not here
        }
    }
}

fn simulate_tick(world: &mut World) {
    world.tick += 1;

    let tick = world.tick;
    let dt = TICK_DURATION.as_secs_f32();

    // Collect entity IDs that are NPCs for behavior update
    let npc_ids: Vec<EntityId> = world
        .entities
        .iter()
        .filter(|(_, e)| e.entity_type == EntityType::Npc)
        .map(|(id, _)| *id)
        .collect();

    // Update NPC behaviors: change direction periodically
    let mut rng = rand::thread_rng();
    for id in &npc_ids {
        if let Some(entity) = world.entities.get_mut(id) {
            // Change direction every N ticks, using entity hash for variety
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
            "Tick {} | {} entities",
            tick,
            world.entities.len()
        );
    }
}
