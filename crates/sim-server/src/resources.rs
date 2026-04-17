use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::protocol::PeerInfo;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;


/// Frame sent through per-client channels. Each frame is a binary-encoded
/// snapshot or delta, sent as `Vec<u8>` through the per-client mpsc channel.
pub type ClientFrame = Vec<u8>;

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
    /// Client inputs paired with the client_id that sent them.
    /// client_id of 0 means server-initiated (e.g., auto-spawn on connect).
    pub client_inputs: Vec<(u64, shared::protocol::ClientMessage)>,
    pub forwarded_removes: Vec<ForwardedRemove>,
    /// Client registrations from handle_client tasks.
    pub register_clients: Vec<ClientRegistrationEvent>,
    /// Client unregistrations on disconnect.
    pub unregister_clients: Vec<u64>,
    /// Client-to-player ID assignments (set when SpawnPlayer is processed).
    pub client_player_assignments: Vec<(u64, EntityId)>,
    /// Clients requesting a forced full snapshot (RequestSnapshot).
    pub force_full_snapshot_clients: Vec<u64>,
    /// Acknowledgements of accepted authority transfers from target servers.
    pub transfer_acks: Vec<InboundTransferAck>,
}

/// Event pushed by handle_client when a new WebSocket client connects.
pub struct ClientRegistrationEvent {
    pub client_id: u64,
    pub tx: tokio::sync::mpsc::Sender<ClientFrame>,
    pub acked_tick: Arc<AtomicU64>,
    pub player_uuid: EntityId,
}

/// Coordinator events received between ticks.
#[derive(Resource, Default)]
pub struct CoordEventQueue {
    pub peer_list_updates: Vec<Vec<PeerInfo>>,
    pub rebalance_directives: Vec<(Vec<EntityId>, ServerId)>,
    pub drain_directives: Vec<Vec<ServerId>>,
    pub recover_events: Vec<(ServerId, Vec<ServerId>)>,
}

/// Outbound replication data, written by prepare_replication system,
/// consumed by the async UDP sender task.
///
/// On normal ticks, `per_peer` contains spatially filtered entity lists
/// per peer, and `full_snapshot` is None.
/// When filtering provides no benefit (e.g., no shadows to compute bounds from),
/// `full_snapshot` is populated and `per_peer` is empty.
#[derive(Resource, Default)]
pub struct OutboundReplication {
    /// Per-peer filtered entity lists. Key is peer server_id (u16).
    /// On hot-only ticks, contains only hot-tier entities.
    /// On cold ticks (tick % 4 == 0), contains hot + cold (all entities).
    pub per_peer: HashMap<u16, Vec<EntityState>>,
    /// Full entity list for broadcast. Set on full-replication ticks (if enabled)
    /// or when filtering provides no benefit (e.g., no shadows to compute bounds from).
    pub full_snapshot: Option<Vec<EntityState>>,
    pub tick: u64,
    /// Count of hot-tier entities sent this tick (sum across all peers).
    pub hot_count: usize,
    /// Count of cold-tier entities sent this tick (sum across all peers, 0 on non-cold ticks).
    pub cold_count: usize,
}

/// Pending authority transfers to send this tick.
#[derive(Resource, Default)]
pub struct PendingTransfers {
    pub transfers: Option<(Vec<EntityId>, ServerId)>,
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
    pub targets: Option<Vec<ServerId>>,
    /// Tick when drain was started. Used for drain timeout.
    pub started_tick: Option<u64>,
}

/// Shutdown requested (set when drain completes).
#[derive(Resource, Default)]
pub struct ShutdownRequested(pub bool);

/// Outbound messages to send to the coordinator (e.g., TransferResult).
/// Pushed by ECS systems, drained by the coordinator_client async task.
#[derive(Resource, Default)]
pub struct OutboundCoordMessages {
    pub messages: Vec<shared::protocol::CoordClientMsg>,
}

/// Tracks which client spawned which entity. Used to validate RemoveEntity
/// so one client can't delete another's entities. Server-to-server removes
/// (ForwardedRemove) bypass this check.
#[derive(Resource, Default)]
pub struct EntityOwnership {
    pub owners: HashMap<EntityId, u64>,
}

/// Per-client channel size. 8 frames = 400ms at 20Hz -- enough buffer for brief lag spikes.
pub const PER_CLIENT_CHANNEL_SIZE: usize = 8;

/// Ticks without ACK progress before forcing a full snapshot.
/// 40 ticks = 2 seconds at 20Hz. Faster recovery than the 5-second periodic full
/// (FULL_SNAPSHOT_INTERVAL = 100 ticks). Detects clients that stopped processing frames
/// (e.g., browser tab backgrounded).
pub const ACK_STALE_TICKS: u64 = 40;

/// Registry of connected clients, each with its own frame channel and delta state.
#[derive(Resource, Default)]
pub struct ClientRegistry {
    pub clients: HashMap<u64, ClientHandle>,
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
                last_sent_tick: 0,
            },
        );
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
    /// Used by prepare_client_snapshot for stale-ACK detection (audit #021).
    pub acked_tick: Arc<AtomicU64>,
    /// Baseline entity state for delta computation. None until first full snapshot sent.
    pub baseline: Option<HashMap<EntityId, EntityState>>,
    /// Ticks since last forced full snapshot.
    pub ticks_since_full: u32,
    /// Tick of the most recently sent frame. Used with acked_tick for stale-ACK detection.
    pub last_sent_tick: u64,
}

// -- Inbound event types --

pub struct PeerUpdate {
    pub tick: u64,
    pub entities: Vec<EntityState>,
}

pub struct InboundTransfer {
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

/// Inbound acknowledgement that an authority transfer was accepted by the target.
pub struct InboundTransferAck {
    pub entity_id: EntityId,
    pub tick: u64,
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
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();

        assert_eq!(world.resource::<TickCounter>().0, 0);
        assert!(world.resource::<PeerAddresses>().0.is_empty());
        assert!(world.resource::<EntityLookup>().0.is_empty());
        assert!(world.resource::<InboundQueue>().peer_updates.is_empty());
        assert!(world.resource::<InboundQueue>().transfer_acks.is_empty());
        assert!(world.resource::<CoordEventQueue>().peer_list_updates.is_empty());
        assert!(world.resource::<OutboundReplication>().full_snapshot.is_none());
        assert!(world.resource::<OutboundReplication>().per_peer.is_empty());
        assert_eq!(world.resource::<OutboundReplication>().hot_count, 0);
        assert_eq!(world.resource::<OutboundReplication>().cold_count, 0);
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
