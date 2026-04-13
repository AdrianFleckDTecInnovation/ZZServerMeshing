use bevy_ecs::prelude::*;
use bevy_ecs::schedule::ScheduleLabel;
use crate::systems;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TickSchedule;

/// Build the ECS schedule with all systems in the correct execution order.
pub fn build_schedule() -> Schedule {
    let mut schedule = Schedule::new(TickSchedule);

    schedule.add_systems((
        systems::drain_inbound_events,
        systems::drain_client_events.after(systems::drain_inbound_events),
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
        systems::prepare_client_snapshot.after(systems::prepare_replication),
        systems::shadow_cleanup.after(systems::prepare_client_snapshot),
        systems::record_metrics.after(systems::shadow_cleanup),
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
        world.init_resource::<TickCounter>();
        world.init_resource::<PeerAddresses>();
        world.init_resource::<EntityLookup>();
        world.init_resource::<InboundQueue>();
        world.init_resource::<CoordEventQueue>();
        world.init_resource::<OutboundReplication>();
        world.init_resource::<ClientSnapshot>();
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();
        world.init_resource::<TimingAccumulator>();
        world.init_resource::<ClientRegistry>();
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
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Position should have moved
        let pos = world.get::<Position>(entity).unwrap();
        assert!(pos.0.x > 0.0);

        // Replication output should have the entity
        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.snapshot.is_some());
        let entities = outbound.snapshot.as_ref().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].id, eid);

        // Client snapshot should have a binary frame
        let snapshot = world.resource::<ClientSnapshot>();
        assert!(snapshot.frame.is_some());
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
            world.resource_mut::<OutboundReplication>().snapshot = None;
            world.resource_mut::<ClientSnapshot>().frame = None;
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
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(2),
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

        world.resource_mut::<InboundQueue>().client_inputs.push(
            shared::protocol::ClientMessage::SpawnEntity {
                position: [10.0, 0.0, 10.0],
                entity_type: "npc".to_string(),
            },
        );

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
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some(1));

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Replication: only authoritative
        let repl = world.resource::<OutboundReplication>();
        assert!(repl.snapshot.is_some());
        assert_eq!(repl.snapshot.as_ref().unwrap().len(), 1);

        // Client snapshot: both auth + shadow
        let snap = world.resource::<ClientSnapshot>();
        let frame = snap.frame.as_ref().unwrap();
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 2);
    }

    // -- Schedule integration: spawn then move in same tick --

    #[test]
    fn spawn_then_move_same_tick() {
        let mut world = setup_world(1);
        let uuid = uuid::Uuid::new_v4();

        // Queue both spawn and move for same entity on same tick
        world.resource_mut::<InboundQueue>().client_inputs.push(
            shared::protocol::ClientMessage::SpawnPlayer {
                player_id: uuid.to_string(),
                position: [0.0, 0.0, 0.0],
            },
        );
        world.resource_mut::<InboundQueue>().client_inputs.push(
            shared::protocol::ClientMessage::MoveInput {
                entity_id: uuid.to_string(),
                direction: [1.0, 0.0, 0.0],
                seq: 1,
            },
        );

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        let eid = shared::entity::EntityId(uuid);
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let vel = world.get::<Velocity>(bevy_entity).unwrap();
        // Player speed = 5.0 on X axis
        assert!((vel.0.x - 5.0).abs() < 0.01);
    }

    // -- Schedule integration: entity transferred during tick is not in replication --

    #[test]
    fn transferred_entity_not_in_replication() {
        let mut world = setup_world(1);
        for _ in 0..10 {
            let state = EntityState::new_npc(Vec3::new(400.0, 0.0, 400.0), ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        let remaining_auth = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        let repl = world.resource::<OutboundReplication>();
        if let Some(entities) = &repl.snapshot {
            // Replication count should equal remaining auth count (transferred entities gone)
            assert_eq!(entities.len(), remaining_auth);
        }
    }

    // -- Schedule integration: shadow cleanup runs after snapshot --

    #[test]
    fn stale_shadow_included_in_snapshot_then_cleaned() {
        let mut world = setup_world(1);

        // Shadow entity that will be cleaned up this tick
        let shadow = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        let shadow_eid = shadow.id;
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some(0));

        // Set tick well past stale threshold
        world.resource_mut::<TickCounter>().0 = 200;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Shadow should have been included in client snapshot BEFORE cleanup
        let frame = world.resource::<ClientSnapshot>().frame.as_ref().unwrap();
        let entity_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        // The snapshot was prepared before shadow_cleanup ran, so the shadow was included
        assert_eq!(entity_count, 1);

        // But after the full schedule run, the shadow should be gone
        assert!(!world.resource::<EntityLookup>().0.contains_key(&shadow_eid));
    }

    // -- Schedule integration: peer update + shadow cleanup race --

    #[test]
    fn fresh_shadow_survives_cleanup() {
        let mut world = setup_world(1);

        let entity_state = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(2));
        let eid = entity_state.id;

        // Push peer update with current tick
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 100,
            server_id: ServerId(2),
            entities: vec![entity_state],
        });

        world.resource_mut::<TickCounter>().0 = 100;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Shadow should survive (created at tick 100, current tick 100, not stale)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- Schedule integration: coord drain triggers shutdown --

    #[test]
    fn coord_drain_and_shutdown_in_schedule() {
        let mut world = setup_world(1);
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<CoordEventQueue>().drain_directives.push(vec![2]);
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Drain state should be active
        assert!(world.resource::<DrainState>().draining);
        // All entities should be gone
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 0);
        assert!(world.resource::<ShutdownRequested>().0);
    }

    // -- Schedule: empty world tick does not panic --

    #[test]
    fn empty_world_tick_no_panic() {
        let mut world = setup_world(1);
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = build_schedule();
        schedule.run(&mut world);

        // Replication should be None (no entities)
        assert!(world.resource::<OutboundReplication>().snapshot.is_none());
        // Client snapshot should still produce a valid frame (0 entities)
        let frame = world.resource::<ClientSnapshot>().frame.as_ref().unwrap();
        assert_eq!(frame[0], 0x01);
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 0);
    }
}
