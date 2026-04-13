use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::protocol::PeerInfo;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::delta::CompactEntityState;

/// Frame sent through per-client channels. Full snapshots are reference-counted
/// (shared across all clients) to avoid cloning the same bytes N times.
/// Delta frames are unique per client and owned directly.
#[derive(Debug)]
pub enum ClientFrame {
    /// Full snapshot shared across all clients via Arc (zero-copy).
    Shared(Arc<Vec<u8>>),
    /// Per-client delta frame (unique content).
    Owned(Vec<u8>),
}

impl ClientFrame {
    /// Get a reference to the underlying bytes.
    #[allow(dead_code)]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            ClientFrame::Shared(arc) => arc.as_slice(),
            ClientFrame::Owned(vec) => vec.as_slice(),
        }
    }

    /// Convert into owned bytes, cloning from Arc only if necessary.
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            ClientFrame::Shared(arc) => Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()),
            ClientFrame::Owned(vec) => vec,
        }
    }
}

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
    /// Client registrations from handle_client tasks.
    pub register_clients: Vec<ClientRegistrationEvent>,
    /// Client unregistrations on disconnect.
    pub unregister_clients: Vec<u64>,
    /// Client-to-player ID assignments (set when SpawnPlayer is processed).
    pub client_player_assignments: Vec<(u64, EntityId)>,
    /// Clients requesting a forced full snapshot (RequestSnapshot).
    pub force_full_snapshot_clients: Vec<u64>,
}

/// Event pushed by handle_client when a new WebSocket client connects.
pub struct ClientRegistrationEvent {
    pub client_id: u64,
    pub tx: tokio::sync::mpsc::Sender<ClientFrame>,
    pub acked_tick: Arc<AtomicU64>,
    pub player_uuid: String,
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

/// Per-client channel size. 8 frames = 400ms at 20Hz -- enough buffer for brief lag spikes.
pub const PER_CLIENT_CHANNEL_SIZE: usize = 8;

/// Registry of connected clients, each with its own frame channel and delta state.
#[derive(Resource, Default)]
pub struct ClientRegistry {
    pub clients: HashMap<u64, ClientHandle>,
    next_id: u64,
}

impl ClientRegistry {
    /// Register a new client from a pre-built event. Returns the assigned client ID.
    pub fn register_from_event(
        &mut self,
        client_id: u64,
        tx: tokio::sync::mpsc::Sender<ClientFrame>,
        acked_tick: Arc<AtomicU64>,
    ) {
        self.clients.insert(
            client_id,
            ClientHandle {
                tx,
                player_id: None,
                acked_tick,
                baseline: None,
                ticks_since_full: 0,
            },
        );
        if client_id >= self.next_id {
            self.next_id = client_id + 1;
        }
    }

    pub fn unregister(&mut self, id: u64) {
        self.clients.remove(&id);
    }
}

/// Per-client handle stored in the ECS ClientRegistry.
pub struct ClientHandle {
    /// Per-client frame channel sender (accepts shared or owned frames).
    pub tx: tokio::sync::mpsc::Sender<ClientFrame>,
    /// The player entity owned by this client (set on SpawnPlayer).
    pub player_id: Option<EntityId>,
    /// Last tick ACK'd by this client. Written by handle_client (async), read by game loop (ECS).
    /// Currently used for diagnostics; future: conditional baseline confirmation.
    #[allow(dead_code)]
    pub acked_tick: Arc<AtomicU64>,
    /// Baseline entity state for delta computation. None until first full snapshot sent.
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    /// Ticks since last forced full snapshot.
    pub ticks_since_full: u32,
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
