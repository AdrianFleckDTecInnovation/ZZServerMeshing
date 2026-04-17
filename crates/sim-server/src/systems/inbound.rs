use crate::components::*;
use crate::conversion::{despawn_and_remove, spawn_from_entity_state_direct};
use crate::game_loop::serialize_versioned;
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::EntityType;
use shared::protocol::PeerMessage;

/// Process inbound events from async tasks: peer updates, authority transfers, forwarded inputs.
pub fn drain_inbound_events(world: &mut World) {
    let _tick = world.resource::<TickCounter>().0;
    let server_id = world.resource::<ServerConfig>().server_id;

    // Take all queued events
    let peer_updates = std::mem::take(&mut world.resource_mut::<InboundQueue>().peer_updates);
    let authority_transfers =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().authority_transfers);
    let forwarded_inputs =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().forwarded_inputs);
    let forwarded_removes =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().forwarded_removes);

    // -- Process peer updates (shadow entities) --
    for update in peer_updates {
        for entity_state in &update.entities {
            let eid = entity_state.id;

            if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&eid) {
                // Entity already exists
                if world.get::<Authoritative>(bevy_entity).is_some() {
                    // Don't overwrite our authoritative entity
                    continue;
                }
                if let Some(mut shadow) = world.get_mut::<Shadow>(bevy_entity) {
                    // Update existing shadow if tick is newer
                    if update.tick < shadow.last_seen_tick {
                        continue; // stale packet
                    }
                    shadow.last_seen_tick = update.tick;
                    if let Some(mut pos) = world.get_mut::<Position>(bevy_entity) {
                        pos.0 = entity_state.position;
                    }
                    if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                        vel.0 = entity_state.velocity;
                    }
                    if let Some(mut auth) = world.get_mut::<AuthorityServer>(bevy_entity) {
                        auth.0 = entity_state.authority_server;
                    }
                    // Propagate last_input_seq for player shadows so
                    // client snapshots reflect the authority's ack
                    if let Some(mut seq) = world.get_mut::<LastInputSeq>(bevy_entity) {
                        seq.0 = entity_state.last_input_seq;
                    }
                    continue;
                }
            }

            // Entity not in lookup -- spawn as shadow
            spawn_from_entity_state_direct(world, entity_state, false, Some(update.tick));
        }
    }

    // -- Process authority transfers --
    let current_tick = world.resource::<TickCounter>().0;
    for transfer in authority_transfers {
        if transfer.to_server != server_id {
            tracing::warn!(
                "Ignoring transfer for {} targeted at {}, we are {}",
                transfer.entity_id,
                transfer.to_server,
                server_id
            );
            continue;
        }

        let eid = transfer.entity_id;

        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&eid) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                tracing::warn!(
                    "Ignoring duplicate transfer for {} from {}",
                    eid,
                    transfer.from_server
                );
                // Still send ACK for duplicate -- source needs it even if we already have the entity
                super::input::send_transfer_ack(world, eid, current_tick, transfer.from_server);
                continue;
            }

            // Promote shadow to authoritative
            if world.get::<Shadow>(bevy_entity).is_some() {
                // Remove Shadow, add Authoritative
                world.entity_mut(bevy_entity).remove::<Shadow>();
                world.entity_mut(bevy_entity).insert(Authoritative);

                // Update all components from full_state
                if let Some(mut pos) = world.get_mut::<Position>(bevy_entity) {
                    pos.0 = transfer.full_state.position;
                }
                if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                    vel.0 = transfer.full_state.velocity;
                }
                if let Some(mut auth) = world.get_mut::<AuthorityServer>(bevy_entity) {
                    auth.0 = transfer.full_state.authority_server;
                }
                if let Some(mut kind) = world.get_mut::<EntityKind>(bevy_entity) {
                    kind.0 = transfer.full_state.entity_type;
                }
                if transfer.full_state.entity_type == EntityType::Player {
                    world
                        .entity_mut(bevy_entity)
                        .insert(LastInputSeq(transfer.full_state.last_input_seq));
                }

                tracing::info!(
                    entity = %eid,
                    from = transfer.from_server.0,
                    to = transfer.to_server.0,
                    "Accepted authority transfer (promoted shadow)"
                );
                super::input::send_transfer_ack(world, eid, current_tick, transfer.from_server);
                continue;
            }
        }

        // Entity not found at all -- spawn as authoritative
        spawn_from_entity_state_direct(world, &transfer.full_state, true, None);
        tracing::info!(
            entity = %eid,
            from = transfer.from_server.0,
            to = transfer.to_server.0,
            "Accepted authority transfer (new entity)"
        );
        super::input::send_transfer_ack(world, eid, current_tick, transfer.from_server);
    }

    // -- Process forwarded inputs --
    for fwd in forwarded_inputs {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&fwd.entity_id) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                // If entity is being transferred, re-forward to the target
                if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                    let target_addr = outbound.target_addr;
                    let target_server = outbound.target_server;
                    let fwd_msg = PeerMessage::ForwardedInput {
                        entity_id: fwd.entity_id,
                        direction: fwd.direction,
                        seq: fwd.seq,
                    };
                    if let Some(bytes) = serialize_versioned(&fwd_msg) {
                        world
                            .resource_mut::<OutboundForwards>()
                            .messages
                            .push((target_addr, bytes));
                    }
                    tracing::debug!(
                        entity = %fwd.entity_id,
                        target = target_server,
                        seq = fwd.seq,
                        "Re-forwarding input to transfer target"
                    );
                    continue;
                }

                let dir = Vec3::new(fwd.direction[0], fwd.direction[1], fwd.direction[2]);
                let kind = world.get::<EntityKind>(bevy_entity).map(|k| k.0);
                let speed = match kind {
                    Some(EntityType::Player) => 5.0,
                    _ => 2.0,
                };
                if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                    vel.0 = dir.normalize_or_zero() * speed;
                }
                if let Some(mut seq) = world.get_mut::<LastInputSeq>(bevy_entity) {
                    seq.0 = fwd.seq;
                }
                tracing::debug!(
                    entity = %fwd.entity_id,
                    seq = fwd.seq,
                    "Applied forwarded input"
                );
            } else {
                tracing::debug!(
                    "Received forwarded input for {} but not authoritative",
                    fwd.entity_id
                );
            }
        }
    }

    // -- Process forwarded removes --
    for fwd_rm in forwarded_removes {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&fwd_rm.entity_id) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                despawn_and_remove(world, bevy_entity, fwd_rm.entity_id);
                tracing::info!(
                    entity = %fwd_rm.entity_id,
                    "Removed entity via forwarded remove"
                );
            } else {
                tracing::debug!(
                    entity = %fwd_rm.entity_id,
                    "Forwarded remove for non-authoritative entity, ignoring"
                );
            }
        } else {
            tracing::debug!(
                entity = %fwd_rm.entity_id,
                "Forwarded remove for unknown entity, ignoring"
            );
        }
    }
}

/// Drain client registration/unregistration events from InboundQueue into ClientRegistry.
/// Runs after drain_inbound_events, before process_client_inputs.
pub fn drain_client_events(
    mut inbound: ResMut<InboundQueue>,
    mut registry: ResMut<ClientRegistry>,
) {
    // Register new clients
    let registrations = std::mem::take(&mut inbound.register_clients);
    for event in registrations {
        registry.register_from_event(event.client_id, event.tx, event.acked_tick);
        // Also push client_player_assignment so the player_id can be set
        // once SpawnPlayer is processed (matched by player_uuid).
        inbound.client_player_assignments.push((
            event.client_id,
            event.player_uuid,
        ));
        tracing::debug!(client_id = event.client_id, "Registered client in ECS");
    }

    // Unregister disconnected clients
    let unregistrations = std::mem::take(&mut inbound.unregister_clients);
    for client_id in unregistrations {
        registry.unregister(client_id);
        tracing::debug!(client_id, "Unregistered client from ECS");
    }

    // Apply player ID assignments
    let assignments = std::mem::take(&mut inbound.client_player_assignments);
    for (client_id, entity_id) in assignments {
        if let Some(handle) = registry.clients.get_mut(&client_id) {
            handle.player_id = Some(entity_id);
        }
    }

    // Handle forced full snapshot requests
    let force_fulls = std::mem::take(&mut inbound.force_full_snapshot_clients);
    for client_id in force_fulls {
        if let Some(handle) = registry.clients.get_mut(&client_id) {
            handle.baseline = None;
            handle.ticks_since_full = 0;
            tracing::debug!(client_id, "Forced full snapshot for client");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use shared::entity::{EntityId, EntityState, EntityType, ServerId};
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

    fn register_test_client(world: &mut World, client_id: u64) -> tokio::sync::mpsc::Receiver<ClientFrame> {
        use std::sync::atomic::AtomicU64;
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = std::sync::Arc::new(AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>()
            .register_from_event(client_id, tx, acked);
        rx
    }

    // -- drain_inbound_events tests --

    #[test]
    fn peer_update_creates_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![state],
        });

        drain_inbound_events(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn peer_update_does_not_overwrite_authoritative() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        let mut shadow_state = EntityState::new_npc(Vec3::new(999.0, 0.0, 999.0), ServerId(2));
        shadow_state.id = eid;
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![shadow_state],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // unchanged
    }

    #[test]
    fn peer_update_stale_tick_rejected() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        let mut stale = state.clone();
        stale.position = Vec3::new(100.0, 0.0, 0.0);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 5, // older than 10
            entities: vec![stale],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // not updated
    }

    #[test]
    fn peer_update_newer_tick_updates_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(5));

        let mut newer = state.clone();
        newer.position = Vec3::new(100.0, 0.0, 0.0);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10, // newer than 5
            entities: vec![newer],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::new(100.0, 0.0, 0.0));
    }

    #[test]
    fn authority_transfer_creates_authoritative() {
        let mut world = setup_world(2);
        let eid = EntityId(uuid::Uuid::new_v4());
        let state = EntityState {
            id: eid,
            position: Vec3::new(10.0, 0.0, 10.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
    }

    #[test]
    fn authority_transfer_promotes_shadow() {
        let mut world = setup_world(2);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        let transfer_state = EntityState {
            id: eid,
            position: Vec3::new(50.0, 0.0, 50.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: transfer_state,
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
        assert!(world.get::<Shadow>(bevy_entity).is_none());
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::new(50.0, 0.0, 50.0));
    }

    #[test]
    fn authority_transfer_wrong_server_rejected() {
        let mut world = setup_world(2);
        let eid = EntityId(uuid::Uuid::new_v4());
        let state = EntityState {
            id: eid,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(3),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(3), // not us (2)
            full_state: state,
        });

        drain_inbound_events(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn authority_transfer_duplicate_rejected() {
        let mut world = setup_world(2);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 2);

        let state = EntityState {
            id: eid,
            position: Vec3::new(999.0, 0.0, 999.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // unchanged
    }

    #[test]
    fn forwarded_input_applies() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 5,
            }
        );

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!(vel.0.x > 0.0);
        assert_eq!(world.get::<LastInputSeq>(entity).unwrap().0, 5);
    }

    #[test]
    fn forwarded_input_non_authoritative_ignored() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 5,
            }
        );

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO);
    }

    // -- Shadow update boundary: equal tick --

    #[test]
    fn peer_update_equal_tick_accepted() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        let mut updated = state.clone();
        updated.position = Vec3::new(100.0, 0.0, 0.0);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10, // equal to last_seen_tick
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::new(100.0, 0.0, 0.0), "equal tick should be accepted (not rejected)");
    }

    #[test]
    fn peer_update_strictly_older_tick_rejected() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        let mut stale = state.clone();
        stale.position = Vec3::new(100.0, 0.0, 0.0);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 9, // strictly less than 10
            entities: vec![stale],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO, "strictly older tick should be rejected");
    }

    // -- Authority transfer promotes shadow with player LastInputSeq --

    #[test]
    fn authority_transfer_promotes_shadow_player_with_seq() {
        let mut world = setup_world(2);
        let state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        let transfer_state = EntityState {
            id: eid,
            position: Vec3::new(50.0, 0.0, 50.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server: ServerId(2),
            last_input_seq: 42,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: transfer_state,
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert_eq!(world.get::<LastInputSeq>(bevy_entity).unwrap().0, 42,
            "player shadow promoted via transfer should have last_input_seq set");
    }

    // -- Forwarded input speed: player vs npc --

    #[test]
    fn forwarded_input_player_speed_is_5() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput { entity_id: eid, direction: [1.0, 0.0, 0.0], seq: 1 }
        );
        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!((vel.0.x - 5.0).abs() < 0.01, "player speed should be 5.0");
    }

    #[test]
    fn forwarded_input_npc_speed_is_2() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput { entity_id: eid, direction: [1.0, 0.0, 0.0], seq: 1 }
        );
        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!((vel.0.x - 2.0).abs() < 0.01, "npc speed should be 2.0");
    }

    // -- Forwarded input for unknown entity --

    #[test]
    fn forwarded_input_unknown_entity_ignored() {
        let mut world = setup_world(1);
        let unknown_eid = EntityId(uuid::Uuid::new_v4());

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput { entity_id: unknown_eid, direction: [1.0, 0.0, 0.0], seq: 1 }
        );

        // Should not panic
        drain_inbound_events(&mut world);
    }

    // -- Multiple peer updates in single tick --

    #[test]
    fn multiple_peer_updates_same_tick() {
        let mut world = setup_world(1);
        let s1 = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let s2 = EntityState::new_npc(Vec3::X, ServerId(2));
        let eid1 = s1.id;
        let eid2 = s2.id;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![s1, s2],
        });

        drain_inbound_events(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid1));
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid2));
    }

    // -- Peer update: multiple entities in single update --

    #[test]
    fn peer_update_batch_creates_all_shadows() {
        let mut world = setup_world(1);
        let entities: Vec<EntityState> = (0..5)
            .map(|i| EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(2)))
            .collect();
        let eids: Vec<EntityId> = entities.iter().map(|e| e.id).collect();

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities,
        });

        drain_inbound_events(&mut world);

        for eid in eids {
            assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        }
    }

    // -- Peer update: updates velocity on existing shadow --

    #[test]
    fn peer_update_updates_shadow_velocity() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(5));

        let mut updated = state.clone();
        updated.velocity = Vec3::new(5.0, 0.0, 3.0);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let vel = world.get::<Velocity>(bevy_entity).unwrap();
        assert_eq!(vel.0, Vec3::new(5.0, 0.0, 3.0));
    }

    // -- Peer update: updates authority_server and last_input_seq on existing shadow --

    #[test]
    fn peer_update_updates_shadow_last_input_seq() {
        let mut world = setup_world(1);
        let state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(5));

        let mut updated = state.clone();
        updated.last_input_seq = 42;
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert_eq!(world.get::<LastInputSeq>(bevy_entity).unwrap().0, 42);
    }

    #[test]
    fn peer_update_updates_shadow_authority() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(5));

        let mut updated = state.clone();
        updated.authority_server = ServerId(3);
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert_eq!(world.get::<AuthorityServer>(bevy_entity).unwrap().0, ServerId(3));
    }

    // -- TransferAck tests (related to drain_inbound_events authority transfer handling) --

    #[test]
    fn transfer_ack_sent_on_accept() {
        let mut world = setup_world(2);
        // Set up peer addresses so ACK can be sent
        world.resource_mut::<PeerAddresses>().0.insert(1, "127.0.0.1:9101".parse().unwrap());

        let eid = EntityId(uuid::Uuid::new_v4());
        let state = EntityState {
            id: eid,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // Check that a TransferAck was queued
        let transfers = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(transfers.len(), 1, "should have one outbound TransferAck");
        let (addr, bytes) = &transfers[0];
        assert_eq!(*addr, "127.0.0.1:9101".parse::<std::net::SocketAddr>().unwrap());
        let msg = deserialize_versioned(bytes);
        match msg {
            PeerMessage::TransferAck { entity_id, .. } => assert_eq!(entity_id, eid),
            _ => panic!("expected TransferAck, got {:?}", msg),
        }
    }

    #[test]
    fn transfer_ack_skipped_when_source_not_in_peers() {
        let mut world = setup_world(2);
        // Do NOT set up peer address for server 1

        let eid = EntityId(uuid::Uuid::new_v4());
        let state = EntityState {
            id: eid,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // No outbound messages (source not in peer list)
        assert!(world.resource::<OutboundTransfers>().messages.is_empty(),
            "should skip TransferAck when source not in peer list");
    }

    #[test]
    fn transfer_ack_sent_on_duplicate_transfer() {
        let mut world = setup_world(2);
        world.resource_mut::<PeerAddresses>().0.insert(1, "127.0.0.1:9101".parse().unwrap());

        // Spawn auth entity first (simulating already-accepted transfer)
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 2);

        let state = EntityState {
            id: eid,
            position: Vec3::new(999.0, 0.0, 999.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // ACK should still be sent for duplicate
        assert_eq!(world.resource::<OutboundTransfers>().messages.len(), 1,
            "should send TransferAck even for duplicate transfer");
    }

    // -- Forwarded input for transfer outbound --

    #[test]
    fn forwarded_input_for_transfer_outbound_is_re_forwarded() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Mark as transfer outbound
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 5,
            }
        );

        drain_inbound_events(&mut world);

        // Velocity should not change (forwarded to target)
        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "should not process input locally");

        // Should have produced a forward message
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should forward to transfer target");
    }

    // -- Forwarded remove tests --

    #[test]
    fn forwarded_remove_despawns_authoritative_entity() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            crate::resources::ForwardedRemove { entity_id: eid }
        );

        drain_inbound_events(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid),
            "authoritative entity should be despawned by forwarded remove");
    }

    #[test]
    fn forwarded_remove_for_nonexistent_entity_is_noop() {
        let mut world = setup_world(1);
        let unknown_eid = EntityId(uuid::Uuid::new_v4());

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            crate::resources::ForwardedRemove { entity_id: unknown_eid }
        );

        // Should not panic
        drain_inbound_events(&mut world);
    }

    #[test]
    fn forwarded_remove_for_shadow_entity_is_ignored() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            crate::resources::ForwardedRemove { entity_id: eid }
        );

        drain_inbound_events(&mut world);

        // Shadow should NOT be removed by forwarded remove (only authority processes removes)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid),
            "forwarded remove should not despawn shadow entities");
    }

    // -- drain_client_events tests --

    #[test]
    fn drain_client_events_registers_client() {
        let mut world = setup_world(1);
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        world.resource_mut::<InboundQueue>().register_clients.push(
            ClientRegistrationEvent {
                client_id: 42,
                tx,
                acked_tick: acked,
                player_uuid: shared::entity::EntityId::new(),
            }
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(world.resource::<ClientRegistry>().clients.contains_key(&42));
    }

    #[test]
    fn drain_client_events_unregisters_client() {
        let mut world = setup_world(1);
        let _rx = register_test_client(&mut world, 42);

        world.resource_mut::<InboundQueue>().unregister_clients.push(42);

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(!world.resource::<ClientRegistry>().clients.contains_key(&42));
    }

    #[test]
    fn drain_client_events_force_full_clears_baseline() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 42);

        // Run one tick to establish baseline
        let mut snap_schedule = Schedule::default();
        snap_schedule.add_systems(crate::systems::prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        snap_schedule.run(&mut world);
        rx.try_recv().unwrap(); // consume

        // Verify baseline exists
        assert!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().baseline.is_some());

        // Force full snapshot
        world.resource_mut::<InboundQueue>().force_full_snapshot_clients.push(42);
        let mut drain_schedule = Schedule::default();
        drain_schedule.add_systems(drain_client_events);
        drain_schedule.run(&mut world);

        // Baseline should be cleared
        assert!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().baseline.is_none());
        assert_eq!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().ticks_since_full, 0);
    }
}
