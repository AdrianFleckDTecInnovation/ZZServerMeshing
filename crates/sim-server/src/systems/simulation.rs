use crate::components::*;
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use rand::Rng;
use shared::entity::EntityType;
use shared::spatial::clamp_to_world;
use shared::tick::TICK_RATE;

/// NPC direction change interval in ticks.
const NPC_DIRECTION_CHANGE_INTERVAL: u64 = (TICK_RATE as u64) * 3; // every 3 seconds

/// NPC wandering behavior: change direction every NPC_DIRECTION_CHANGE_INTERVAL ticks.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn npc_behavior(
    tick: Res<TickCounter>,
    mut query: Query<
        (&NetworkId, &mut Velocity, &EntityKind),
        (With<Authoritative>, Without<TransferOutbound>),
    >,
) {
    let current_tick = tick.0;
    let mut rng = rand::thread_rng();

    for (nid, mut vel, kind) in query.iter_mut() {
        if kind.0 != EntityType::Npc {
            continue;
        }

        let phase = (nid.0 .0.as_u128() % NPC_DIRECTION_CHANGE_INTERVAL as u128) as u64;
        if current_tick % NPC_DIRECTION_CHANGE_INTERVAL == phase {
            let angle = rng.gen_range(0.0..std::f32::consts::TAU);
            let speed = rng.gen_range(1.0..3.0);
            vel.0 = Vec3::new(angle.cos() * speed, 0.0, angle.sin() * speed);
        }
    }
}

/// Integrate position from velocity for all authoritative entities.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn physics_integration(
    mut query: Query<(&mut Position, &Velocity), (With<Authoritative>, Without<TransferOutbound>)>,
) {
    let dt = super::TICK_DURATION.as_secs_f32();
    for (mut pos, vel) in query.iter_mut() {
        pos.0 += vel.0 * dt;
    }
}

/// Clamp all authoritative entity positions to world bounds.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn clamp_positions(
    mut query: Query<&mut Position, (With<Authoritative>, Without<TransferOutbound>)>,
) {
    for mut pos in query.iter_mut() {
        pos.0 = clamp_to_world(pos.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
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

    fn spawn_auth_player(world: &mut World, pos: Vec3, server_id: u16) -> (Entity, EntityId) {
        let state = EntityState::new_player(pos, ServerId(server_id));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, true, None);
        (entity, eid)
    }

    // -- physics_integration tests --

    #[test]
    fn physics_moves_entity() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(5.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert!(pos.0.x > 0.0); // moved in +X
    }

    #[test]
    fn physics_shadow_not_moved() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(10.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // shadow not moved
    }

    #[test]
    fn physics_zero_velocity_no_movement() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 5.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::new(10.0, 0.0, 5.0)); // unchanged
    }

    // -- clamp_positions tests --

    #[test]
    fn clamp_out_of_bounds() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(9999.0, 0.0, -9999.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0.x, shared::spatial::WORLD_EXTENT);
        assert_eq!(pos.0.z, -shared::spatial::WORLD_EXTENT);
    }

    #[test]
    fn clamp_in_bounds_unchanged() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, -200.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::new(100.0, 0.0, -200.0));
    }

    // -- npc_behavior tests --

    #[test]
    fn npc_velocity_changes_at_interval() {
        let mut world = setup_world(1);
        let (entity, _eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Run enough ticks to trigger direction change
        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        let mut found_nonzero = false;
        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let vel = world.get::<Velocity>(entity).unwrap();
            if vel.0.length() > 0.01 {
                found_nonzero = true;
            }
        }
        assert!(found_nonzero, "NPC should have gained velocity after direction change");
    }

    #[test]
    fn npc_behavior_does_not_modify_player() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO); // player velocity not touched
    }

    #[test]
    fn npc_behavior_does_not_modify_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO);
    }

    // -- Physics integration dt correctness --

    #[test]
    fn physics_dt_is_tick_duration() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // Set velocity to 1 unit/sec on X. After one tick (50ms = 0.05s), position should be 0.05.
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(1.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert!((pos.0.x - 0.05).abs() < 1e-6, "dt should be TICK_DURATION (50ms)");
    }

    // -- TransferOutbound exclusion tests --

    #[test]
    fn physics_does_not_move_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(10.0, 0.0, 0.0);

        // Mark as TransferOutbound
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO, "TransferOutbound entity should not be moved by physics");
    }

    #[test]
    fn npc_behavior_skips_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Mark as TransferOutbound
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "TransferOutbound NPC should not have velocity changed");
    }

    #[test]
    fn clamp_skips_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(9999.0, 0.0, 0.0), 1);

        // Mark as TransferOutbound
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0.x, 9999.0, "TransferOutbound entity should not be clamped");
    }
}
