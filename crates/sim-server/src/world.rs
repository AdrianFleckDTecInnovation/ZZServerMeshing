use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use shared::spatial::{clamp_to_world, WORLD_EXTENT};
use glam::Vec3;
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;

pub struct World {
    pub server_id: ServerId,
    pub tick: u64,
    /// Entities this server is authoritative for.
    pub entities: HashMap<EntityId, EntityState>,
    /// Read-only copies of entities from peer servers.
    pub shadow_entities: HashMap<EntityId, EntityState>,
    /// Tick at which each shadow was last updated (for stale cleanup).
    pub shadow_last_seen: HashMap<EntityId, u64>,
    /// UDP addresses of peer servers.
    pub peer_addrs: HashMap<u16, SocketAddr>,
    /// Pending rebalance: (entity_id strings, target_server_id). Set by coordinator client.
    pub pending_transfers: Option<(Vec<String>, u16)>,
    /// Graceful drain mode: transfer all entities and exit.
    pub draining: bool,
    pub drain_targets: Option<Vec<u16>>,
}

/// Max ticks before a shadow entity is considered stale and removed.
const SHADOW_STALE_TICKS: u64 = shared::tick::TICK_RATE as u64 * 5; // 5 seconds

impl World {
    pub fn new(server_id: ServerId) -> Self {
        Self {
            server_id,
            tick: 0,
            entities: HashMap::new(),
            shadow_entities: HashMap::new(),
            shadow_last_seen: HashMap::new(),
            peer_addrs: HashMap::new(),
            pending_transfers: None,
            draining: false,
            drain_targets: None,
        }
    }

    pub fn spawn_initial_npcs(&mut self, count: u32) {
        let mut rng = rand::thread_rng();
        for _ in 0..count {
            let pos = Vec3::new(
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
                0.0,
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
            );
            let entity = EntityState::new_npc(pos, self.server_id);
            self.entities.insert(entity.id, entity);
        }
    }

    pub fn spawn_entity(&mut self, position: Vec3, entity_type: EntityType) -> EntityId {
        let entity = match entity_type {
            EntityType::Player => EntityState::new_player(position, self.server_id),
            EntityType::Npc => EntityState::new_npc(position, self.server_id),
        };
        let id = entity.id;
        self.entities.insert(id, entity);
        id
    }

    /// Spawn a player with a specific UUID (assigned by the gateway).
    pub fn spawn_player_with_id(&mut self, uuid: uuid::Uuid, position: Vec3) -> EntityId {
        let id = EntityId(uuid);
        let entity = EntityState {
            id,
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server: self.server_id,
        };
        self.entities.insert(id, entity);
        id
    }

    pub fn remove_entity(&mut self, id: &EntityId) {
        self.entities.remove(id);
    }

    pub fn apply_move_input(&mut self, entity_id: &EntityId, direction: Vec3) {
        if let Some(entity) = self.entities.get_mut(entity_id) {
            let speed = match entity.entity_type {
                EntityType::Player => 5.0,
                EntityType::Npc => 2.0,
            };
            entity.velocity = direction.normalize_or_zero() * speed;
        }
    }

    /// Returns only authoritative entities (for WebSocket broadcast).
    pub fn get_snapshot(&self) -> Vec<EntityState> {
        self.entities.values().cloned().collect()
    }

    /// Apply incoming shadow entity data from a peer server.
    /// Discards stale updates (lower tick than what we already have).
    pub fn apply_peer_update(&mut self, tick: u64, entities: Vec<EntityState>) {
        for entity in entities {
            // Don't overwrite our own authoritative entities
            if self.entities.contains_key(&entity.id) {
                continue;
            }
            // Discard stale packets (out-of-order UDP)
            if let Some(&last_tick) = self.shadow_last_seen.get(&entity.id) {
                if tick < last_tick {
                    continue;
                }
            }
            self.shadow_last_seen.insert(entity.id, tick);
            self.shadow_entities.insert(entity.id, entity);
        }
    }

    /// Remove stale shadow entities.
    pub fn cleanup_shadows(&mut self) {
        let tick = self.tick;
        let stale: Vec<EntityId> = self
            .shadow_last_seen
            .iter()
            .filter(|(_, &last)| tick.saturating_sub(last) > SHADOW_STALE_TICKS)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.shadow_entities.remove(&id);
            self.shadow_last_seen.remove(&id);
        }
    }

    pub fn clamp_all_positions(&mut self) {
        for entity in self.entities.values_mut() {
            entity.position = clamp_to_world(entity.position);
        }
    }

    /// Recover orphaned entities from a dead server by promoting shadows to authoritative.
    /// Recover entities from a dead server.
    /// `survivor_ids` is the sorted list of all surviving server IDs.
    /// Each survivor only adopts entities whose UUID hashes to its position
    /// in the survivor list, preventing duplicate adoption.
    pub fn recover_from_dead_server(&mut self, dead_server_id: u16, survivor_ids: &[u16]) {
        let my_index = survivor_ids.iter().position(|&id| id == self.server_id.0);
        let my_index = match my_index {
            Some(i) => i,
            None => return, // we're not in the survivor list
        };
        let survivor_count = survivor_ids.len();

        let to_adopt: Vec<EntityId> = self
            .shadow_entities
            .iter()
            .filter(|(_, e)| e.authority_server.0 == dead_server_id)
            .filter(|(id, _)| {
                // Distribute entities across survivors by UUID hash
                (id.0.as_u128() as usize) % survivor_count == my_index
            })
            .map(|(id, _)| *id)
            .collect();

        let mut adopted = 0;
        for id in to_adopt {
            if let Some(mut entity) = self.shadow_entities.remove(&id) {
                entity.authority_server = self.server_id;
                self.entities.insert(id, entity);
                self.shadow_last_seen.remove(&id);
                adopted += 1;
            }
        }

        if adopted > 0 {
            tracing::info!("Recovered {} entities from dead S{}", adopted, dead_server_id);
        }
    }
}
