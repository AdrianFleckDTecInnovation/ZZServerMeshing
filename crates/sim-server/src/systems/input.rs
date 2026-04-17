use crate::components::*;
use crate::conversion::{despawn_and_remove, spawn_from_entity_state_direct};
use crate::game_loop::serialize_versioned;
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use shared::protocol::{ClientMessage, PeerMessage};

/// Send a TransferAck back to the source server of an authority transfer.
/// Looks up the source server's UDP address in PeerAddresses. If the source
/// is not in the peer list (e.g. it crashed), skips silently.
pub(super) fn send_transfer_ack(
    world: &mut World,
    entity_id: EntityId,
    tick: u64,
    from_server: ServerId,
) {
    if let Some(&source_addr) = world.resource::<PeerAddresses>().0.get(&from_server.0) {
        let ack_msg = PeerMessage::TransferAck { entity_id, tick };
        if let Some(bytes) = serialize_versioned(&ack_msg) {
            world
                .resource_mut::<OutboundTransfers>()
                .messages
                .push((source_addr, bytes));
            tracing::debug!(
                entity = %entity_id,
                to = from_server.0,
                "Sent TransferAck"
            );
        }
    } else {
        tracing::debug!(
            entity = %entity_id,
            from = from_server.0,
            "Skipping TransferAck: source server not in peer list"
        );
    }
}

/// Process client inputs from WebSocket connections.
pub fn process_client_inputs(world: &mut World) {
    let server_id = world.resource::<ServerConfig>().server_id;
    let client_inputs = std::mem::take(&mut world.resource_mut::<InboundQueue>().client_inputs);

    for (client_id, input) in client_inputs {
        match input {
            ClientMessage::SpawnEntity {
                position,
                entity_type,
            } => {
                let pos = Vec3::new(position[0], position[1], position[2]);
                let etype = match entity_type.as_str() {
                    "player" => EntityType::Player,
                    _ => EntityType::Npc,
                };
                let state = match etype {
                    EntityType::Player => EntityState::new_player(pos, server_id),
                    EntityType::Npc => EntityState::new_npc(pos, server_id),
                };
                let eid = state.id;
                spawn_from_entity_state_direct(world, &state, true, None);
                // Track entity ownership for RemoveEntity validation
                world.resource_mut::<EntityOwnership>().owners.insert(eid, client_id);
                tracing::debug!("Spawned {:?} entity {}", etype, eid);
            }
            ClientMessage::SpawnPlayer {
                player_id,
                position,
            } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&player_id) {
                    let pos = Vec3::new(position[0], position[1], position[2]);
                    let id = EntityId(uuid);
                    let state = EntityState {
                        id,
                        position: pos,
                        velocity: Vec3::ZERO,
                        entity_type: EntityType::Player,
                        authority_server: server_id,
                        last_input_seq: 0,
                    };
                    spawn_from_entity_state_direct(world, &state, true, None);
                    // Track player entity ownership
                    world.resource_mut::<EntityOwnership>().owners.insert(id, client_id);
                    tracing::info!("Spawned player {}", id);
                }
            }
            ClientMessage::MoveInput {
                entity_id,
                direction,
                seq,
            } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&entity_id) {
                    let eid = EntityId(uuid);
                    let dir = Vec3::new(direction[0], direction[1], direction[2]);

                    if let Some(&bevy_entity) =
                        world.resource::<EntityLookup>().0.get(&eid)
                    {
                        if world.get::<Authoritative>(bevy_entity).is_some() {
                            // Check if entity is being transferred -- forward to target instead
                            if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                                let target_addr = outbound.target_addr;
                                let target_server = outbound.target_server;
                                let fwd_msg = PeerMessage::ForwardedInput {
                                    entity_id: eid,
                                    direction,
                                    seq,
                                };
                                if let Some(bytes) = serialize_versioned(&fwd_msg) {
                                    world
                                        .resource_mut::<OutboundForwards>()
                                        .messages
                                        .push((target_addr, bytes));
                                }
                                tracing::debug!(
                                    entity = %eid,
                                    target = target_server,
                                    seq = seq,
                                    "Forwarding input to transfer target"
                                );
                                continue;
                            }

                            // Process locally
                            let kind = world.get::<EntityKind>(bevy_entity).map(|k| k.0);
                            let speed = match kind {
                                Some(EntityType::Player) => 5.0,
                                _ => 2.0,
                            };
                            if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                                vel.0 = dir.normalize_or_zero() * speed;
                            }
                            if let Some(mut input_seq) =
                                world.get_mut::<LastInputSeq>(bevy_entity)
                            {
                                input_seq.0 = seq;
                            }
                            tracing::debug!(
                                entity = %eid,
                                seq = seq,
                                "MoveInput processed"
                            );
                            continue;
                        }

                        // Shadow -- produce forward
                        if let Some(shadow_auth) =
                            world.get::<AuthorityServer>(bevy_entity)
                        {
                            if world.get::<Shadow>(bevy_entity).is_some() {
                                let target_server = shadow_auth.0 .0;
                                if let Some(&peer_addr) = world
                                    .resource::<PeerAddresses>()
                                    .0
                                    .get(&target_server)
                                {
                                    let msg = PeerMessage::ForwardedInput {
                                        entity_id: eid,
                                        direction,
                                        seq,
                                    };
                                    if let Some(bytes) = serialize_versioned(&msg) {
                                        world
                                            .resource_mut::<OutboundForwards>()
                                            .messages
                                            .push((peer_addr, bytes));
                                    }
                                }
                                tracing::debug!(
                                    entity = %eid,
                                    target = target_server,
                                    seq = seq,
                                    "Forwarding input to authority"
                                );
                                continue;
                            }
                        }
                    }
                    tracing::debug!("MoveInput for unknown entity {}", eid);
                }
            }
            ClientMessage::RemoveEntity { entity_id } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&entity_id) {
                    let eid = EntityId(uuid);
                    // Ownership check: only the client that spawned an entity can remove it.
                    // client_id 0 means server-initiated (always allowed).
                    if client_id != 0 {
                        if let Some(&owner) = world.resource::<EntityOwnership>().owners.get(&eid) {
                            if owner != client_id {
                                tracing::warn!(
                                    entity = %eid,
                                    client_id,
                                    owner,
                                    "RemoveEntity rejected: client doesn't own entity"
                                );
                                continue;
                            }
                        }
                        // Entities not in the ownership map (e.g., adopted via crash recovery,
                        // transferred from another server) can be removed by anyone — they
                        // have no original client owner on this server.
                    }
                    if let Some(&bevy_entity) =
                        world.resource::<EntityLookup>().0.get(&eid)
                    {
                        if world.get::<Authoritative>(bevy_entity).is_some() {
                            // If entity is mid-transfer, forward the remove to the target
                            // server so it doesn't become permanently orphaned there.
                            if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                                let target_addr = outbound.target_addr;
                                let target_server = outbound.target_server;
                                let msg = PeerMessage::ForwardedRemove {
                                    entity_id: eid,
                                };
                                if let Some(bytes) = serialize_versioned(&msg) {
                                    world
                                        .resource_mut::<OutboundForwards>()
                                        .messages
                                        .push((target_addr, bytes));
                                }
                                tracing::info!(
                                    entity = %eid,
                                    target = target_server,
                                    "Forwarding remove to transfer target before local despawn"
                                );
                            }
                            // Case A: We are authoritative -- remove locally
                            despawn_and_remove(world, bevy_entity, eid);
                            tracing::info!(entity = %eid, "Removed authoritative entity on disconnect");
                        } else if world.get::<Shadow>(bevy_entity).is_some() {
                            // Case B: Entity was transferred -- forward remove to authority
                            if let Some(shadow_auth) =
                                world.get::<AuthorityServer>(bevy_entity)
                            {
                                let target_server = shadow_auth.0 .0;
                                if let Some(&peer_addr) = world
                                    .resource::<PeerAddresses>()
                                    .0
                                    .get(&target_server)
                                {
                                    let msg = PeerMessage::ForwardedRemove {
                                        entity_id: eid,
                                    };
                                    if let Some(bytes) = serialize_versioned(&msg) {
                                        world
                                            .resource_mut::<OutboundForwards>()
                                            .messages
                                            .push((peer_addr, bytes));
                                    }
                                    tracing::info!(
                                        entity = %eid,
                                        target = target_server,
                                        "Forwarding remove to authority server"
                                    );
                                } else {
                                    tracing::warn!(
                                        entity = %eid,
                                        target = target_server,
                                        "Cannot forward remove: no peer address for authority server"
                                    );
                                }
                            }
                            // Also remove the local shadow copy
                            despawn_and_remove(world, bevy_entity, eid);
                        }
                    }
                }
            }
            ClientMessage::RequestSnapshot => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use shared::protocol::PeerMessage;

    /// Deserialize a versioned UDP message (skip 2-byte version prefix).
    fn deserialize_versioned(bytes: &[u8]) -> PeerMessage {
        assert!(bytes.len() >= 2, "versioned message too short");
        let version = u16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(version, shared::PROTOCOL_VERSION, "unexpected protocol version in test");
        bincode::deserialize(&bytes[2..]).unwrap()
    }

    fn setup_world(server_id: u16) -> World {
        let mut world = World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(server_id),
        });
        world.init_resource::<TickCounter>();
        world.init_resource::<PeerAddresses>();
        world.init_resource::<EntityLookup>();
        world.init_resource::<InboundQueue>();
        world.init_resource::<CoordEventQueue>();
        world.init_resource::<OutboundReplication>();
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();
        world.init_resource::<ClientRegistry>();
        world.init_resource::<OutboundCoordMessages>();
        world.init_resource::<EntityOwnership>();
        world
    }

    fn spawn_auth_npc(world: &mut World, pos: Vec3, server_id: u16) -> (Entity, EntityId) {
        let state = EntityState::new_npc(pos, ServerId(server_id));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, true, None);
        (entity, eid)
    }

    fn spawn_auth_player(world: &mut World, pos: Vec3, server_id: u16) -> (Entity, EntityId) {
        let state = EntityState::new_player(pos, ServerId(server_id));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, true, None);
        (entity, eid)
    }

    fn spawn_shadow_npc(world: &mut World, pos: Vec3, owner_server: u16) -> (Entity, EntityId) {
        let state = EntityState::new_npc(pos, ServerId(owner_server));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, false, Some(0));
        (entity, eid)
    }

    // -- process_client_inputs tests --

    #[test]
    fn client_spawn_npc() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::SpawnEntity {
            position: [10.0, 0.0, 20.0],
            entity_type: "npc".into(),
        }));

        process_client_inputs(&mut world);

        let mut query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(query.iter(&world).count(), 1);
    }

    #[test]
    fn client_spawn_player_type() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::SpawnEntity {
            position: [0.0, 0.0, 0.0],
            entity_type: "player".into(),
        }));

        process_client_inputs(&mut world);

        let mut query = world.query::<&EntityKind>();
        let kind = query.iter(&world).next().unwrap();
        assert_eq!(kind.0, EntityType::Player);
    }

    #[test]
    fn client_spawn_player_with_uuid() {
        let mut world = setup_world(1);
        let uuid = uuid::Uuid::new_v4();
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::SpawnPlayer {
            player_id: uuid.to_string(),
            position: [5.0, 0.0, 5.0],
        }));

        process_client_inputs(&mut world);

        let eid = EntityId(uuid);
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn client_move_input_sets_velocity() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        }));

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!(vel.0.x > 0.0); // moved
    }

    #[test]
    fn client_move_for_shadow_produces_forward() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 2);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        }));

        process_client_inputs(&mut world);

        assert!(!world.resource::<OutboundForwards>().messages.is_empty());
    }

    #[test]
    fn client_remove_entity() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn client_request_snapshot_noop() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RequestSnapshot));
        // Should not panic
        process_client_inputs(&mut world);
    }

    // -- Client input: MoveInput direction normalization --

    #[test]
    fn client_move_input_normalizes_direction() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Unnormalized direction (length > 1)
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [3.0, 0.0, 4.0], // length = 5
            seq: 1,
        }));

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        // Speed should be 5.0 (player), direction should be normalized
        assert!((vel.0.length() - 5.0).abs() < 0.01, "velocity magnitude should be player speed 5.0");
    }

    // -- Client input: zero direction gives zero velocity --

    #[test]
    fn client_move_input_zero_direction() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [0.0, 0.0, 0.0],
            seq: 1,
        }));

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "zero direction should give zero velocity");
    }

    // -- Client input: invalid UUID ignored --

    #[test]
    fn client_move_input_invalid_uuid_ignored() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: "not-a-uuid".into(),
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        }));

        // Should not panic
        process_client_inputs(&mut world);
    }

    #[test]
    fn client_remove_invalid_uuid_ignored() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: "not-a-uuid".into(),
        }));

        process_client_inputs(&mut world);
    }

    #[test]
    fn client_remove_nonexistent_entity_noop() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: uuid::Uuid::new_v4().to_string(),
        }));

        process_client_inputs(&mut world);
    }

    // -- RemoveEntity during transfer forwards remove to target (#040) --

    #[test]
    fn client_remove_during_transfer_forwards_to_target() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Mark entity as mid-transfer
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 10,
            retry_count: 0,
        });

        // Client sends RemoveEntity
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        // Entity should be despawned locally
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // A ForwardedRemove should have been sent to the target
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should forward remove to transfer target");
        assert_eq!(forwards[0].0, "127.0.0.1:9102".parse::<std::net::SocketAddr>().unwrap());

        // Verify it deserializes to ForwardedRemove
        let msg: PeerMessage = deserialize_versioned(&forwards[0].1);
        match msg {
            PeerMessage::ForwardedRemove { entity_id } => {
                assert_eq!(entity_id, eid);
            }
            _ => panic!("expected ForwardedRemove, got {:?}", msg),
        }
    }

    #[test]
    fn client_remove_without_transfer_does_not_forward() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // No TransferOutbound -- normal remove
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        // Entity should be despawned locally
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // No forward should be sent
        assert!(world.resource::<OutboundForwards>().messages.is_empty());
    }

    // -- Heartbeat count excludes frozen entities (#041) --

    #[test]
    fn heartbeat_count_excludes_transfer_outbound() {
        let mut world = setup_world(1);

        // Spawn 5 auth entities
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        // Mark 2 as mid-transfer
        let mut query = world.query_filtered::<Entity, With<Authoritative>>();
        let entities: Vec<Entity> = query.iter(&world).collect();
        for entity in entities.iter().take(2) {
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 1,
                retry_count: 0,
            });
        }

        // Simulate the heartbeat count query from game_loop.rs
        let auth_count = world
            .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
            .iter(&world)
            .count();

        // Should be 3, not 5 (excludes the 2 frozen entities)
        assert_eq!(auth_count, 3);
    }

    // -- Client SpawnPlayer with invalid UUID --

    #[test]
    fn client_spawn_player_invalid_uuid_ignored() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::SpawnPlayer {
            player_id: "not-a-uuid".into(),
            position: [0.0, 0.0, 0.0],
        }));

        // Should not panic
        process_client_inputs(&mut world);

        let mut query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(query.iter(&world).count(), 0, "invalid UUID should not spawn entity");
    }

    // -- MoveInput for transfer outbound forwarded to target --

    #[test]
    fn move_input_for_transfer_outbound_is_forwarded() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Mark as transfer outbound to server 2
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 10,
            retry_count: 0,
        });

        // Client sends MoveInput for this entity
        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 5,
        }));

        process_client_inputs(&mut world);

        // Should not modify velocity locally
        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "TransferOutbound entity should not have velocity changed locally");

        // Should have produced a forward message
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should forward input to transfer target");
        assert_eq!(forwards[0].0, "127.0.0.1:9102".parse::<std::net::SocketAddr>().unwrap());

        let msg = deserialize_versioned(&forwards[0].1);
        match msg {
            PeerMessage::ForwardedInput { entity_id, direction, seq } => {
                assert_eq!(entity_id, eid);
                assert_eq!(direction, [1.0, 0.0, 0.0]);
                assert_eq!(seq, 5);
            }
            _ => panic!("expected ForwardedInput"),
        }
    }

    // -- Remove shadow entity produces forwarded remove --

    #[test]
    fn remove_shadow_entity_produces_forwarded_remove() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 2);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        // Local shadow should be removed
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // ForwardedRemove should be sent to authority
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1);
        let msg = deserialize_versioned(&forwards[0].1);
        match msg {
            PeerMessage::ForwardedRemove { entity_id } => assert_eq!(entity_id, eid),
            _ => panic!("expected ForwardedRemove"),
        }
    }

    #[test]
    fn remove_authoritative_entity_does_not_forward() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
        assert!(world.resource::<OutboundForwards>().messages.is_empty(),
            "removing an authoritative entity should not forward");
    }

    // -- Remove shadow without peer address --

    #[test]
    fn remove_shadow_without_peer_address_still_removes_local() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 2);
        // Do NOT set up peer address for server 2

        world.resource_mut::<InboundQueue>().client_inputs.push((0, ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        }));

        process_client_inputs(&mut world);

        // Local shadow should still be removed even without peer address
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // No forward sent (no peer address)
        assert!(world.resource::<OutboundForwards>().messages.is_empty());
    }

    // -- Full ghost player scenario --

    #[test]
    fn full_ghost_player_scenario_via_schedule() {
        let mut world = setup_world(1);

        // 1. Spawn player entity via client input
        let player_uuid = uuid::Uuid::new_v4();
        world.resource_mut::<InboundQueue>().client_inputs.push((42, ClientMessage::SpawnPlayer {
            player_id: player_uuid.to_string(),
            position: [10.0, 0.0, 20.0],
        }));

        let mut schedule = Schedule::default();
        schedule.add_systems(process_client_inputs);
        schedule.run(&mut world);

        let player_eid = EntityId(player_uuid);
        assert!(world.resource::<EntityLookup>().0.contains_key(&player_eid));

        // 2. Move the player
        world.resource_mut::<InboundQueue>().client_inputs.push((42, ClientMessage::MoveInput {
            entity_id: player_uuid.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        }));
        schedule.run(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&player_eid];
        let vel = world.get::<Velocity>(bevy_entity).unwrap();
        assert!(vel.0.x > 0.0, "player should have moved");

        // 3. Spawn NPC
        world.resource_mut::<InboundQueue>().client_inputs.push((42, ClientMessage::SpawnEntity {
            position: [0.0, 0.0, 0.0],
            entity_type: "npc".into(),
        }));
        schedule.run(&mut world);

        let mut auth_count_query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(auth_count_query.iter(&world).count(), 2, "player + npc = 2 auth entities");

        // 4. Remove NPC by entity_id
        // Find the NPC entity ID
        let mut npc_eid = None;
        let mut npc_query = world.query::<(&NetworkId, &EntityKind)>();
        for (nid, kind) in npc_query.iter(&world) {
            if kind.0 == EntityType::Npc {
                npc_eid = Some(nid.0);
            }
        }
        let npc_eid = npc_eid.unwrap();

        world.resource_mut::<InboundQueue>().client_inputs.push((42, ClientMessage::RemoveEntity {
            entity_id: npc_eid.0.to_string(),
        }));
        schedule.run(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&npc_eid),
            "NPC should be removed");
        assert!(world.resource::<EntityLookup>().0.contains_key(&player_eid),
            "player should still exist");
    }

}
