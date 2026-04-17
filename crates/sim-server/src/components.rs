use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityId, EntityType, ServerId};

/// Network-visible UUID identity. Maps between Bevy Entity and network EntityId.
#[derive(Component, Debug, Clone, Copy)]
pub struct NetworkId(pub EntityId);

/// Spatial position in world coordinates.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Position(pub Vec3);

/// Movement velocity (units per second).
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Velocity(pub Vec3);

/// What kind of entity this is.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityTypeComp(pub EntityType);

/// Which server is authoritative for this entity.
#[derive(Component, Debug, Clone, Copy)]
pub struct AuthorityServer(pub ServerId);

/// Marker: this server is authoritative for this entity.
#[derive(Component, Debug, Default)]
pub struct Authoritative;

/// Marker + data: this is a shadow copy from a peer server.
#[derive(Component, Debug)]
pub struct Shadow {
    pub last_seen_tick: u64,
}

/// Last acknowledged input sequence number. Only present on player entities.
#[derive(Component, Debug, Default)]
pub struct LastInputSeq(pub u32);

/// Marker: this entity is being transferred to another server via two-phase protocol.
/// The source continues holding it until the ACK arrives or timeout.
/// While marked, the entity is frozen: excluded from simulation and input processing,
/// but STILL replicated to peers so shadows remain up-to-date for crash recovery.
/// Inputs arriving for this entity are forwarded to the target server.
#[derive(Component, Debug)]
pub struct TransferOutbound {
    pub target_server: u16,
    pub target_addr: std::net::SocketAddr,
    pub sent_tick: u64,
    pub retry_count: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;

    #[test]
    fn spawn_entity_with_all_components_and_query() {
        let mut world = World::default();
        let eid = EntityId::new();

        let entity = world
            .spawn((
                NetworkId(eid),
                Position(Vec3::new(10.0, 0.0, -20.0)),
                Velocity(Vec3::new(1.0, 0.0, 0.0)),
                EntityTypeComp(EntityType::Npc),
                AuthorityServer(ServerId(1)),
                Authoritative,
            ))
            .id();

        // Query it back
        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::new(10.0, 0.0, -20.0));
        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::new(1.0, 0.0, 0.0));
        let kind = world.get::<EntityTypeComp>(entity).unwrap();
        assert_eq!(kind.0, EntityType::Npc);
        let auth = world.get::<AuthorityServer>(entity).unwrap();
        assert_eq!(auth.0, ServerId(1));
        let nid = world.get::<NetworkId>(entity).unwrap();
        assert_eq!(nid.0, eid);
        assert!(world.get::<Authoritative>(entity).is_some());
    }

    #[test]
    fn authoritative_vs_shadow_query_filter() {
        let mut world = World::default();

        // Spawn authoritative entity
        world.spawn((
            NetworkId(EntityId::new()),
            Position(Vec3::ZERO),
            Velocity(Vec3::ZERO),
            EntityTypeComp(EntityType::Npc),
            AuthorityServer(ServerId(1)),
            Authoritative,
        ));

        // Spawn shadow entity
        world.spawn((
            NetworkId(EntityId::new()),
            Position(Vec3::X),
            Velocity(Vec3::ZERO),
            EntityTypeComp(EntityType::Npc),
            AuthorityServer(ServerId(2)),
            Shadow { last_seen_tick: 10 },
        ));

        // With<Authoritative> filter
        let mut auth_query = world.query_filtered::<&Position, With<Authoritative>>();
        let auth_results: Vec<_> = auth_query.iter(&world).collect();
        assert_eq!(auth_results.len(), 1);
        assert_eq!(auth_results[0].0, Vec3::ZERO);

        // With<Shadow> filter
        let mut shadow_query = world.query_filtered::<&Position, With<Shadow>>();
        let shadow_results: Vec<_> = shadow_query.iter(&world).collect();
        assert_eq!(shadow_results.len(), 1);
        assert_eq!(shadow_results[0].0, Vec3::X);
    }

    #[test]
    fn last_input_seq_optional_on_npc() {
        let mut world = World::default();

        // NPC without LastInputSeq
        world.spawn((
            NetworkId(EntityId::new()),
            Position(Vec3::ZERO),
            Velocity(Vec3::ZERO),
            EntityTypeComp(EntityType::Npc),
            AuthorityServer(ServerId(1)),
            Authoritative,
        ));

        // Player with LastInputSeq
        world.spawn((
            NetworkId(EntityId::new()),
            Position(Vec3::ZERO),
            Velocity(Vec3::ZERO),
            EntityTypeComp(EntityType::Player),
            AuthorityServer(ServerId(1)),
            Authoritative,
            LastInputSeq(42),
        ));

        let mut query = world.query::<(&EntityTypeComp, Option<&LastInputSeq>)>();
        let results: Vec<_> = query.iter(&world).collect();
        assert_eq!(results.len(), 2);

        for (kind, seq) in &results {
            match kind.0 {
                EntityType::Npc => assert!(seq.is_none()),
                EntityType::Player => {
                    assert!(seq.is_some());
                    assert_eq!(seq.unwrap().0, 42);
                }
            }
        }
    }

    #[test]
    fn transfer_outbound_component_query() {
        let mut world = World::default();
        let eid = EntityId::new();
        let entity = world
            .spawn((
                NetworkId(eid),
                Position(Vec3::ZERO),
                Velocity(Vec3::ZERO),
                EntityTypeComp(EntityType::Npc),
                AuthorityServer(ServerId(1)),
                Authoritative,
                TransferOutbound {
                    target_server: 2,
                    target_addr: "127.0.0.1:9102".parse().unwrap(),
                    sent_tick: 10,
                    retry_count: 0,
                },
            ))
            .id();

        let outbound = world.get::<TransferOutbound>(entity).unwrap();
        assert_eq!(outbound.target_server, 2);
        assert_eq!(outbound.sent_tick, 10);
        assert_eq!(outbound.retry_count, 0);
        assert!(world.get::<Authoritative>(entity).is_some());
    }

    #[test]
    fn shadow_component_stores_tick() {
        let mut world = World::default();
        let entity = world
            .spawn((
                NetworkId(EntityId::new()),
                Position(Vec3::ZERO),
                Velocity(Vec3::ZERO),
                EntityTypeComp(EntityType::Npc),
                AuthorityServer(ServerId(2)),
                Shadow { last_seen_tick: 99 },
            ))
            .id();

        let shadow = world.get::<Shadow>(entity).unwrap();
        assert_eq!(shadow.last_seen_tick, 99);
    }
}
