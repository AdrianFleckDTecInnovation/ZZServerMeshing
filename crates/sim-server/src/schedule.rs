use crate::systems;
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::ScheduleLabel;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TickSchedule;

/// Build the ECS schedule with all systems in the correct execution order.
pub fn build_schedule() -> Schedule {
    let mut schedule = Schedule::new(TickSchedule);

    // WI-222 (design 016, Batch 6): flip `shadow_cleanup` to run BEFORE
    // `prepare_client_snapshot` so stale shadows no longer appear in the
    // per-client snapshot on the tick they expire. Previously, a shadow that
    // crossed `SHADOW_STALE_TICKS` was rendered for one extra tick before
    // despawn — a visible "dead but still drawn" blip during crash churn.
    // `shadow_cleanup` depends only on `prepare_replication` (both read
    // Shadow components; replication must see them before cleanup removes
    // them), so moving it here preserves correctness.
    schedule.add_systems((
        systems::drain_inbound_events,
        systems::process_transfer_acks.after(systems::drain_inbound_events),
        systems::process_transfer_timeouts.after(systems::process_transfer_acks),
        systems::drain_client_events.after(systems::process_transfer_timeouts),
        systems::process_client_inputs.after(systems::drain_client_events),
        systems::process_coord_events.after(systems::drain_inbound_events),
        systems::npc_behavior
            .after(systems::process_client_inputs)
            .after(systems::process_coord_events),
        systems::physics_integration.after(systems::npc_behavior),
        systems::clamp_positions.after(systems::physics_integration),
        systems::process_transfers.after(systems::clamp_positions),
        systems::process_drain.after(systems::process_transfers),
        systems::prepare_replication.after(systems::process_drain),
        systems::shadow_cleanup.after(systems::prepare_replication),
        systems::prepare_client_snapshot.after(systems::shadow_cleanup),
        systems::record_metrics.after(systems::prepare_client_snapshot),
    ));

    schedule
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use crate::resources::*;
    use bevy_ecs::world::World;
    use glam::Vec3;
    use shared::entity::{EntityState, ServerId};

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

    #[test]
    fn schedule_builds_without_panic() {
        let _ = build_schedule();
    }

    #[test]
    fn full_tick_cycle() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Give it a velocity
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(5.0, 0.0, 0.0);

        // Register a test client so prepare_client_snapshot sends a frame
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Position should have moved
        let pos = world.get::<Position>(entity).unwrap();
        assert!(pos.0.x > 0.0);

        // Replication output should have the entity
        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_some());
        let entities = outbound.full_snapshot.as_ref().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].id, eid);

        // Client snapshot should have a binary frame via test client channel
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x01); // full snapshot
    }

    #[test]
    fn multi_tick_cycle() {
        let mut world = setup_world(1);
        // Use a Player entity so npc_behavior doesn't randomize the velocity
        let state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(5.0, 0.0, 0.0);

        let mut schedule = build_schedule();

        for tick in 1..=10 {
            world.resource_mut::<TickCounter>().0 = tick;
            // Clear outputs from previous tick
            world.resource_mut::<OutboundReplication>().full_snapshot = None;
            world.resource_mut::<OutboundReplication>().per_peer.clear();
            world.resource_mut::<OutboundTransfers>().messages.clear();
            world.resource_mut::<OutboundForwards>().messages.clear();

            schedule.run(&mut world);
        }

        // After 10 ticks, position should have advanced significantly
        let pos = world.get::<Position>(entity).unwrap();
        assert!(pos.0.x > 1.0);
    }

    #[test]
    fn inbound_queue_drain_in_schedule() {
        let mut world = setup_world(1);

        // Push a peer update
        let entity_state = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        let eid = entity_state.id;
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 10,
                entities: vec![entity_state],
            });

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Shadow entity should have been created
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        assert!(world.get::<Shadow>(bevy_entity).is_some());
    }

    #[test]
    fn client_input_in_schedule() {
        let mut world = setup_world(1);

        world.resource_mut::<InboundQueue>().client_inputs.push((
            0,
            shared::protocol::ClientMessage::SpawnEntity {
                position: [10.0, 0.0, 10.0],
                entity_type: "npc".to_string(),
            },
        ));

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Entity should have been spawned
        assert_eq!(world.resource::<EntityLookup>().0.len(), 1);
    }

    #[test]
    fn replication_output_in_schedule() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        spawn_from_entity_state_direct(&mut world, &state, true, None);

        let shadow = EntityState::new_npc(Vec3::X, ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some((1, 1)));

        // Register a test client to verify client snapshot content
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Replication: only authoritative
        let repl = world.resource::<OutboundReplication>();
        assert!(repl.full_snapshot.is_some());
        assert_eq!(repl.full_snapshot.as_ref().unwrap().len(), 1);

        // Client snapshot: both auth + shadow via test client channel
        let frame = rx.try_recv().unwrap();
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 2);
    }

    // -- Schedule integration: spawn then move in same tick --

    #[test]
    fn spawn_then_move_same_tick() {
        let mut world = setup_world(1);
        let eid = shared::entity::EntityId::new();

        // Queue both spawn and move for same entity on same tick
        world.resource_mut::<InboundQueue>().client_inputs.push((
            0,
            shared::protocol::ClientMessage::SpawnPlayer {
                player_id: eid,
                position: [0.0, 0.0, 0.0],
            },
        ));
        world.resource_mut::<InboundQueue>().client_inputs.push((
            0,
            shared::protocol::ClientMessage::MoveInput {
                entity_id: eid,
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
        ));

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let vel = world.get::<Velocity>(bevy_entity).unwrap();
        // Player speed = 5.0 on X axis
        assert!((vel.0.x - 5.0).abs() < 0.01);
    }

    // -- Schedule integration: entity transferred during tick remains (two-phase) --

    #[test]
    fn transferred_entity_remains_until_ack() {
        use crate::components::TransferOutbound;

        let mut world = setup_world(1);
        for _ in 0..10 {
            let state = EntityState::new_npc(Vec3::new(400.0, 0.0, 400.0), ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers =
            Some((vec![], shared::entity::ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Two-phase: transferred entities keep Authoritative + TransferOutbound
        let auth_count = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        assert_eq!(
            auth_count, 10,
            "all entities should still be authoritative (two-phase)"
        );

        // Some should have TransferOutbound marker
        let outbound_count = world
            .query_filtered::<(), With<TransferOutbound>>()
            .iter(&world)
            .count();
        assert!(
            outbound_count > 0,
            "some entities should be marked for transfer"
        );

        // Replication output includes TransferOutbound entities so peers maintain
        // up-to-date shadows for crash recovery.
        let repl = world.resource::<OutboundReplication>();
        if let Some(entities) = &repl.full_snapshot {
            assert_eq!(entities.len(), 10,
                "all entities including TransferOutbound should be in replication for crash recovery");
        }
    }

    // -- WI-222: shadow_cleanup runs BEFORE prepare_client_snapshot --

    #[test]
    fn stale_shadow_cleaned_before_snapshot_built() {
        // WI-222 (design 016, Batch 6): a shadow that has crossed the
        // staleness threshold must be cleaned up BEFORE the per-client
        // snapshot is built, so clients never see a "dead but still rendered"
        // entity for one extra tick. This test locks in the new ordering:
        //   prepare_replication → shadow_cleanup → prepare_client_snapshot
        let mut world = setup_world(1);

        // Shadow entity that will be cleaned up this tick
        let shadow = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        let shadow_eid = shadow.id;
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some((0, 0)));

        // Register a test client to verify frame content
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        // Set tick well past stale threshold (SHADOW_STALE_TICKS = 300)
        world.resource_mut::<TickCounter>().0 = 400;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Post-WI-222: shadow is cleaned up BEFORE the snapshot is built,
        // so the snapshot contains zero entities (no 1-tick blip).
        let frame = rx.try_recv().unwrap();
        let entity_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(
            entity_count, 0,
            "stale shadow must not appear in snapshot on the tick it expires"
        );

        // Shadow is also gone from the world after the full schedule run.
        assert!(!world.resource::<EntityLookup>().0.contains_key(&shadow_eid));
    }

    // -- Schedule ordering: process_transfer_acks runs before process_transfer_timeouts --

    #[test]
    fn ack_processed_before_timeout_in_schedule() {
        use crate::components::TransferOutbound;
        use crate::resources::InboundTransferAck;

        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Entity is in TransferOutbound state, sent long ago (will trigger timeout)
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // ACK arrives at the same tick as the timeout would fire
        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 12,
            });

        // Set tick past timeout
        world.resource_mut::<TickCounter>().0 = 12;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Because process_transfer_acks runs BEFORE process_transfer_timeouts,
        // the ACK should have despawned the entity BEFORE timeout had a chance to retry.
        assert!(
            !world.resource::<EntityLookup>().0.contains_key(&eid),
            "ACK should despawn the entity before timeout can retry it"
        );

        // No retry messages should have been sent
        assert!(
            world.resource::<OutboundTransfers>().messages.is_empty(),
            "no retry should be attempted because ACK was processed first"
        );
    }

    // -- Schedule ordering: both ack + timeout run after drain_inbound_events --

    #[test]
    fn transfer_ack_and_timeout_after_inbound_events() {
        use crate::components::TransferOutbound;
        use crate::resources::{InboundTransferAck, PeerUpdate};

        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Entity is in TransferOutbound state
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Push both a peer update (inbound event) and a transfer ACK in the same tick
        let shadow_state = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(3));
        let shadow_eid = shadow_state.id;
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 12,
                entities: vec![shadow_state],
            });

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 12,
            });

        world.resource_mut::<TickCounter>().0 = 12;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Both should have been processed:
        // 1. Peer update creates the shadow (drain_inbound_events)
        assert!(
            world.resource::<EntityLookup>().0.contains_key(&shadow_eid),
            "shadow from peer update should exist"
        );
        // 2. Transfer ACK despawns the entity (process_transfer_acks)
        assert!(
            !world.resource::<EntityLookup>().0.contains_key(&eid),
            "transfer ACK should have despawned entity"
        );
    }

    // -- Schedule ordering: drain_client_events runs after transfer systems --

    #[test]
    fn client_events_processed_after_transfers() {
        use crate::components::TransferOutbound;
        use crate::resources::InboundTransferAck;

        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Entity is in TransferOutbound state
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // ACK for the entity
        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 5,
            });

        // Also register a new client (drain_client_events)
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::resources::ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world.resource_mut::<InboundQueue>().register_clients.push(
            crate::resources::ClientRegistrationEvent {
                client_id: 42,
                tx,
                acked_tick: acked,
                player_uuid: shared::entity::EntityId::new(),
                migrating_out: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );

        world.resource_mut::<TickCounter>().0 = 5;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Transfer ACK processed: entity gone
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // Client registered (drain_client_events ran after)
        assert!(world
            .resource::<crate::resources::ClientRegistry>()
            .clients
            .contains_key(&42));
    }

    // -- Schedule integration: peer update + shadow cleanup race --

    #[test]
    fn fresh_shadow_survives_cleanup() {
        let mut world = setup_world(1);

        let entity_state = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(2));
        let eid = entity_state.id;

        // Push peer update with current tick
        world
            .resource_mut::<InboundQueue>()
            .peer_updates
            .push(PeerUpdate {
                tick: 100,
                entities: vec![entity_state],
            });

        world.resource_mut::<TickCounter>().0 = 100;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Shadow should survive (created at tick 100, current tick 100, not stale)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- Schedule integration: coord drain with two-phase transfers --

    #[test]
    fn coord_drain_two_phase_in_schedule() {
        use crate::components::TransferOutbound;
        use crate::resources::InboundTransferAck;

        let mut world = setup_world(1);
        let mut eids = Vec::new();
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            eids.push(state.id);
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<CoordEventQueue>()
            .drain_directives
            .push(vec![shared::entity::ServerId(2)]);
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // After first tick: drain active, entities have TransferOutbound, NOT yet shutdown
        assert!(world.resource::<DrainState>().0.is_some());
        let outbound_count = world
            .query_filtered::<(), With<TransferOutbound>>()
            .iter(&world)
            .count();
        assert_eq!(
            outbound_count, 3,
            "all 3 entities should have TransferOutbound"
        );
        assert!(
            !world.resource::<ShutdownRequested>().0,
            "should not shutdown yet (waiting for ACKs)"
        );

        // Simulate ACKs arriving for all 3 entities
        for eid in &eids {
            world
                .resource_mut::<InboundQueue>()
                .transfer_acks
                .push(InboundTransferAck {
                    entity_id: *eid,
                    tick: 2,
                });
        }

        // Tick 2: ACKs processed, entities despawned, drain complete
        world.resource_mut::<TickCounter>().0 = 2;
        // Clear outbound from previous tick
        world.resource_mut::<OutboundTransfers>().messages.clear();
        schedule.run(&mut world);

        let remaining = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        assert_eq!(remaining, 0, "all entities should be despawned after ACKs");
        assert!(
            world.resource::<ShutdownRequested>().0,
            "shutdown should be requested"
        );
    }

    #[test]
    fn coord_drain_timeout_forces_shutdown() {
        let mut world = setup_world(1);
        for _ in 0..2 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<CoordEventQueue>()
            .drain_directives
            .push(vec![shared::entity::ServerId(2)]);
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        assert!(!world.resource::<ShutdownRequested>().0);

        // Jump tick far past drain timeout (600 ticks)
        world.resource_mut::<TickCounter>().0 = 700;
        world.resource_mut::<OutboundTransfers>().messages.clear();
        schedule.run(&mut world);

        // Should force shutdown even with pending transfers
        assert!(
            world.resource::<ShutdownRequested>().0,
            "drain timeout should force shutdown"
        );
    }

    // -- Schedule: empty world tick does not panic --

    #[test]
    fn empty_world_tick_no_panic() {
        let mut world = setup_world(1);

        // Register a test client to verify empty frame
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Replication should be None (no entities)
        assert!(world
            .resource::<OutboundReplication>()
            .full_snapshot
            .is_none());
        // Client snapshot should still produce a valid frame (0 entities) via test channel
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x01);
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 0);
    }
}
