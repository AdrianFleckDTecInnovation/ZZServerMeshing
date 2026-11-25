//! Integration tests for the input → entity → snapshot pipeline.
//! Tests cross-module flows without spawning real network listeners.

use glam::Vec3;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::protocol::{ClientMessage, PeerMessage};

#[test]
fn local_worker_runs_the_network_servers_schedule() {
    let mut worker = sim_server::DemoWorker::new(ServerId(1), 8, 3, true);
    worker.step();
    assert_eq!(worker.world.resource::<sim_server::resources::TickCounter>().0, 1);
    assert_eq!(worker.world.resource::<sim_server::resources::EntityLookup>().0.len(), 3);
}

#[test]
fn move_input_json_roundtrip_with_seq() {
    let eid = EntityId(uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap());
    let msg = ClientMessage::MoveInput {
        entity_id: eid,
        direction: [1.0, 0.0, -0.5],
        seq: 17,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    match decoded {
        ClientMessage::MoveInput {
            entity_id,
            direction,
            seq,
        } => {
            assert_eq!(entity_id, eid);
            assert_eq!(direction, [1.0, 0.0, -0.5]);
            assert_eq!(seq, 17);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn move_input_without_seq_defaults_to_zero() {
    // Simulates an old client that doesn't send seq
    let json = r#"{"type":"MoveInput","entity_id":"550e8400-e29b-41d4-a716-446655440000","direction":[1.0,0.0,0.0]}"#;
    let decoded: ClientMessage = serde_json::from_str(json).unwrap();
    match decoded {
        ClientMessage::MoveInput { seq, .. } => assert_eq!(seq, 0),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn forwarded_input_bincode_preserves_seq() {
    let eid = EntityId::new();
    let msg = PeerMessage::ForwardedInput {
        session_id: 0,
        entity_id: eid,
        direction: [1.0, 0.0, 0.0],
        seq: 42,
    };
    let bytes = bincode::serialize(&msg).unwrap();
    let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
    match decoded {
        PeerMessage::ForwardedInput {
            entity_id,
            direction,
            seq,
            ..
        } => {
            assert_eq!(entity_id, eid);
            assert_eq!(direction, [1.0, 0.0, 0.0]);
            assert_eq!(seq, 42);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn entity_state_last_input_seq_survives_bincode_replication() {
    // Simulates: server sets seq → bincode serialize → UDP → bincode deserialize
    let mut player = EntityState::new_player(Vec3::new(10.0, 0.0, 20.0), ServerId(1));
    player.last_input_seq = 100;

    let msg = PeerMessage::EntityStateUpdate {
        tick: 500,
        server_id: ServerId(1),
        session_id: 0,
        entities: vec![player.clone()],
    };

    let bytes = bincode::serialize(&msg).unwrap();
    let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
    match decoded {
        PeerMessage::EntityStateUpdate { entities, .. } => {
            assert_eq!(entities.len(), 1);
            assert_eq!(entities[0].last_input_seq, 100);
            assert_eq!(entities[0].position, Vec3::new(10.0, 0.0, 20.0));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn authority_transfer_preserves_last_input_seq() {
    let mut player = EntityState::new_player(Vec3::new(5.0, 0.0, 5.0), ServerId(1));
    player.last_input_seq = 77;
    let eid = player.id;

    let msg = PeerMessage::AuthorityTransfer {
        tick: 300,
        session_id: 0,
        entity_id: eid,
        from_server: ServerId(1),
        to_server: ServerId(2),
        full_state: player,
    };

    let bytes = bincode::serialize(&msg).unwrap();
    let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
    match decoded {
        PeerMessage::AuthorityTransfer { full_state, .. } => {
            assert_eq!(full_state.last_input_seq, 77);
            assert_eq!(full_state.authority_server, ServerId(1)); // caller mutates before sending
        }
        _ => panic!("wrong variant"),
    }
}
