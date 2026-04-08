use crate::entity::{EntityId, EntityState, ServerId};
use serde::{Deserialize, Serialize};

// ── Server → Browser Client (JSON over WebSocket) ──

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

// ── Browser Client / Gateway → Server (JSON over WebSocket) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    SpawnEntity {
        position: [f32; 3],
        entity_type: String,
    },
    /// Gateway assigns a specific UUID for a player entity.
    SpawnPlayer {
        player_id: String,
        position: [f32; 3],
    },
    MoveInput {
        entity_id: String,
        direction: [f32; 3],
    },
    /// Gateway requests removal of a player entity on disconnect.
    RemoveEntity {
        entity_id: String,
    },
    RequestSnapshot,
}

// ── Sim-Server ↔ Sim-Server (bincode over UDP) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerMessage {
    EntityStateUpdate {
        tick: u64,
        server_id: ServerId,
        entities: Vec<EntityState>,
    },
    AuthorityTransfer {
        tick: u64,
        entity_id: EntityId,
        from_server: ServerId,
        to_server: ServerId,
        full_state: EntityState,
    },
}

// ── Sim-Server → Coordinator (JSON over WebSocket) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CoordClientMsg {
    Register {
        server_id: u16,
        udp_port: u16,
        ws_port: u16,
    },
    Heartbeat {
        server_id: u16,
        entity_count: u32,
    },
    /// Server wants to leave gracefully — drain all entities first.
    Draining {
        server_id: u16,
    },
}

// ── Coordinator → Sim-Server (JSON over WebSocket) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CoordServerMsg {
    PeerList { peers: Vec<PeerInfo> },
    /// Instruct a server to transfer entities to another server.
    RebalanceDirective {
        entity_ids: Vec<String>,
        target_server: u16,
    },
    /// Instruct a server to drain ALL entities to the listed targets, then exit.
    DrainDirective {
        target_servers: Vec<u16>,
    },
    /// A server died — adopt its orphaned shadow entities.
    RecoverEntities {
        dead_server_id: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub server_id: u16,
    pub udp_addr: String,
    pub ws_addr: String,
}
