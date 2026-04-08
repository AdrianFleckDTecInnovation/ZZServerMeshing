use glam::Vec3;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(pub Uuid);

impl EntityId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerId(pub u16);

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityType {
    Player,
    Npc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    pub id: EntityId,
    pub position: Vec3,
    pub velocity: Vec3,
    pub entity_type: EntityType,
    pub authority_server: ServerId,
}

impl EntityState {
    pub fn new_npc(position: Vec3, authority_server: ServerId) -> Self {
        Self {
            id: EntityId::new(),
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server,
        }
    }

    pub fn new_player(position: Vec3, authority_server: ServerId) -> Self {
        Self {
            id: EntityId::new(),
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server,
        }
    }
}
