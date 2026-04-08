use crate::entity::{EntityId, EntityState, ServerId};
use serde::{Deserialize, Serialize};

/// Messages sent from the server to browser clients (JSON over WebSocket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    WorldSnapshot {
        tick: u64,
        entities: Vec<EntityState>,
    },
    EntityDelta {
        tick: u64,
        updates: Vec<EntityState>,
        removals: Vec<EntityId>,
    },
}

/// Messages sent from browser clients to the server (JSON over WebSocket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    SpawnEntity {
        position: [f32; 3],
        entity_type: String,
    },
    MoveInput {
        entity_id: String,
        direction: [f32; 3],
    },
    RequestSnapshot,
}

/// Messages sent between sim-servers (bincode over UDP). Used in Phase 2+.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerMessage {
    EntityStateUpdate {
        tick: u64,
        server_id: ServerId,
        entities: Vec<EntityState>,
    },
    AuthorityTransfer {
        entity_id: EntityId,
        from_server: ServerId,
        to_server: ServerId,
        full_state: EntityState,
    },
    Heartbeat {
        server_id: ServerId,
        entity_count: u32,
        cpu_load: f32,
    },
}
