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
    let local_tick = world.resource::<TickCounter>().0;
    let server_id = world.resource::<ServerConfig>().server_id;
    let session_id = world.resource::<SessionId>().0;

    // WI-135 / design 015: Drain peer session resets FIRST so any shadow
    // `last_peer_tick` values for a restarted peer are zeroed before we
    // process this tick's peer_updates. Without this ordering, post-restart
    // updates with low ticks would be rejected by the
    // `update.tick < shadow.last_peer_tick` guard below at the cost of one
    // additional tick of staleness. Note: `last_local_tick` is intentionally
    // left alone here — it lives in our clock domain, which is continuous
    // across peer restarts.
    let session_resets =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().peer_session_resets);
    for reset in session_resets {
        // Collect the Bevy entities (not the EntityIds) whose AuthorityServer
        // matches the reset target AND which are shadows (not authoritative on
        // this server — we don't touch entities we own).
        let mut affected: Vec<bevy_ecs::entity::Entity> = Vec::new();
        let mut query = world.query::<(bevy_ecs::entity::Entity, &AuthorityServer, &Shadow)>();
        for (entity, auth, _shadow) in query.iter(world) {
            if auth.0 == reset.server_id {
                affected.push(entity);
            }
        }
        let count = affected.len();
        for entity in affected {
            if let Some(mut shadow) = world.get_mut::<Shadow>(entity) {
                shadow.last_peer_tick = 0;
                // last_local_tick intentionally untouched — local clock domain
                // is continuous across peer session changes (design 015).
            }
        }
        tracing::info!(
            server_id = reset.server_id.0,
            new_session_id = format_args!("0x{:016x}", reset.new_session_id),
            shadows_reset = count,
            "Reset shadow last_peer_tick for restarted peer"
        );
    }

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
                    if update.tick < shadow.last_peer_tick {
                        continue; // stale packet
                    }
                    shadow.last_peer_tick = update.tick;
                    shadow.last_local_tick = local_tick;
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
            spawn_from_entity_state_direct(
                world,
                entity_state,
                false,
                Some((update.tick, local_tick)),
            );
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
                if let Some(mut kind) = world.get_mut::<EntityTypeComp>(bevy_entity) {
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
                        session_id,
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
                let kind = world.get::<EntityTypeComp>(bevy_entity).map(|k| k.0);
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
        registry.register_from_event_with_migrating(
            event.client_id,
            event.tx,
            event.acked_tick,
            event.migrating_out,
        );
        // Also push client_player_assignment so the player_id can be set
        // once SpawnPlayer is processed (matched by player_uuid).
        inbound
            .client_player_assignments
            .push((event.client_id, event.player_uuid));
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

    // Apply forced full snapshots (RequestSnapshot)
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
        assert_eq!(
            version,
            shared::PROTOCOL_VERSION,
            "unexpected protocol version in test"
        );
        bincode::deserialize(&bytes[2..]).unwrap()
    }

    fn setup_world(server_id: u16) -> World {
        let mut world = World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(server_id),
        });
        world.insert_resource(SessionId(0));
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
        let entity = spawn_from_entity_state_direct(world, &state, false, Some((0, 0)));
        (entity, eid)
    }

    fn register_test_client(
        world: &mut World,
        client_id: u64,
    ) -> tokio::sync::mpsc::Receiver<ClientFrame> {
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = Arc::new(AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(client_id, tx, acked);
        rx
    }

    // -- drain_inbound_events tests --

    #[test]
    fn peer_update_creates_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;

        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((10, 10)));

        let mut stale = state.clone();
        stale.position = Vec3::new(100.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((5, 5)));

        let mut newer = state.clone();
        newer.position = Vec3::new(100.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((1, 1)));

        let transfer_state = EntityState {
            id: eid,
            position: Vec3::new(50.0, 0.0, 50.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
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

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
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

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
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
            },
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
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some((1, 1)));

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 5,
            },
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((10, 10)));

        let mut updated = state.clone();
        updated.position = Vec3::new(100.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 10, // equal to last_peer_tick
                entities: vec![updated],
            });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(
            pos.0,
            Vec3::new(100.0, 0.0, 0.0),
            "equal tick should be accepted (not rejected)"
        );
    }

    #[test]
    fn peer_update_strictly_older_tick_rejected() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some((10, 10)));

        let mut stale = state.clone();
        stale.position = Vec3::new(100.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((1, 1)));

        let transfer_state = EntityState {
            id: eid,
            position: Vec3::new(50.0, 0.0, 50.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server: ServerId(2),
            last_input_seq: 42,
        };

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
                entity_id: eid,
                from_server: ServerId(1),
                to_server: ServerId(2),
                full_state: transfer_state,
            });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert_eq!(
            world.get::<LastInputSeq>(bevy_entity).unwrap().0,
            42,
            "player shadow promoted via transfer should have last_input_seq set"
        );
    }

    // -- Forwarded input speed: player vs npc --

    #[test]
    fn forwarded_input_player_speed_is_5() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
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
            crate::resources::ForwardedInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
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
            crate::resources::ForwardedInput {
                entity_id: unknown_eid,
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
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

        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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

        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate { tick: 10, entities });

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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((5, 5)));

        let mut updated = state.clone();
        updated.velocity = Vec3::new(5.0, 0.0, 3.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((5, 5)));

        let mut updated = state.clone();
        updated.last_input_seq = 42;
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((5, 5)));

        let mut updated = state.clone();
        updated.authority_server = ServerId(3);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 10,
                entities: vec![updated],
            });

        drain_inbound_events(&mut world);

        let bevy_entity = world.resource::<EntityLookup>().0[&eid];
        assert_eq!(
            world.get::<AuthorityServer>(bevy_entity).unwrap().0,
            ServerId(3)
        );
    }

    // -- TransferAck tests (related to drain_inbound_events authority transfer handling) --

    #[test]
    fn transfer_ack_sent_on_accept() {
        let mut world = setup_world(2);
        // Set up peer addresses so ACK can be sent
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(1, "127.0.0.1:9101".parse().unwrap());

        let eid = EntityId(uuid::Uuid::new_v4());
        let state = EntityState {
            id: eid,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2),
            last_input_seq: 0,
        };

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
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
        assert_eq!(
            *addr,
            "127.0.0.1:9101".parse::<std::net::SocketAddr>().unwrap()
        );
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

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
                entity_id: eid,
                from_server: ServerId(1),
                to_server: ServerId(2),
                full_state: state,
            });

        drain_inbound_events(&mut world);

        // No outbound messages (source not in peer list)
        assert!(
            world.resource::<OutboundTransfers>().messages.is_empty(),
            "should skip TransferAck when source not in peer list"
        );
    }

    #[test]
    fn transfer_ack_sent_on_duplicate_transfer() {
        let mut world = setup_world(2);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(1, "127.0.0.1:9101".parse().unwrap());

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

        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(InboundTransfer {
                entity_id: eid,
                from_server: ServerId(1),
                to_server: ServerId(2),
                full_state: state,
            });

        drain_inbound_events(&mut world);

        // ACK should still be sent for duplicate
        assert_eq!(
            world.resource::<OutboundTransfers>().messages.len(),
            1,
            "should send TransferAck even for duplicate transfer"
        );
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
            },
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

        world
            .resource_mut::<InboundQueue>()
            .forwarded_removes
            .push(crate::resources::ForwardedRemove { entity_id: eid });

        drain_inbound_events(&mut world);

        assert!(
            !world.resource::<EntityLookup>().0.contains_key(&eid),
            "authoritative entity should be despawned by forwarded remove"
        );
    }

    #[test]
    fn forwarded_remove_for_nonexistent_entity_is_noop() {
        let mut world = setup_world(1);
        let unknown_eid = EntityId(uuid::Uuid::new_v4());

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            crate::resources::ForwardedRemove {
                entity_id: unknown_eid,
            },
        );

        // Should not panic
        drain_inbound_events(&mut world);
    }

    #[test]
    fn forwarded_remove_for_shadow_entity_is_ignored() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 2);

        world
            .resource_mut::<InboundQueue>()
            .forwarded_removes
            .push(crate::resources::ForwardedRemove { entity_id: eid });

        drain_inbound_events(&mut world);

        // Shadow should NOT be removed by forwarded remove (only authority processes removes)
        assert!(
            world.resource::<EntityLookup>().0.contains_key(&eid),
            "forwarded remove should not despawn shadow entities"
        );
    }

    // -- drain_client_events tests --

    #[test]
    fn drain_client_events_registers_client() {
        use std::sync::atomic::AtomicU64;
        let mut world = setup_world(1);
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);

        world
            .resource_mut::<InboundQueue>()
            .register_clients
            .push(ClientRegistrationEvent {
                client_id: 42,
                tx,
                acked_tick: std::sync::Arc::new(AtomicU64::new(0)),
                player_uuid: shared::entity::EntityId::new(),
                migrating_out: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            });

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(world.resource::<ClientRegistry>().clients.contains_key(&42));
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
        assert!(world
            .resource::<ClientRegistry>()
            .clients
            .get(&42)
            .unwrap()
            .baseline
            .is_some());

        // Force full snapshot
        world
            .resource_mut::<InboundQueue>()
            .force_full_snapshot_clients
            .push(42);
        let mut drain_schedule = Schedule::default();
        drain_schedule.add_systems(drain_client_events);
        drain_schedule.run(&mut world);

        // Baseline should be cleared
        assert!(world
            .resource::<ClientRegistry>()
            .clients
            .get(&42)
            .unwrap()
            .baseline
            .is_none());
        assert_eq!(
            world
                .resource::<ClientRegistry>()
                .clients
                .get(&42)
                .unwrap()
                .ticks_since_full,
            0
        );
    }

    // -- WI-202: ClaimPlayer unit-level coverage --
    //
    // The full async ClaimPlayer flow (wait-for-message → poll auth_players
    // → adopt or fallback-spawn) lives in `network.rs::handle_client` and is
    // integration-tested via the WI-206 browser scenarios. These unit tests
    // cover the ECS-level pieces it composes: (a) registering a client with
    // a claimed player_uuid and NOT emitting SpawnPlayer binds the client to
    // the pre-existing entity, and (b) the `migrating_out` atomic is shared
    // end-to-end (ECS <-> async close handler).

    #[test]
    fn claim_player_adopts_existing_entity() {
        // Simulate the target-side adopt path: an auth-transferred Player
        // entity exists on this server, and a client registers with the
        // matching player_uuid. No SpawnPlayer is pushed — the client should
        // be bound to the pre-existing entity.
        let mut world = setup_world(1);
        // Spawn the Player entity first (simulating prior AuthorityTransfer).
        let (_, pid) = spawn_auth_player(&mut world, Vec3::new(10.0, 0.0, 20.0), 1);
        let pre_count = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        assert_eq!(pre_count, 1, "setup sanity");

        // Register the claiming client (simulating handle_client's post-
        // claim ECS push: register_clients only, NO SpawnPlayer).
        use std::sync::atomic::AtomicU64;
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        world
            .resource_mut::<InboundQueue>()
            .register_clients
            .push(ClientRegistrationEvent {
                client_id: 7,
                tx,
                acked_tick: std::sync::Arc::new(AtomicU64::new(0)),
                player_uuid: pid,
                migrating_out: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            });

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        // Client registered with the claimed player_id set.
        let reg = world.resource::<ClientRegistry>();
        let handle = reg.clients.get(&7).expect("client registered");
        assert_eq!(handle.player_id, Some(pid), "client adopted the claimed id");

        // Auth entity count unchanged: we did NOT spawn a second player.
        let post_count = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        assert_eq!(
            post_count, 1,
            "ClaimPlayer adopt path must not produce a second player entity"
        );
    }

    #[test]
    fn claim_player_migrating_flag_is_shared_with_client_handle() {
        // The `migrating_out` Arc<AtomicBool> passed in via the registration
        // event must be the SAME one stored on the ClientHandle. Otherwise,
        // flipping it from `process_transfers` (ECS side) wouldn't be
        // observable from the network close handler (async side).
        use std::sync::atomic::AtomicU64;
        let mut world = setup_world(1);
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let migrating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        world
            .resource_mut::<InboundQueue>()
            .register_clients
            .push(ClientRegistrationEvent {
                client_id: 9,
                tx,
                acked_tick: std::sync::Arc::new(AtomicU64::new(0)),
                player_uuid: EntityId::new(),
                migrating_out: std::sync::Arc::clone(&migrating),
            });

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        // Flip via the external handle (ECS-side path).
        migrating.store(true, std::sync::atomic::Ordering::Release);

        // ClientHandle must observe the flip via Arc sharing.
        let handle = world.resource::<ClientRegistry>().clients.get(&9).unwrap();
        assert!(
            handle
                .migrating_out
                .load(std::sync::atomic::Ordering::Acquire),
            "ClientHandle.migrating_out must point at the same atomic as the external handle"
        );
    }

    #[test]
    fn drain_client_events_unregisters_client() {
        let mut world = setup_world(1);
        let _rx = register_test_client(&mut world, 42);

        world
            .resource_mut::<InboundQueue>()
            .unregister_clients
            .push(42);

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(!world.resource::<ClientRegistry>().clients.contains_key(&42));
    }

    // =========================================================================
    // WI-135 / design 015: peer session reset zeroes Shadow.last_peer_tick
    // =========================================================================

    /// Spawn a shadow NPC owned by `owner_server` with a specific
    /// `last_peer_tick`. `last_local_tick` is seeded to 0 — callers that care
    /// about the local-tick value should use `spawn_from_entity_state_direct`
    /// directly. Returns the Bevy Entity handle and the EntityId.
    fn spawn_shadow_with_tick(
        world: &mut World,
        owner_server: u16,
        last_peer_tick: u64,
    ) -> (Entity, EntityId) {
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(owner_server));
        let eid = state.id;
        let entity =
            spawn_from_entity_state_direct(world, &state, false, Some((last_peer_tick, 0)));
        (entity, eid)
    }

    #[test]
    fn peer_session_reset_zeroes_matching_shadow_last_peer_tick() {
        // Spawn two shadows owned by server 2 with non-zero last_peer_tick,
        // push a PeerSessionReset, and verify both are zeroed. Also verify
        // that last_local_tick is left untouched (design 015).
        let mut world = setup_world(1);
        // Seed some local_tick context so we can assert it is NOT zeroed.
        world.resource_mut::<TickCounter>().0 = 12345;
        let (e1, _) = spawn_shadow_with_tick(&mut world, 2, 500);
        let (e2, _) = spawn_shadow_with_tick(&mut world, 2, 14730);
        // Force a non-zero last_local_tick on both so the post-reset
        // assertion is meaningful (spawn_shadow_with_tick seeds local=0).
        world.get_mut::<Shadow>(e1).unwrap().last_local_tick = 12000;
        world.get_mut::<Shadow>(e2).unwrap().last_local_tick = 12300;

        // Sanity: both shadows start with the expected last_peer_tick.
        assert_eq!(world.get::<Shadow>(e1).unwrap().last_peer_tick, 500);
        assert_eq!(world.get::<Shadow>(e2).unwrap().last_peer_tick, 14730);

        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 0xABCD_EF01_2345_6789,
            });

        drain_inbound_events(&mut world);

        assert_eq!(
            world.get::<Shadow>(e1).unwrap().last_peer_tick,
            0,
            "shadow for reset server should have last_peer_tick zeroed"
        );
        assert_eq!(
            world.get::<Shadow>(e2).unwrap().last_peer_tick,
            0,
            "all shadows for reset server should have last_peer_tick zeroed"
        );
        // Design 015 regression guard: session reset must not touch the
        // local-clock stamp, only the peer-tick stamp.
        assert_eq!(
            world.get::<Shadow>(e1).unwrap().last_local_tick,
            12000,
            "session reset must not zero last_local_tick"
        );
        assert_eq!(
            world.get::<Shadow>(e2).unwrap().last_local_tick,
            12300,
            "session reset must not zero last_local_tick"
        );
    }

    #[test]
    fn peer_session_reset_only_affects_target_servers_shadows() {
        // Shadows for server 2 are zeroed; shadows for server 3 are untouched.
        let mut world = setup_world(1);
        let (e2a, _) = spawn_shadow_with_tick(&mut world, 2, 500);
        let (e3a, _) = spawn_shadow_with_tick(&mut world, 3, 700);
        let (e3b, _) = spawn_shadow_with_tick(&mut world, 3, 800);

        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 0x1234_5678_9ABC_DEF0,
            });

        drain_inbound_events(&mut world);

        assert_eq!(
            world.get::<Shadow>(e2a).unwrap().last_peer_tick,
            0,
            "server 2 shadow should be reset"
        );
        assert_eq!(
            world.get::<Shadow>(e3a).unwrap().last_peer_tick,
            700,
            "server 3 shadow must not be touched by a server 2 reset"
        );
        assert_eq!(
            world.get::<Shadow>(e3b).unwrap().last_peer_tick,
            800,
            "server 3 shadow must not be touched by a server 2 reset"
        );
    }

    #[test]
    fn peer_session_reset_does_not_touch_authoritative_entities() {
        // Authoritative entities owned by this server (on any server_id) must
        // never be modified by a peer session reset — even if the target
        // server_id happens to match the Authoritative entity's AuthorityServer
        // (which would be a bug state, but let's be defensive).
        let mut world = setup_world(2);
        // An authoritative entity on server 2 (ourselves).
        let (auth_ent, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 2);
        // The auth entity has no Shadow component, so the query cannot match it.

        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 0xAA,
            });

        drain_inbound_events(&mut world);

        // Entity still exists, still Authoritative.
        assert!(
            world.get::<Authoritative>(auth_ent).is_some(),
            "authoritative entity must remain authoritative"
        );
        assert!(
            world.get::<Shadow>(auth_ent).is_none(),
            "authoritative entity must not gain a Shadow component"
        );
    }

    #[test]
    fn peer_session_reset_drain_is_idempotent_single_pass() {
        // The reset event vec must be fully consumed by a single drain call so
        // a subsequent drain does not double-process. Without take semantics we
        // would re-apply the reset each tick.
        let mut world = setup_world(1);
        let (e, _) = spawn_shadow_with_tick(&mut world, 2, 500);

        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 1,
            });

        drain_inbound_events(&mut world);
        assert_eq!(world.get::<Shadow>(e).unwrap().last_peer_tick, 0);
        // The queue must be drained.
        assert!(
            world
                .resource::<InboundQueue>()
                .peer_session_resets
                .is_empty(),
            "peer_session_resets must be drained after drain_inbound_events"
        );

        // Simulate the authoritative peer advancing the shadow via a legitimate
        // peer_update. If the reset event were re-processed, the update would
        // be overwritten back to 0 on the next drain.
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        // Note: we can't easily reuse the same EntityId; simpler path is to
        // bump the existing shadow's tick via another peer_update.
        let eid = world
            .get::<crate::components::NetworkId>(e)
            .map(|n| n.0)
            .expect("shadow should have NetworkId");
        let mut updated = state.clone();
        updated.id = eid;
        updated.position = Vec3::new(5.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 50,
                entities: vec![updated],
            });

        drain_inbound_events(&mut world);
        assert_eq!(
            world.get::<Shadow>(e).unwrap().last_peer_tick,
            50,
            "second drain (no new reset event) must not re-zero last_peer_tick"
        );
    }

    #[test]
    fn peer_session_reset_with_no_matching_shadows_is_noop() {
        // Reset for a server we have no shadows for. Must not panic and must
        // still drain the event.
        let mut world = setup_world(1);
        // No shadows owned by server 2.
        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 42,
            });

        drain_inbound_events(&mut world);

        assert!(
            world
                .resource::<InboundQueue>()
                .peer_session_resets
                .is_empty(),
            "reset event must be drained even when there are no matching shadows"
        );
    }

    #[test]
    fn peer_session_reset_applies_before_peer_update_in_same_tick() {
        // Integration-shape test (per batch spec): in a single drain call,
        // the reset must be applied BEFORE this tick's peer_updates so a
        // post-restart update with a low tick is accepted by the
        // `update.tick < shadow.last_peer_tick` guard. Without the ordering,
        // the old last_peer_tick would reject the low-tick update.
        let mut world = setup_world(1);

        // Seed a shadow with a HIGH last_peer_tick (the pre-restart value).
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        let bevy_entity =
            spawn_from_entity_state_direct(&mut world, &state, false, Some((14730, 14730)));
        assert_eq!(
            world.get::<Shadow>(bevy_entity).unwrap().last_peer_tick,
            14730
        );

        // Enqueue BOTH: a session reset AND a low-tick peer_update for the
        // same shadow. Ordering matters — the reset must be drained first.
        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 0xCAFE,
            });
        let mut post_restart = state.clone();
        post_restart.id = eid;
        post_restart.position = Vec3::new(100.0, 0.0, 0.0);
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 5, // far below 14730
                entities: vec![post_restart],
            });

        drain_inbound_events(&mut world);

        // The low-tick update would have been rejected without the reset.
        assert_eq!(
            world.get::<Shadow>(bevy_entity).unwrap().last_peer_tick,
            5,
            "post-reset low-tick update must be accepted (reset drained first)"
        );
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(
            pos.0,
            Vec3::new(100.0, 0.0, 0.0),
            "post-reset low-tick update must actually mutate position"
        );
    }

    #[test]
    fn peer_session_reset_isolated_per_authority_server() {
        // Integration-shape test (per batch spec): enqueue a reset for server 2,
        // tick once, assert shadows for auth=2 are zeroed and shadows for
        // auth=3 are untouched. Uses the full drain_inbound_events pipeline.
        let mut world = setup_world(1);

        let (e2_a, _) = spawn_shadow_with_tick(&mut world, 2, 100);
        let (e2_b, _) = spawn_shadow_with_tick(&mut world, 2, 200);
        let (e3_a, _) = spawn_shadow_with_tick(&mut world, 3, 300);

        world
            .resource_mut::<InboundQueue>()
            .peer_session_resets
            .push(PeerSessionReset {
                server_id: ServerId(2),
                new_session_id: 0xDEAD,
            });

        drain_inbound_events(&mut world);

        assert_eq!(world.get::<Shadow>(e2_a).unwrap().last_peer_tick, 0);
        assert_eq!(world.get::<Shadow>(e2_b).unwrap().last_peer_tick, 0);
        assert_eq!(
            world.get::<Shadow>(e3_a).unwrap().last_peer_tick,
            300,
            "server 3 shadow must not be affected by a server 2 reset"
        );
    }
}
