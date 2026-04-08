use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use shared::spatial::{clamp_to_world, WORLD_EXTENT};
use glam::Vec3;
use rand::Rng;
use std::collections::HashMap;

pub struct World {
    pub server_id: ServerId,
    pub tick: u64,
    pub entities: HashMap<EntityId, EntityState>,
}

impl World {
    pub fn new(server_id: ServerId) -> Self {
        Self {
            server_id,
            tick: 0,
            entities: HashMap::new(),
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

    pub fn get_snapshot(&self) -> Vec<EntityState> {
        self.entities.values().cloned().collect()
    }

    pub fn clamp_all_positions(&mut self) {
        for entity in self.entities.values_mut() {
            entity.position = clamp_to_world(entity.position);
        }
    }
}
