use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::protocol::PeerInfo;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

/// Frame sent through per-client channels. A frame is either a binary-encoded
/// snapshot/delta (every tick) or a JSON text payload (rare: currently only
/// `ServerMessage::Reconnect` during player migration — see WI-201 + WI-203).
///
/// WI-201 (design 016, Batch 4): before this change, ClientFrame was a plain
/// `Vec<u8>` that always produced `Message::Binary` on the wire. Player
/// migration needs to interleave a single JSON text frame mid-session. Keeping
/// the producer ECS-system-local (no async) means the ECS pushes a typed frame
/// and the async task in `network.rs` dispatches to the correct WebSocket
/// message type.
#[derive(Debug, Clone)]
pub enum ClientFrame {
    /// Binary snapshot or delta. The every-tick hot path.
    Binary(tokio_tungstenite::tungstenite::Bytes),
    /// JSON text frame. Used for mid-session server→client control messages
    /// like `ServerMessage::Reconnect { url }`.
    Text(String),
}

impl ClientFrame {
    /// View the binary payload. Panics if this is a Text frame.
    /// Primarily a test helper — production code matches on the enum.
    #[cfg(test)]
    pub fn as_binary(&self) -> &[u8] {
        match self {
            ClientFrame::Binary(b) => b.as_ref(),
            ClientFrame::Text(s) => {
                panic!("expected ClientFrame::Binary, got ClientFrame::Text({})", s)
            }
        }
    }

    /// Consume and return the binary bytes. Panics on Text (tests only).
    #[cfg(test)]
    pub fn into_binary(self) -> Vec<u8> {
        match self {
            ClientFrame::Binary(b) => b.to_vec(),
            ClientFrame::Text(s) => {
                panic!("expected ClientFrame::Binary, got ClientFrame::Text({})", s)
            }
        }
    }
}

/// WI-201: Deref into the binary payload only in test builds. Lets existing
/// test code that indexes `frame[..]` or calls `frame.len()` keep compiling
/// after the Binary/Text split. Production code matches on the enum and never
/// relies on Deref.
#[cfg(test)]
impl std::ops::Deref for ClientFrame {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_binary()
    }
}

/// Server identity and configuration.
#[derive(Resource)]
pub struct ServerConfig {
    pub server_id: ServerId,
}
/// All workers in a demo world use the same XZ bounds.
pub fn world_half_extent() -> f32 {
    static EXTENT: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *EXTENT.get_or_init(|| match std::env::var("MESH_WORLD_EXTENT") {
        Ok(value) => value
            .parse::<f32>()
            .ok()
            .filter(|n| (1.0..=1_000_000.0).contains(n))
            .expect("MESH_WORLD_EXTENT must be a finite half-width in 1..1000000"),
        Err(std::env::VarError::NotPresent) => shared::spatial::WORLD_EXTENT,
        Err(_) => panic!("invalid MESH_WORLD_EXTENT"),
    })
}

/// Unique per-process session identifier. Generated at sim-server boot from
/// `rand::random::<u64>()` and carried on every outbound `PeerMessage` so peers
/// can detect when this server has restarted and reset stale receiver state
/// (see design 013: peer-restart-recovery).
#[derive(Resource, Clone, Copy, Debug)]
pub struct SessionId(pub u64);

/// Monotonically increasing tick counter.
#[derive(Resource, Default)]
pub struct TickCounter(pub u64);

/// UDP addresses of peer servers.
#[derive(Resource, Default)]
pub struct PeerAddresses(pub HashMap<u16, SocketAddr>);

/// Full WebSocket URLs of peer servers, keyed by server_id.
/// Populated from `PeerList` updates using the `PeerInfo.tls` flag to
/// choose `ws://` vs `wss://`. Source servers use this to construct the
/// target URL when sending `ServerMessage::Reconnect { url }` during
/// player migration (design 016, Batch 4, WI-202 + WI-203).
#[derive(Resource, Default)]
pub struct PeerWsAddresses(pub HashMap<u16, String>);

/// WI-213 (design 016, Batch 5): shared set of currently-active peer
/// `server_id` values. The ECS writes this from `process_coord_events`
/// on each `PeerList` broadcast. The UDP receiver reads it during the
/// periodic sweep (`GLOBAL_SWEEP_INTERVAL`) to prune state for peers
/// that have departed the mesh, keeping `session_of_peer`,
/// `session_history`, and `latest_tick` bounded by the active peer
/// count rather than lifetime restart count.
///
/// `Arc<RwLock<...>>` over `Mutex`: writes are rare (once per PeerList
/// event, typically seconds apart) and reads are on the receiver's
/// sweep interval. The `RwLock` lets reads stay cheap without
/// contention with the tick loop.
#[derive(Resource, Default, Clone)]
pub struct ActivePeers(pub Arc<std::sync::RwLock<std::collections::HashSet<u16>>>);

/// Reverse lookup: NetworkId (EntityId) -> Bevy Entity handle.
#[derive(Resource, Default)]
pub struct EntityLookup(pub HashMap<EntityId, bevy_ecs::entity::Entity>);

/// Queue of inbound events from async tasks (UDP receiver, WebSocket, coordinator).
/// Drained at the start of each tick.
#[derive(Resource, Default)]
pub struct InboundQueue {
    pub(crate) peer_interests: HashMap<u16, crate::peer_interest::Advert>,
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
    /// Signals from the UDP receiver that a peer's session_id changed (peer
    /// restart detected). Downstream ECS systems can consume these to reset
    /// per-peer state (e.g., shadow last_peer_tick — see WI-135 + design 015).
    pub peer_session_resets: Vec<PeerSessionReset>,
}

/// Event emitted by the UDP receiver when it observes a peer replicating with
/// a new `session_id` that differs from the previously-known session for that
/// `server_id`. Used to signal the ECS that the peer has restarted so stale
/// per-peer state (shadow last_peer_tick, etc.) can be reset.
///
/// See design 013 (peer-restart-recovery) and design 015 (shadow-tick-split).
/// Consumed by `systems::inbound::drain_inbound_events` (WI-135), which zeroes
/// `Shadow.last_peer_tick` for every shadow whose `AuthorityServer == server_id`
/// so the first post-restart update from the peer is accepted. `last_local_tick`
/// is intentionally NOT zeroed — that field lives in our local clock domain.
#[derive(Debug, Clone, Copy)]
pub struct PeerSessionReset {
    pub server_id: ServerId,
    pub new_session_id: u64,
}

/// Event pushed by handle_client when a new WebSocket client connects.
pub struct ClientRegistrationEvent {
    pub client_id: u64,
    pub tx: tokio::sync::mpsc::Sender<ClientFrame>,
    /// Last tick ACK'd by this client (delta compression, WI-220 restore).
    pub acked_tick: Arc<AtomicU64>,
    pub player_uuid: EntityId,
    /// Shared flag the source server flips when it sends `Reconnect` so the
    /// close handler can suppress `RemoveEntity` on the migrating client's
    /// disconnect (see WI-202 + WI-203). Defaults to `false`.
    pub migrating_out: Arc<AtomicBool>,
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
    pub(crate) discovery: crate::peer_interest::Discovery,
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

/// Info about an active drain operation.
///
/// `started_tick` is `None` until the first `process_drain` run stamps it
/// (WI-190). Using `Option<u64>` instead of a `0` sentinel avoids a latent
/// bug where a SIGINT on tick 0 would re-stamp the drain start every tick
/// and the timeout would never fire.
pub struct DrainInfo {
    pub targets: Vec<ServerId>,
    pub started_tick: Option<u64>,
}

/// Graceful drain state.
#[derive(Resource, Default)]
pub struct DrainState(pub Option<DrainInfo>);

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
    /// Heartbeat interval for an unchanged view. Zero or one sends every tick.
    pub idle_snapshot_ticks: u8,
    /// Per-client output interval. Rotation spreads recipients over simulation ticks.
    pub snapshot_every_ticks: u8,
    /// Cumulative snapshot opportunities skipped because a recipient's channel was full.
    pub snapshot_queue_full: u64,
    /// Elapsed time collecting, encoding and enqueueing client snapshots on this tick.
    pub snapshot_time: std::time::Duration,
}

impl ClientRegistry {
    /// Register a new client from a pre-built event. Returns the assigned client ID.
    /// Test-only: production uses `register_from_event_with_migrating` so the
    /// `migrating_out` flag is shared with the network task. Tests that don't
    /// care about migration use this for brevity.
    #[cfg(test)]
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
                migrating_out: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    /// Register a new client with an externally-owned `migrating_out` flag.
    /// Used by the network task so the shared flag can be flipped from either
    /// side (ECS sets it in `process_transfers`; close handler reads it).
    pub fn register_from_event_with_migrating(
        &mut self,
        client_id: u64,
        tx: tokio::sync::mpsc::Sender<ClientFrame>,
        acked_tick: Arc<AtomicU64>,
        migrating_out: Arc<AtomicBool>,
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
                migrating_out,
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
    pub baseline: Option<Arc<HashMap<EntityId, EntityState>>>,
    /// Ticks since last forced full snapshot.
    pub ticks_since_full: u32,
    /// Tick of the most recently sent frame. Used with acked_tick for stale-ACK detection.
    pub last_sent_tick: u64,
    /// Set to `true` by the source server when it sends `ServerMessage::Reconnect`
    /// to this client (player migration). Read by the async close handler in
    /// `network.rs` so it can suppress `RemoveEntity` when the client
    /// subsequently disconnects — the target server now owns the entity, so
    /// the source removing it would cause a visible gap.
    /// Wrapped in `Arc<AtomicBool>` so the ECS system (`process_transfers`)
    /// and the async task can share the flag without a channel.
    /// See WI-202 + WI-203.
    pub migrating_out: Arc<AtomicBool>,
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
        assert!(world
            .resource::<InboundQueue>()
            .peer_session_resets
            .is_empty());
        assert!(world
            .resource::<CoordEventQueue>()
            .peer_list_updates
            .is_empty());
        assert!(world
            .resource::<OutboundReplication>()
            .full_snapshot
            .is_none());
        assert!(world.resource::<OutboundReplication>().per_peer.is_empty());
        assert_eq!(world.resource::<OutboundReplication>().hot_count, 0);
        assert_eq!(world.resource::<OutboundReplication>().cold_count, 0);
        assert!(world.resource::<PendingTransfers>().transfers.is_none());
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
        assert!(world.resource::<OutboundForwards>().messages.is_empty());
        assert!(world.resource::<DrainState>().0.is_none());
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
    fn session_id_resource_can_be_inserted_and_read() {
        let mut world = World::default();
        world.insert_resource(SessionId(0xDEADBEEFCAFEBABE));
        let sid = *world.resource::<SessionId>();
        assert_eq!(sid.0, 0xDEADBEEFCAFEBABE);
    }

    #[test]
    fn session_id_is_copy_and_debug() {
        // Copy check: binding the resource should not move it.
        let sid = SessionId(42);
        let _a = sid; // Copy
        let _b = sid; // Still usable
        assert_eq!(sid.0, 42);
        // Debug must format without panicking.
        let debug_str = format!("{:?}", sid);
        assert!(
            debug_str.contains("42"),
            "Debug output should contain the value"
        );
    }

    #[test]
    fn session_id_random_generation_differs() {
        // Two independent invocations of rand::random::<u64>() should
        // overwhelmingly differ. Collision probability is 1 / 2^64.
        let a: u64 = rand::random();
        let b: u64 = rand::random();
        assert_ne!(a, b, "two random u64 values should differ");
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
        assert_eq!(world.resource::<InboundQueue>().forwarded_inputs.len(), 1);

        // Drain
        let inputs: Vec<_> =
            std::mem::take(&mut world.resource_mut::<InboundQueue>().forwarded_inputs);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].seq, 5);
        assert!(world.resource::<InboundQueue>().forwarded_inputs.is_empty());
    }
}
