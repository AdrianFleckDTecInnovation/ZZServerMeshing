use crate::entity::{EntityId, EntityState, ServerId};
use serde::{Deserialize, Serialize};

// ── Server → Browser Client (JSON over WebSocket) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Sent to a client on connect to assign their player entity.
    PlayerAssigned {
        player_id: String,
        #[serde(default)]
        protocol_version: u16,
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
    /// Acknowledgement that an authority transfer was received and accepted.
    /// Sent by the target server back to the source server.
    TransferAck {
        entity_id: EntityId,
        tick: u64,
    },
    /// Chunked entity state update for large payloads that exceed safe UDP datagram size.
    /// The receiver reassembles chunks from the same tick before processing.
    ChunkedEntityUpdate {
        tick: u64,
        server_id: ServerId,
        chunk_index: u16,
        total_chunks: u16,
        entities: Vec<EntityState>,
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
        /// Wire protocol version. Coordinator logs mismatches.
        #[serde(default)]
        protocol_version: u16,
    },
    Heartbeat {
        server_id: u16,
        entity_count: u32,
        /// Weighted load: players count 3x, NPCs count 1x.
        /// Used by the coordinator for smarter rebalance decisions.
        #[serde(default)]
        weighted_load: u32,
    },
    /// Server wants to leave gracefully — drain all entities first.
    Draining {
        server_id: u16,
    },
    /// Atomically transfer player account ownership between servers.
    /// Sent after a two-phase authority transfer ACK confirms the entity arrived.
    PlayerTransfer {
        account_id: String,
        from_server: u16,
        to_server: u16,
    },
    /// Report the outcome of an authority transfer back to the coordinator.
    /// Enables closed-loop rebalancing (coordinator knows what happened).
    TransferResult {
        entity_id: String,
        target_server: u16,
        success: bool,
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
    /// Result of an atomic player transfer request.
    PlayerTransferResult {
        account_id: String,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Periodic keepalive from coordinator. Prevents sim-server read timeout
    /// on stable clusters where no topology changes occur.
    Keepalive,
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
            protocol_version: 1,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Register\""));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Register { server_id, udp_port, ws_port, host, tls, .. } => {
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
            protocol_version: 1,
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
            protocol_version: 1,
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

    // -- TransferAck tests (WI-001) --

    #[test]
    fn peer_message_transfer_ack_bincode_roundtrip() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::TransferAck {
            entity_id: eid,
            tick: 12345,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::TransferAck { entity_id, tick } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 12345);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- ChunkedEntityUpdate tests (WI-009) --

    #[test]
    fn peer_message_chunked_entity_update_bincode_roundtrip() {
        let entities: Vec<EntityState> = (0..10)
            .map(|_| EntityState::new_npc(Vec3::new(1.0, 0.0, 2.0), ServerId(1)))
            .collect();
        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 500,
            server_id: ServerId(1),
            chunk_index: 2,
            total_chunks: 5,
            entities,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ChunkedEntityUpdate {
                tick,
                server_id,
                chunk_index,
                total_chunks,
                entities,
            } => {
                assert_eq!(tick, 500);
                assert_eq!(server_id, ServerId(1));
                assert_eq!(chunk_index, 2);
                assert_eq!(total_chunks, 5);
                assert_eq!(entities.len(), 10);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn peer_message_chunked_entity_update_empty_entities() {
        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 1,
            server_id: ServerId(1),
            chunk_index: 0,
            total_chunks: 1,
            entities: vec![],
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ChunkedEntityUpdate {
                entities,
                chunk_index,
                total_chunks,
                ..
            } => {
                assert!(entities.is_empty());
                assert_eq!(chunk_index, 0);
                assert_eq!(total_chunks, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn peer_message_chunked_entity_update_chunk_fields_survive() {
        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 999,
            server_id: ServerId(7),
            chunk_index: 14,
            total_chunks: 20,
            entities: vec![],
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ChunkedEntityUpdate {
                chunk_index,
                total_chunks,
                ..
            } => {
                assert_eq!(chunk_index, 14);
                assert_eq!(total_chunks, 20);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- PlayerTransfer tests (WI-013) --

    #[test]
    fn coord_client_msg_player_transfer_roundtrip() {
        let msg = CoordClientMsg::PlayerTransfer {
            account_id: "player123".to_string(),
            from_server: 1,
            to_server: 2,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"PlayerTransfer\""));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::PlayerTransfer {
                account_id,
                from_server,
                to_server,
            } => {
                assert_eq!(account_id, "player123");
                assert_eq!(from_server, 1);
                assert_eq!(to_server, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_player_transfer_result_success() {
        let msg = CoordServerMsg::PlayerTransferResult {
            account_id: "player123".to_string(),
            success: true,
            error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("error")); // skip_serializing_if
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::PlayerTransferResult {
                account_id,
                success,
                error,
            } => {
                assert_eq!(account_id, "player123");
                assert!(success);
                assert!(error.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_player_transfer_result_failure() {
        let msg = CoordServerMsg::PlayerTransferResult {
            account_id: "player456".to_string(),
            success: false,
            error: Some("Account already reassigned".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("error"));
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::PlayerTransferResult {
                account_id,
                success,
                error,
            } => {
                assert_eq!(account_id, "player456");
                assert!(!success);
                assert_eq!(error, Some("Account already reassigned".to_string()));
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- PlayerTransferResult: error field defaults when missing from JSON --

    #[test]
    fn coord_server_msg_player_transfer_result_error_defaults() {
        // JSON without "error" field -- should default to None
        let json = r#"{"type":"PlayerTransferResult","account_id":"test","success":true}"#;
        let decoded: CoordServerMsg = serde_json::from_str(json).unwrap();
        match decoded {
            CoordServerMsg::PlayerTransferResult { error, success, .. } => {
                assert!(success);
                assert!(error.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- TransferAck: JSON roundtrip (cross-format verification) --

    #[test]
    fn peer_message_transfer_ack_json_roundtrip() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::TransferAck {
            entity_id: eid,
            tick: 42,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: PeerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            PeerMessage::TransferAck { entity_id, tick } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 42);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- ChunkedEntityUpdate: JSON roundtrip --

    #[test]
    fn peer_message_chunked_entity_update_json_roundtrip() {
        let entities: Vec<EntityState> = (0..3)
            .map(|_| EntityState::new_npc(Vec3::new(1.0, 0.0, 2.0), ServerId(1)))
            .collect();
        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 100,
            server_id: ServerId(2),
            chunk_index: 0,
            total_chunks: 3,
            entities,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: PeerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            PeerMessage::ChunkedEntityUpdate {
                tick,
                server_id,
                chunk_index,
                total_chunks,
                entities,
            } => {
                assert_eq!(tick, 100);
                assert_eq!(server_id, ServerId(2));
                assert_eq!(chunk_index, 0);
                assert_eq!(total_chunks, 3);
                assert_eq!(entities.len(), 3);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- PlayerTransfer: bincode roundtrip (coordinator messages also tested via bincode) --

    #[test]
    fn coord_client_msg_player_transfer_json_fields() {
        let msg = CoordClientMsg::PlayerTransfer {
            account_id: "abc-123".to_string(),
            from_server: 5,
            to_server: 10,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"PlayerTransfer\""));
        assert!(json.contains("\"account_id\":\"abc-123\""));
        assert!(json.contains("\"from_server\":5"));
        assert!(json.contains("\"to_server\":10"));
    }

    // -- TransferAck: edge case with tick=0 --

    #[test]
    fn peer_message_transfer_ack_tick_zero() {
        let eid = crate::entity::EntityId::new();
        let msg = PeerMessage::TransferAck {
            entity_id: eid,
            tick: 0,
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::TransferAck { entity_id, tick } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- ChunkedEntityUpdate: max u16 chunk indices --

    #[test]
    fn peer_message_chunked_entity_update_max_chunk_index() {
        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 1,
            server_id: ServerId(1),
            chunk_index: u16::MAX,
            total_chunks: u16::MAX,
            entities: vec![],
        };
        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::ChunkedEntityUpdate {
                chunk_index,
                total_chunks,
                ..
            } => {
                assert_eq!(chunk_index, u16::MAX);
                assert_eq!(total_chunks, u16::MAX);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- CoordServerMsg exhaustive serialization: every variant roundtrips --

    #[test]
    fn coord_server_msg_rebalance_directive_roundtrip() {
        let msg = CoordServerMsg::RebalanceDirective {
            entity_ids: vec!["uuid-1".into(), "uuid-2".into()],
            target_server: 3,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"RebalanceDirective\""));
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::RebalanceDirective { entity_ids, target_server } => {
                assert_eq!(entity_ids.len(), 2);
                assert_eq!(target_server, 3);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_drain_directive_roundtrip() {
        let msg = CoordServerMsg::DrainDirective {
            target_servers: vec![2, 3, 4],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"DrainDirective\""));
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::DrainDirective { target_servers } => {
                assert_eq!(target_servers, vec![2, 3, 4]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_server_msg_recover_entities_with_survivors_roundtrip() {
        let msg = CoordServerMsg::RecoverEntities {
            dead_server_id: 5,
            survivors: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::RecoverEntities { dead_server_id, survivors } => {
                assert_eq!(dead_server_id, 5);
                assert_eq!(survivors, vec![1, 2, 3]);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- CoordClientMsg exhaustive: Heartbeat and Draining --

    #[test]
    fn coord_client_msg_heartbeat_roundtrip() {
        let msg = CoordClientMsg::Heartbeat {
            server_id: 2,
            entity_count: 150,
            weighted_load: 180,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Heartbeat\""));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Heartbeat { server_id, entity_count, weighted_load } => {
                assert_eq!(server_id, 2);
                assert_eq!(entity_count, 150);
                assert_eq!(weighted_load, 180);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_client_msg_draining_roundtrip() {
        let msg = CoordClientMsg::Draining { server_id: 7 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Draining\""));
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::Draining { server_id } => {
                assert_eq!(server_id, 7);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- PlayerTransfer with mismatched from_server scenario --

    #[test]
    fn coord_client_msg_player_transfer_from_server_preserved() {
        // The coordinator should reject transfers where from_server doesn't match
        // the sending server, but the protocol message should still roundtrip.
        let msg = CoordClientMsg::PlayerTransfer {
            account_id: "test-player".to_string(),
            from_server: 99, // mismatched
            to_server: 2,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: CoordClientMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordClientMsg::PlayerTransfer { from_server, to_server, .. } => {
                assert_eq!(from_server, 99);
                assert_eq!(to_server, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- PlayerTransferResult with error message roundtrip --

    #[test]
    fn coord_server_msg_player_transfer_result_error_message_roundtrip() {
        let msg = CoordServerMsg::PlayerTransferResult {
            account_id: "p-123".to_string(),
            success: false,
            error: Some("from_server mismatch".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("from_server mismatch"));
        let decoded: CoordServerMsg = serde_json::from_str(&json).unwrap();
        match decoded {
            CoordServerMsg::PlayerTransferResult { success, error, .. } => {
                assert!(!success);
                assert_eq!(error, Some("from_server mismatch".to_string()));
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- ServerMessage: PlayerAssigned and Reconnect roundtrips --

    #[test]
    fn server_message_player_assigned_roundtrip() {
        let msg = ServerMessage::PlayerAssigned {
            player_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            protocol_version: 1,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"PlayerAssigned\""));
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ServerMessage::PlayerAssigned { player_id, .. } => {
                assert_eq!(player_id, "550e8400-e29b-41d4-a716-446655440000");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_message_reconnect_roundtrip() {
        let msg = ServerMessage::Reconnect {
            url: "wss://10.0.0.5:7879".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Reconnect\""));
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ServerMessage::Reconnect { url } => {
                assert_eq!(url, "wss://10.0.0.5:7879");
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- ClientMessage: RemoveEntity and SpawnPlayer roundtrips --

    #[test]
    fn client_message_remove_entity_roundtrip() {
        let msg = ClientMessage::RemoveEntity {
            entity_id: "some-uuid".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"RemoveEntity\""));
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ClientMessage::RemoveEntity { entity_id } => {
                assert_eq!(entity_id, "some-uuid");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_message_spawn_player_roundtrip() {
        let msg = ClientMessage::SpawnPlayer {
            player_id: "p-uuid".to_string(),
            position: [1.0, 2.0, 3.0],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"SpawnPlayer\""));
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ClientMessage::SpawnPlayer { player_id, position } => {
                assert_eq!(player_id, "p-uuid");
                assert_eq!(position, [1.0, 2.0, 3.0]);
            }
            _ => panic!("wrong variant"),
        }
    }

    // -- All PeerMessage variants: verify bincode discriminants don't collide --

    #[test]
    fn all_peer_message_variants_distinct_bincode() {
        let eid = crate::entity::EntityId::new();
        let entity = EntityState::new_npc(Vec3::ZERO, ServerId(1));

        let messages: Vec<PeerMessage> = vec![
            PeerMessage::EntityStateUpdate {
                tick: 1,
                server_id: ServerId(1),
                entities: vec![entity.clone()],
            },
            PeerMessage::AuthorityTransfer {
                tick: 1,
                entity_id: eid,
                from_server: ServerId(1),
                to_server: ServerId(2),
                full_state: entity.clone(),
            },
            PeerMessage::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
            PeerMessage::ForwardedRemove { entity_id: eid },
            PeerMessage::TransferAck { entity_id: eid, tick: 1 },
            PeerMessage::ChunkedEntityUpdate {
                tick: 1,
                server_id: ServerId(1),
                chunk_index: 0,
                total_chunks: 1,
                entities: vec![entity],
            },
        ];

        // Serialize all and verify first bytes (discriminant) are unique
        let mut discriminants = std::collections::HashSet::new();
        for msg in &messages {
            let bytes = bincode::serialize(msg).unwrap();
            // bincode encodes enum variant index as u32 LE
            let disc = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            assert!(discriminants.insert(disc),
                "duplicate bincode discriminant {} for {:?}", disc, msg);
        }
        assert_eq!(discriminants.len(), 6);
    }
}
