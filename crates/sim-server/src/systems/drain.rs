use crate::components::*;
use crate::conversion::to_entity_state;
use crate::resources::*;
use bevy_ecs::prelude::*;
use shared::entity::EntityId;
use shared::protocol::PeerMessage;

/// Maximum ticks a drain can be active before forcing shutdown.
const DRAIN_TIMEOUT_TICKS: u64 = 600; // 30 seconds at 20Hz

/// Drain all entities to target servers (round-robin). Called when draining.
/// Uses two-phase transfers: entities get TransferOutbound instead of being despawned.
/// Drain completes (ShutdownRequested) only when all entities are ACK'd or timed out.
pub fn process_drain(world: &mut World) {
    let (targets, started_tick) = match &world.resource::<DrainState>().0 {
        Some(info) if !info.targets.is_empty() => (info.targets.clone(), info.started_tick),
        _ => return,
    };

    let server_id = world.resource::<ServerConfig>().server_id;
    let tick = world.resource::<TickCounter>().0;

    // Record drain start tick if not yet set (started_tick == 0 means just activated)
    if started_tick == 0 {
        if let Some(info) = &mut world.resource_mut::<DrainState>().0 {
            info.started_tick = tick;
        }
    }

    // Collect authoritative entities that do NOT already have TransferOutbound
    let mut to_transfer: Vec<(Entity, EntityId)> = Vec::new();
    let mut query = world.query_filtered::<(Entity, &NetworkId), With<Authoritative>>();
    for (entity, nid) in query.iter(&world) {
        if world.get::<TransferOutbound>(entity).is_none() {
            to_transfer.push((entity, nid.0));
        }
    }

    let mut transferred = 0;

    for (i, (entity, eid)) in to_transfer.iter().enumerate() {
        let target_server = targets[i % targets.len()];
        let target_addr = match world.resource::<PeerAddresses>().0.get(&target_server.0) {
            Some(addr) => *addr,
            None => continue,
        };

        // Build EntityState
        let nid = world.get::<NetworkId>(*entity).unwrap();
        let pos = world.get::<Position>(*entity).unwrap();
        let vel = world.get::<Velocity>(*entity).unwrap();
        let kind = world.get::<EntityTypeComp>(*entity).unwrap();
        let auth_comp = world.get::<AuthorityServer>(*entity).unwrap();
        let seq = world.get::<LastInputSeq>(*entity);

        let mut entity_state = to_entity_state(nid, pos, vel, kind, auth_comp, seq);
        entity_state.authority_server = target_server;

        let msg = PeerMessage::AuthorityTransfer {
            tick,
            entity_id: *eid,
            from_server: server_id,
            to_server: target_server,
            full_state: entity_state,
        };

        super::enqueue_outbound(
            &mut world.resource_mut::<OutboundTransfers>().messages,
            target_addr,
            &msg,
        );

        // Two-phase: mark with TransferOutbound instead of despawning.
        // Entity is frozen (excluded from sim/replication via query filters).
        world.entity_mut(*entity).insert(TransferOutbound {
            target_server: target_server.0,
            target_addr,
            sent_tick: tick,
            retry_count: 0,
        });
        transferred += 1;
        // DO NOT despawn -- entity will be despawned when ACK arrives
    }

    if transferred > 0 {
        tracing::info!("Drained {transferred} entities (awaiting ACKs)");
    }

    // Check if drain is complete: no authoritative entities without TransferOutbound,
    // and no pending TransferOutbound entities.
    let remaining = world
        .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
        .iter(&world)
        .count();
    let pending_transfers = world
        .query_filtered::<(), With<TransferOutbound>>()
        .iter(&world)
        .count();

    if remaining == 0 && pending_transfers == 0 {
        world.resource_mut::<ShutdownRequested>().0 = true;
        tracing::info!("Drain complete: all entities transferred and ACK'd");
        return;
    }

    // Drain timeout: if drain has been active too long, force shutdown
    if let Some(info) = &world.resource::<DrainState>().0 {
        let started = info.started_tick;
        if started > 0 && tick.saturating_sub(started) > DRAIN_TIMEOUT_TICKS {
            tracing::error!(
                remaining,
                pending_transfers,
                "Drain timeout after {} ticks, forcing shutdown",
                tick.saturating_sub(started),
            );
            world.resource_mut::<ShutdownRequested>().0 = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::world::World;
    use glam::Vec3;
    use shared::entity::{EntityId, EntityState, ServerId};

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

    // -- process_drain tests --

    #[test]
    fn drain_all_entities() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Entities should have TransferOutbound (not despawned yet)
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 2);
    }

    #[test]
    fn drain_not_active_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // draining = false (default)
        process_drain(&mut world);

        // No changes
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 0);
    }

    // -- process_drain: round-robin to multiple targets --

    #[test]
    fn drain_round_robin_to_multiple_targets() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PeerAddresses>().0.insert(3, "127.0.0.1:9103".parse().unwrap());

        for _ in 0..6 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2), ServerId(3)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // All 6 should have TransferOutbound
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 6);

        // Check round-robin: 3 to server 2, 3 to server 3
        let mut query = world.query::<&TransferOutbound>();
        let mut to_2 = 0;
        let mut to_3 = 0;
        for outbound in query.iter(&world) {
            if outbound.target_server == 2 { to_2 += 1; }
            if outbound.target_server == 3 { to_3 += 1; }
        }
        assert_eq!(to_2, 3, "should round-robin 3 to server 2");
        assert_eq!(to_3, 3, "should round-robin 3 to server 3");
    }

    // -- process_drain: empty targets list --

    #[test]
    fn drain_empty_targets_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![], started_tick: 0 });

        process_drain(&mut world);

        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 0);
    }

    #[test]
    fn drain_targets_none_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = None;

        process_drain(&mut world);

        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 0);
    }

    // -- Drain + transfer interaction --

    #[test]
    fn drain_and_transfer_same_tick() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        // Entity already has TransferOutbound (from prior rebalance)
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Both should have TransferOutbound: 1 existing + 1 new
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 2);
    }

    // -- Drain waits for ACKs --

    #[test]
    fn drain_does_not_shutdown_with_pending_transfers() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        for _ in 0..3 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        assert!(!world.resource::<ShutdownRequested>().0);
    }

    #[test]
    fn drain_completes_after_all_acks_received() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let mut eids = Vec::new();
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            eids.push(state.id);
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);
        assert!(!world.resource::<ShutdownRequested>().0);

        // Simulate ACKs
        for eid in &eids {
            world.resource_mut::<InboundQueue>().transfer_acks.push(
                InboundTransferAck { entity_id: *eid, tick: 11 }
            );
        }

        crate::systems::process_transfer_acks(&mut world);

        world.resource_mut::<TickCounter>().0 = 12;
        process_drain(&mut world);

        assert!(world.resource::<ShutdownRequested>().0);
    }

    #[test]
    fn drain_skips_entities_already_in_transfer() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let mut entities = Vec::new();
        for _ in 0..4 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            entities.push((entity, eid));
        }

        for (entity, _) in entities.iter().take(2) {
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 5,
                retry_count: 0,
            });
        }

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 4);

        for (entity, _) in entities.iter().take(2) {
            assert_eq!(world.get::<TransferOutbound>(*entity).unwrap().sent_tick, 5);
        }

        for (entity, _) in entities.iter().skip(2) {
            assert_eq!(world.get::<TransferOutbound>(*entity).unwrap().sent_tick, 10);
        }
    }

    // -- EntityLookup consistency after drain --

    #[test]
    fn entity_lookup_populated_after_drain_before_acks() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Entity should still be in lookup (waiting for ACK)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- Drain with target peer that has no address --

    #[test]
    fn drain_target_no_address_skips_entity() {
        let mut world = setup_world(1);
        // Do NOT add peer address for server 2

        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Entity should NOT have TransferOutbound (no peer address)
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 0);
    }

    // -- Drain with no entities --

    #[test]
    fn drain_with_no_entities_immediate_shutdown() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // No entities
        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Should complete immediately (no entities = remaining=0, pending=0)
        assert!(world.resource::<ShutdownRequested>().0);
    }

    // -- Drain timeout --

    #[test]
    fn drain_timeout_forces_shutdown_with_pending() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);
        assert!(!world.resource::<ShutdownRequested>().0);

        // Advance far past timeout (10 + 600 + 1 = 611)
        world.resource_mut::<TickCounter>().0 = 611;
        process_drain(&mut world);

        assert!(world.resource::<ShutdownRequested>().0);
    }

    #[test]
    fn drain_records_start_tick() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().0 = Some(DrainInfo { targets: vec![ServerId(2)], started_tick: 0 });
        world.resource_mut::<TickCounter>().0 = 42;

        process_drain(&mut world);

        assert_eq!(world.resource::<DrainState>().0.as_ref().unwrap().started_tick, 42);
    }
}
