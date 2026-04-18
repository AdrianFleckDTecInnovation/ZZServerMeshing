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
        // Design 015: compare local-tick stamps in the same (local) clock domain.
        // Using shadow.last_peer_tick here would be a cross-clock compare —
        // spurious cleanup firing every tick when peer clocks differ.
        if tick.saturating_sub(shadow.last_local_tick) > SHADOW_STALE_TICKS {
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

    // -- shadow_cleanup tests --

    #[test]
    fn shadow_cleanup_removes_stale() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some((1, 1)));

        world.resource_mut::<TickCounter>().0 = 400; // well past stale threshold (300)

        shadow_cleanup(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn shadow_cleanup_keeps_fresh() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some((50, 50)));

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
        spawn_from_entity_state_direct(&mut world, &state, false, Some((0, 0)));

        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS; // exactly at threshold

        shadow_cleanup(&mut world);
        assert!(
            world.resource::<EntityLookup>().0.contains_key(&eid),
            "entity at exact threshold should NOT be removed (strict >)"
        );
    }

    #[test]
    fn shadow_cleanup_one_past_threshold() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some((0, 0)));

        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS + 1;

        shadow_cleanup(&mut world);
        assert!(
            !world.resource::<EntityLookup>().0.contains_key(&eid),
            "entity one past threshold should be removed"
        );
    }

    #[test]
    fn shadow_stale_ticks_is_300() {
        assert_eq!(
            SHADOW_STALE_TICKS, 300,
            "SHADOW_STALE_TICKS should be 300 (15 seconds at 20Hz)"
        );
    }

    // =========================================================================
    // Design 015 regression: shadow_cleanup must use the local-tick domain.
    // =========================================================================

    /// Design 015 regression: shadow_cleanup must not treat a peer's tick as
    /// our tick. Pre-015, a peer with a lower TickCounter (e.g., restarted
    /// mid-session) caused `local_tick - peer_tick > SHADOW_STALE_TICKS` and
    /// every replication packet triggered a despawn-then-respawn cycle that
    /// showed to clients as rapid flicker of S2-authoritative shadows.
    #[test]
    fn shadow_cleanup_survives_peer_tick_far_behind_local_tick() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        // Peer is thousands of ticks behind us (simulates a peer that
        // restarted 7 minutes after us — the production pathology).
        spawn_from_entity_state_direct(&mut world, &state, false, Some((1_000, 59_000)));

        // Our tick is now 59_100 — only 100 ticks after the refresh.
        world.resource_mut::<TickCounter>().0 = 59_100;

        shadow_cleanup(&mut world);

        assert!(
            world.resource::<EntityLookup>().0.contains_key(&eid),
            "shadow must survive: peer tick is far behind ours but we stamped \
             last_local_tick=59_000, so 59_100-59_000=100 <= SHADOW_STALE_TICKS(300)"
        );
    }

    /// Design 015 complement: cleanup must still fire when the LOCAL tick has
    /// moved past SHADOW_STALE_TICKS since the last refresh — regardless of
    /// what last_peer_tick holds. Guards against the opposite failure mode
    /// where a naive "use peer tick" fix would keep truly stale shadows alive.
    #[test]
    fn shadow_cleanup_fires_when_local_tick_stale_even_if_peer_tick_recent() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        // Peer-tick is high (say peer is ahead of us — bogus in practice, but
        // the test is about what cleanup uses). Local-tick stamp is old.
        spawn_from_entity_state_direct(&mut world, &state, false, Some((50_000, 1_000)));

        // Our tick has advanced well past the TTL from last_local_tick=1000.
        world.resource_mut::<TickCounter>().0 = 1_000 + SHADOW_STALE_TICKS + 1;

        shadow_cleanup(&mut world);

        assert!(
            !world.resource::<EntityLookup>().0.contains_key(&eid),
            "shadow must be removed when last_local_tick is stale regardless of peer_tick"
        );
    }
}
