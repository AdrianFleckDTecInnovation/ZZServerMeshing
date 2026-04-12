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
    /// Last acknowledged input sequence number (only meaningful for players, 0 for NPCs).
    #[serde(default)]
    pub last_input_seq: u32,
}

impl EntityState {
    pub fn new_npc(position: Vec3, authority_server: ServerId) -> Self {
        Self {
            id: EntityId::new(),
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server,
            last_input_seq: 0,
        }
    }

    pub fn new_player(position: Vec3, authority_server: ServerId) -> Self {
        Self {
            id: EntityId::new(),
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server,
            last_input_seq: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_uniqueness() {
        let a = EntityId::new();
        let b = EntityId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn entity_id_display() {
        let id = EntityId::new();
        let s = format!("{}", id);
        assert_eq!(s.len(), 36); // UUID string length
    }

    #[test]
    fn server_id_display() {
        let s = ServerId(3);
        assert_eq!(format!("{}", s), "S3");
    }

    #[test]
    fn new_npc_defaults() {
        let pos = Vec3::new(10.0, 0.0, -20.0);
        let npc = EntityState::new_npc(pos, ServerId(1));
        assert_eq!(npc.position, pos);
        assert_eq!(npc.velocity, Vec3::ZERO);
        assert_eq!(npc.entity_type, EntityType::Npc);
        assert_eq!(npc.authority_server, ServerId(1));
    }

    #[test]
    fn new_player_defaults() {
        let pos = Vec3::new(0.0, 0.0, 0.0);
        let player = EntityState::new_player(pos, ServerId(2));
        assert_eq!(player.entity_type, EntityType::Player);
        assert_eq!(player.authority_server, ServerId(2));
    }

    #[test]
    fn entity_state_json_roundtrip() {
        let e = EntityState::new_npc(Vec3::new(1.5, 0.0, -3.5), ServerId(1));
        let json = serde_json::to_string(&e).unwrap();
        let decoded: EntityState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, e.id);
        assert_eq!(decoded.position, e.position);
        assert_eq!(decoded.authority_server, e.authority_server);
    }

    #[test]
    fn entity_state_bincode_roundtrip() {
        let e = EntityState::new_player(Vec3::new(-100.0, 0.0, 250.0), ServerId(5));
        let bytes = bincode::serialize(&e).unwrap();
        let decoded: EntityState = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.id, e.id);
        assert_eq!(decoded.position, e.position);
        assert_eq!(decoded.entity_type, EntityType::Player);
    }

    #[test]
    fn entity_id_serde_transparent() {
        let id = EntityId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert!(json.starts_with('"'));
        let decoded: EntityId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn server_id_serde_transparent() {
        let sid = ServerId(42);
        let json = serde_json::to_string(&sid).unwrap();
        assert_eq!(json, "42");
        let decoded: ServerId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, sid);
    }

    // -- last_input_seq tests --

    #[test]
    fn last_input_seq_defaults_to_zero_json() {
        // Simulate old-format JSON without last_input_seq
        let json = r#"{"id":"550e8400-e29b-41d4-a716-446655440000","position":[1.0,0.0,2.0],"velocity":[0.0,0.0,0.0],"entity_type":"Npc","authority_server":1}"#;
        let decoded: EntityState = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.last_input_seq, 0);
    }

    #[test]
    fn last_input_seq_roundtrip_json() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = 999;
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("999"));
        let decoded: EntityState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.last_input_seq, 999);
    }

    #[test]
    fn last_input_seq_roundtrip_bincode() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = 12345;
        let bytes = bincode::serialize(&e).unwrap();
        let decoded: EntityState = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.last_input_seq, 12345);
    }

    #[test]
    fn new_npc_has_zero_last_input_seq() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        assert_eq!(e.last_input_seq, 0);
    }

    #[test]
    fn new_player_has_zero_last_input_seq() {
        let e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        assert_eq!(e.last_input_seq, 0);
    }
}
