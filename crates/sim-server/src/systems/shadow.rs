use crate::components::*;
use crate::conversion::despawn_and_remove;
use crate::resources::*;
use bevy_ecs::prelude::*;
use shared::entity::EntityId;
use shared::tick::TICK_RATE;

/// Max ticks before a shadow entity is considered stale and removed.
/// 15 seconds gives comfortable margin over worst-case crash detection (5s heartbeat
/// timeout + 5s rebalance interval = 10s) so shadows survive until RecoverEntities arrives.
const SHADOW_STALE_TICKS: u64 = TICK_RATE as u64 * 15; // 15 seconds

/// Remove stale shadow entities.
pub fn shadow_cleanup(world: &mut World) {
    let tick = world.resource::<TickCounter>().0;

    let mut stale: Vec<(Entity, EntityId)> = Vec::new();
    let mut query = world.query_filtered::<(Entity, &NetworkId, &Shadow), With<Shadow>>();
    for (entity, nid, shadow) in query.iter(&world) {
        if tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS {
            stale.push((entity, nid.0));
        }
    }

    for (entity, eid) in stale {
        despawn_and_remove(world, entity, eid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
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

    // -- shadow_cleanup tests --

    #[test]
    fn shadow_cleanup_removes_stale() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        world.resource_mut::<TickCounter>().0 = 400; // well past stale threshold (300)

        shadow_cleanup(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn shadow_cleanup_keeps_fresh() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(50));

        world.resource_mut::<TickCounter>().0 = 60; // only 10 ticks old

        shadow_cleanup(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- Shadow cleanup boundary: exactly at stale threshold --

    #[test]
    fn shadow_cleanup_at_exact_threshold() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(0));

        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS; // exactly at threshold

        shadow_cleanup(&mut world);
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid), "entity at exact threshold should NOT be removed (strict >)");
    }

    #[test]
    fn shadow_cleanup_one_past_threshold() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(0));

        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS + 1;

        shadow_cleanup(&mut world);
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid), "entity one past threshold should be removed");
    }

    #[test]
    fn shadow_stale_ticks_is_300() {
        assert_eq!(SHADOW_STALE_TICKS, 300, "SHADOW_STALE_TICKS should be 300 (15 seconds at 20Hz)");
    }
}
