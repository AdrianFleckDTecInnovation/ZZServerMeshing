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
    /// Sent to a client on connect to assign their player entity.
    PlayerAssigned {
        player_id: String,
    },
    /// Instruct the client to reconnect to a different sim-server.
    Reconnect {
        url: String,
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
        #[serde(default)]
        seq: u32,
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
    /// Forward a client input to the authority peer when a sim-server
    /// receives a MoveInput for an entity it doesn't own.
    ForwardedInput {
        entity_id: EntityId,
        direction: [f32; 3],
        seq: u32,
    },
    /// Forward a remove request to the authority peer when a client
    /// disconnects but their player entity was transferred to another server.
    ForwardedRemove {
        entity_id: EntityId,
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
        /// Advertised hostname for peer discovery (Docker/k8s networking).
        /// Falls back to the connection source IP when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// Whether this server's WebSocket listener uses TLS (wss://).
        #[serde(default)]
        tls: bool,
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
    /// `survivors` is the sorted list of surviving sim-server IDs so each
    /// server adopts a distinct subset (by UUID hash).
    RecoverEntities {
        dead_server_id: u16,
        #[serde(default)]
        survivors: Vec<u16>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub server_id: u16,
    pub udp_addr: String,
    pub ws_addr: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityState, ServerId};
    use glam::Vec3;

    #[test]
    fn server_message_world_snapshot_json_roundtrip() {
        let e = EntityState::new_npc(Vec3::new(1.0, 0.0, 2.0), ServerId(1));
        let msg = ServerMessage::WorldSnapshot {
            tick: 42,
            entities: vec![e],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"WorldSnapshot\""));
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ServerMessage::WorldSnapshot { tick, entities } => {
                assert_eq!(tick, 42);
                assert_eq!(entities.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_message_entity_delta_roundtrip() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let msg = ServerMessage::EntityDelta {
            tick: 100,
            updates: vec![e.clone()],
            removals: vec![e.id],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ServerMessage::EntityDelta { tick, updates, removals } => {
                assert_eq!(tick, 100);
                assert_eq!(updates.len(), 1);
                assert_eq!(removals.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_message_spawn_entity_roundtrip() {
        let msg = ClientMessage::SpawnEntity {
            position: [10.0, 0.0, -5.0],
            entity_type: "npc".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"SpawnEntity\""));
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ClientMessage::SpawnEntity { position, entity_type } => {
                assert_eq!(position, [10.0, 0.0, -5.0]);
                assert_eq!(entity_type, "npc");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_message_move_input_roundtrip() {
        let msg = ClientMessage::MoveInput {
            entity_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 42,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ClientMessage::MoveInput { entity_id, direction, seq } => {
                assert_eq!(entity_id, "550e8400-e29b-41d4-a716-446655440000");
                assert_eq!(direction, [1.0, 0.0, 0.0]);
                assert_eq!(seq, 42);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_message_request_snapshot() {
        let msg = ClientMessage::RequestSnapshot;
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ClientMessage::RequestSnapshot));
    }

    #[test]
    fn peer_message_bincode_roundtrip() {
        let e = EntityState::new_npc(Vec3::new(50.0, 0.0, -50.0), ServerId(1));
        let msg = PeerMessage::EntityStateUpdate {
            tick: 200,
            server_id: ServerId(1),
            entities: vec![e],
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::EntityStateUpdate { tick, server_id, entities } => {
                assert_eq!(tick, 200);
                assert_eq!(server_id, ServerId(1));
                assert_eq!(entities.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn peer_message_authority_transfer_bincode_roundtrip() {
        let e = EntityState::new_player(Vec3::new(10.0, 0.0, 20.0), ServerId(1));
        let eid = e.id;
        let msg = PeerMessage::AuthorityTransfer {
            tick: 300,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: e,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::AuthorityTransfer { tick, entity_id, from_server, to_server, full_state } => {
                assert_eq!(tick, 300);
                assert_eq!(entity_id, eid);
                assert_eq!(from_server, ServerId(1));
                assert_eq!(to_server, ServerId(2));
                assert_eq!(full_state.entity_type, crate::entity::EntityType::Player);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn peer_message_forwarded_input_bincode_roundtrip() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, -0.5],
            seq: 7,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ForwardedInput { entity_id, direction, seq } => {
                assert_eq!(entity_id, eid);
                assert_eq!(direction, [1.0, 0.0, -0.5]);
                assert_eq!(seq, 7);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_client_msg_register_roundtrip() {
        let msg = CoordClientMsg::Register {
            server_id: 1,
            udp_port: 9101,
            ws_port: 7878,
            host: Some("10.0.0.1".to_string()),
            tls: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Register\""));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Register { server_id, udp_port, ws_port, host, tls } => {
                assert_eq!(server_id, 1);
                assert_eq!(udp_port, 9101);
                assert_eq!(ws_port, 7878);
                assert_eq!(host, Some("10.0.0.1".to_string()));
                assert!(!tls);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_client_msg_register_host_optional() {
        // host field should be omitted when None
        let msg = CoordClientMsg::Register {
            server_id: 2,
            udp_port: 9102,
            ws_port: 7879,
            host: None,
            tls: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("host"));
        // Deserialize without host field
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Register { host, .. } => assert_eq!(host, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_client_msg_register_tls_defaults_to_false() {
        // Old-format JSON without tls field
        let json = r#"{"type":"Register","server_id":1,"udp_port":9101,"ws_port":7878}"#;
        let decoded: CoordClientMsg = serde_json::from_str(json).unwrap();
        match decoded {
            CoordClientMsg::Register { tls, .. } => assert!(!tls),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_client_msg_register_tls_true() {
        let msg = CoordClientMsg::Register {
            server_id: 1,
            udp_port: 9101,
            ws_port: 7878,
            host: None,
            tls: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"tls\":true"));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Register { tls, .. } => assert!(tls),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_peer_list_roundtrip() {
        let msg = CoordServerMsg::PeerList {
            peers: vec![
                PeerInfo { server_id: 1, udp_addr: "127.0.0.1:9101".into(), ws_addr: "127.0.0.1:7878".into() },
                PeerInfo { server_id: 2, udp_addr: "127.0.0.1:9102".into(), ws_addr: "127.0.0.1:7879".into() },
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::PeerList { peers } => assert_eq!(peers.len(), 2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_recover_entities_survivors_default() {
        // survivors should default to empty vec when not present in JSON
        let json = r#"{"type":"RecoverEntities","dead_server_id":3}"#;
        let decoded: CoordServerMsg = serde_json::from_str(json).unwrap();
        match decoded {
            CoordServerMsg::RecoverEntities { dead_server_id, survivors } => {
                assert_eq!(dead_server_id, 3);
                assert!(survivors.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn empty_world_snapshot() {
        let msg = ServerMessage::WorldSnapshot {
            tick: 0,
            entities: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ServerMessage::WorldSnapshot { tick, entities } => {
                assert_eq!(tick, 0);
                assert!(entities.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- seq field tests --

    #[test]
    fn move_input_seq_defaults_to_zero() {
        // Old-format JSON without seq field
        let json = r#"{"type":"MoveInput","entity_id":"550e8400-e29b-41d4-a716-446655440000","direction":[1.0,0.0,0.0]}"#;
        let decoded: ClientMessage = serde_json::from_str(json).unwrap();
        match decoded {
            ClientMessage::MoveInput { seq, .. } => assert_eq!(seq, 0),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn move_input_seq_preserved() {
        let msg = ClientMessage::MoveInput {
            entity_id: "test".to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 999,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("999"));
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ClientMessage::MoveInput { seq, .. } => assert_eq!(seq, 999),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn forwarded_input_seq_bincode_roundtrip() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, -0.5],
            seq: 42,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ForwardedInput { seq, .. } => assert_eq!(seq, 42),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn peer_message_forwarded_remove_bincode_roundtrip() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::ForwardedRemove { entity_id: eid };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ForwardedRemove { entity_id } => {
                assert_eq!(entity_id, eid);
            }
            _ => panic!("wrong variant"),
        }
    }
}
