use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::protocol::PeerInfo;
use std::collections::HashMap;
use std::net::SocketAddr;

/// Server identity and configuration.
#[derive(Resource)]
pub struct ServerConfig {
    pub server_id: ServerId,
}

/// Monotonically increasing tick counter.
#[derive(Resource, Default)]
pub struct TickCounter(pub u64);

/// UDP addresses of peer servers.
#[derive(Resource, Default)]
pub struct PeerAddresses(pub HashMap<u16, SocketAddr>);

/// Reverse lookup: NetworkId (EntityId) -> Bevy Entity handle.
#[derive(Resource, Default)]
pub struct EntityLookup(pub HashMap<EntityId, bevy_ecs::entity::Entity>);

/// Queue of inbound events from async tasks (UDP receiver, WebSocket, coordinator).
/// Drained at the start of each tick.
#[derive(Resource, Default)]
pub struct InboundQueue {
    pub peer_updates: Vec<PeerUpdate>,
    pub authority_transfers: Vec<InboundTransfer>,
    pub forwarded_inputs: Vec<ForwardedInput>,
    pub client_inputs: Vec<shared::protocol::ClientMessage>,
    pub forwarded_removes: Vec<ForwardedRemove>,
}

/// Coordinator events received between ticks.
#[derive(Resource, Default)]
pub struct CoordEventQueue {
    pub peer_list_updates: Vec<Vec<PeerInfo>>,
    pub rebalance_directives: Vec<(Vec<String>, u16)>,
    pub drain_directives: Vec<Vec<u16>>,
    pub recover_events: Vec<(u16, Vec<u16>)>,
}

/// Outbound replication data, written by prepare_replication system,
/// consumed by the async UDP sender task.
#[derive(Resource, Default)]
pub struct OutboundReplication {
    pub snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
}

/// Outbound client snapshot, written by prepare_client_snapshot system,
/// consumed by the async WebSocket broadcast task.
#[derive(Resource, Default)]
pub struct ClientSnapshot {
    pub frame: Option<Vec<u8>>,
}

/// Pending authority transfers to send this tick.
#[derive(Resource, Default)]
pub struct PendingTransfers {
    pub transfers: Option<(Vec<String>, u16)>,
}

/// Outbound transfer messages to send via UDP.
#[derive(Resource, Default)]
pub struct OutboundTransfers {
    pub messages: Vec<(SocketAddr, Vec<u8>)>,
}

/// Outbound forwarded inputs to send via UDP.
#[derive(Resource, Default)]
pub struct OutboundForwards {
    pub messages: Vec<(SocketAddr, Vec<u8>)>,
}

/// Graceful drain state.
#[derive(Resource, Default)]
pub struct DrainState {
    pub draining: bool,
    pub targets: Option<Vec<u16>>,
}

/// Shutdown requested (set when drain completes).
#[derive(Resource, Default)]
pub struct ShutdownRequested(pub bool);

/// Per-tick timing accumulator for metrics (reserved for future per-system timing).
#[derive(Resource, Default)]
#[allow(dead_code)]
pub struct TimingAccumulator {
    pub phase_start: Option<std::time::Instant>,
    pub input_drain: std::time::Duration,
    pub simulation: std::time::Duration,
    pub transfers: std::time::Duration,
    pub replication: std::time::Duration,
    pub shadow_cleanup: std::time::Duration,
}

// -- Inbound event types --

pub struct PeerUpdate {
    pub tick: u64,
    #[allow(dead_code)]
    pub server_id: ServerId,
    pub entities: Vec<EntityState>,
}

pub struct InboundTransfer {
    #[allow(dead_code)]
    pub tick: u64,
    pub entity_id: EntityId,
    pub from_server: ServerId,
    pub to_server: ServerId,
    pub full_state: EntityState,
}

pub struct ForwardedInput {
    pub entity_id: EntityId,
    pub direction: [f32; 3],
    pub seq: u32,
}

pub struct ForwardedRemove {
    pub entity_id: EntityId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;
    use shared::entity::EntityId;

    #[test]
    fn resources_default_initialization() {
        let mut world = World::default();
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

        assert_eq!(world.resource::<TickCounter>().0, 0);
        assert!(world.resource::<PeerAddresses>().0.is_empty());
        assert!(world.resource::<EntityLookup>().0.is_empty());
        assert!(world.resource::<InboundQueue>().peer_updates.is_empty());
        assert!(world.resource::<CoordEventQueue>().peer_list_updates.is_empty());
        assert!(world.resource::<OutboundReplication>().snapshot.is_none());
        assert!(world.resource::<ClientSnapshot>().frame.is_none());
        assert!(world.resource::<PendingTransfers>().transfers.is_none());
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
        assert!(world.resource::<OutboundForwards>().messages.is_empty());
        assert!(!world.resource::<DrainState>().draining);
        assert!(!world.resource::<ShutdownRequested>().0);
    }

    #[test]
    fn server_config_resource() {
        let mut world = World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(3),
        });

        let config = world.resource::<ServerConfig>();
        assert_eq!(config.server_id, ServerId(3));
    }

    #[test]
    fn entity_lookup_insert_and_retrieve() {
        let mut world = World::default();
        world.init_resource::<EntityLookup>();

        let eid = EntityId::new();
        let entity = world.spawn_empty().id();

        world.resource_mut::<EntityLookup>().0.insert(eid, entity);
        let lookup = world.resource::<EntityLookup>();
        assert_eq!(lookup.0.get(&eid), Some(&entity));
    }

    #[test]
    fn inbound_queue_push_and_drain() {
        let mut world = World::default();
        world.init_resource::<InboundQueue>();

        let fwd = ForwardedInput {
            entity_id: EntityId::new(),
            direction: [1.0, 0.0, 0.0],
            seq: 5,
        };
        world
            .resource_mut::<InboundQueue>()
            .forwarded_inputs
            .push(fwd);
        assert_eq!(
            world.resource::<InboundQueue>().forwarded_inputs.len(),
            1
        );

        // Drain
        let inputs: Vec<_> = std::mem::take(
            &mut world.resource_mut::<InboundQueue>().forwarded_inputs,
        );
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].seq, 5);
        assert!(world.resource::<InboundQueue>().forwarded_inputs.is_empty());
    }
}
