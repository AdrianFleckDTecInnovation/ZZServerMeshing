use crate::components::*;
use crate::resources::EntityLookup;
use bevy_ecs::prelude::*;
use shared::entity::{EntityState, EntityType};

/// Spawn a new entity in the ECS World directly from an EntityState.
/// This variant operates on &mut World for unit tests and initialization.
pub fn spawn_from_entity_state_direct(
    world: &mut World,
    state: &EntityState,
    is_authoritative: bool,
    shadow_tick: Option<u64>,
) -> Entity {
    let mut entity_cmd = world.spawn((
        NetworkId(state.id),
        Position(state.position),
        Velocity(state.velocity),
        EntityKind(state.entity_type),
        AuthorityServer(state.authority_server),
    ));

    if is_authoritative {
        entity_cmd.insert(Authoritative);
    } else {
        entity_cmd.insert(Shadow {
            last_seen_tick: shadow_tick.unwrap_or(0),
        });
    }

    if state.entity_type == EntityType::Player {
        entity_cmd.insert(LastInputSeq(state.last_input_seq));
    }

    let entity = entity_cmd.id();

    // Update entity lookup
    world
        .resource_mut::<EntityLookup>()
        .0
        .insert(state.id, entity);

    entity
}

/// Assemble an EntityState from ECS components for network serialization.
pub fn to_entity_state(
    network_id: &NetworkId,
    position: &Position,
    velocity: &Velocity,
    kind: &EntityKind,
    authority: &AuthorityServer,
    last_seq: Option<&LastInputSeq>,
) -> EntityState {
    EntityState {
        id: network_id.0,
        position: position.0,
        velocity: velocity.0,
        entity_type: kind.0,
        authority_server: authority.0,
        last_input_seq: last_seq.map_or(0, |s| s.0),
    }
}

/// Despawn a Bevy entity and remove it from EntityLookup.
pub fn despawn_and_remove(
    world: &mut World,
    entity: Entity,
    network_id: shared::entity::EntityId,
) {
    world.resource_mut::<EntityLookup>().0.remove(&network_id);
    if world.get_entity(entity).is_ok() {
        world.despawn(entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;
    use glam::Vec3;
    use shared::entity::{EntityState, EntityType, ServerId};

    fn setup_world() -> World {
        let mut world = World::default();
        world.init_resource::<EntityLookup>();
        world
    }

    #[test]
    fn roundtrip_npc() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::new(10.0, 0.0, -20.0), ServerId(1));
        let original_id = state.id;

        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Query components and convert back
        let nid = world.get::<NetworkId>(entity).unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        let vel = world.get::<Velocity>(entity).unwrap();
        let kind = world.get::<EntityKind>(entity).unwrap();
        let auth = world.get::<AuthorityServer>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let roundtripped = to_entity_state(nid, pos, vel, kind, auth, seq);

        assert_eq!(roundtripped.id, original_id);
        assert_eq!(roundtripped.position, Vec3::new(10.0, 0.0, -20.0));
        assert_eq!(roundtripped.velocity, Vec3::ZERO);
        assert_eq!(roundtripped.entity_type, EntityType::Npc);
        assert_eq!(roundtripped.authority_server, ServerId(1));
        assert_eq!(roundtripped.last_input_seq, 0);
    }

    #[test]
    fn roundtrip_player() {
        let mut world = setup_world();
        let mut state = EntityState::new_player(Vec3::new(5.0, 0.0, 5.0), ServerId(2));
        state.last_input_seq = 99;

        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        let nid = world.get::<NetworkId>(entity).unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        let vel = world.get::<Velocity>(entity).unwrap();
        let kind = world.get::<EntityKind>(entity).unwrap();
        let auth = world.get::<AuthorityServer>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let roundtripped = to_entity_state(nid, pos, vel, kind, auth, seq);

        assert_eq!(roundtripped.entity_type, EntityType::Player);
        assert_eq!(roundtripped.last_input_seq, 99);
        assert_eq!(roundtripped.authority_server, ServerId(2));
    }

    #[test]
    fn authoritative_marker() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        assert!(world.get::<Authoritative>(entity).is_some());
        assert!(world.get::<Shadow>(entity).is_none());
    }

    #[test]
    fn shadow_marker_with_tick() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(42));

        assert!(world.get::<Authoritative>(entity).is_none());
        let shadow = world.get::<Shadow>(entity).unwrap();
        assert_eq!(shadow.last_seen_tick, 42);
    }

    #[test]
    fn lookup_insertion() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        let lookup = world.resource::<EntityLookup>();
        assert_eq!(lookup.0.get(&eid), Some(&entity));
    }

    #[test]
    fn despawn_removes_from_lookup() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        despawn_and_remove(&mut world, entity, eid);
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
        assert!(world.get_entity(entity).is_err());
    }

    #[test]
    fn zero_last_input_seq_for_npc() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // NPC should not have LastInputSeq component
        assert!(world.get::<LastInputSeq>(entity).is_none());

        // But roundtrip should produce 0
        let nid = world.get::<NetworkId>(entity).unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        let vel = world.get::<Velocity>(entity).unwrap();
        let kind = world.get::<EntityKind>(entity).unwrap();
        let auth = world.get::<AuthorityServer>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let result = to_entity_state(nid, pos, vel, kind, auth, seq);
        assert_eq!(result.last_input_seq, 0);
    }

    #[test]
    fn nonzero_last_input_seq_for_player() {
        let mut world = setup_world();
        let mut state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        state.last_input_seq = 999;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        let seq = world.get::<LastInputSeq>(entity).unwrap();
        assert_eq!(seq.0, 999);
    }

    #[test]
    fn shadow_with_none_tick_defaults_to_zero() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, None);

        let shadow = world.get::<Shadow>(entity).unwrap();
        assert_eq!(shadow.last_seen_tick, 0);
    }

    #[test]
    fn despawn_already_gone_entity_does_not_panic() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // First despawn
        despawn_and_remove(&mut world, entity, eid);
        // Second despawn of same entity should not panic
        despawn_and_remove(&mut world, entity, eid);
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn double_spawn_same_network_id_overwrites_lookup() {
        let mut world = setup_world();
        let state = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(1));
        let eid = state.id;
        let entity1 = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Spawn again with same EntityId but different position
        let mut state2 = state.clone();
        state2.position = Vec3::new(99.0, 0.0, 99.0);
        let entity2 = spawn_from_entity_state_direct(&mut world, &state2, true, None);

        // Lookup should point to the second entity
        let lookup = world.resource::<EntityLookup>();
        assert_eq!(lookup.0.get(&eid), Some(&entity2));
        assert_ne!(entity1, entity2);

        // First entity still exists in ECS but is orphaned from the lookup
        assert!(world.get::<Position>(entity1).is_some());
        assert_eq!(world.get::<Position>(entity2).unwrap().0.x, 99.0);
    }

    #[test]
    fn spawn_player_shadow_has_last_input_seq() {
        let mut world = setup_world();
        let mut state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        state.last_input_seq = 55;
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        // Player shadow should have LastInputSeq
        let seq = world.get::<LastInputSeq>(entity).unwrap();
        assert_eq!(seq.0, 55);
        // And should be Shadow, not Authoritative
        assert!(world.get::<Shadow>(entity).is_some());
        assert!(world.get::<Authoritative>(entity).is_none());
    }

    #[test]
    fn roundtrip_preserves_velocity() {
        let mut world = setup_world();
        let mut state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        state.velocity = Vec3::new(3.0, -1.0, 7.5);
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        let nid = world.get::<NetworkId>(entity).unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        let vel = world.get::<Velocity>(entity).unwrap();
        let kind = world.get::<EntityKind>(entity).unwrap();
        let auth = world.get::<AuthorityServer>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let result = to_entity_state(nid, pos, vel, kind, auth, seq);
        assert_eq!(result.velocity, Vec3::new(3.0, -1.0, 7.5));
    }
}
