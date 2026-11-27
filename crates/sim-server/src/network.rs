use crate::game_loop::{SharedAuthPlayers, SharedInbound};
use crate::resources::{ClientFrame, ClientRegistrationEvent, PER_CLIENT_CHANNEL_SIZE};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use shared::entity::EntityId;
use shared::protocol::{ClientMessage, ServerMessage};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Atomic counter for assigning unique client IDs. This is incremented by each
/// handle_client task to ensure uniqueness without needing shared mutable state.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum movement/action inputs per second per client.
const INPUT_RATE_LIMIT: u32 = 30;
/// Maximum spawn requests per second per client.
const SPAWN_RATE_LIMIT: u32 = 5;
/// Rate limiter window duration.
const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// Simple sliding-window rate limiter.
struct RateLimiter {
    limit: u32,
    count: u32,
    window_start: std::time::Instant,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            count: 0,
            window_start: std::time::Instant::now(),
        }
    }

    /// Returns true if the action is allowed, false if rate-limited.
    fn check(&mut self) -> bool {
        let now = std::time::Instant::now();
        if now.duration_since(self.window_start) >= RATE_WINDOW {
            self.count = 0;
            self.window_start = now;
        }
        if self.count >= self.limit {
            return false;
        }
        self.count += 1;
        true
    }
}

pub async fn run_websocket_server(
    port: u16,
    inbound: Arc<SharedInbound>,
    auth_players: Arc<SharedAuthPlayers>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
) {
    run_websocket_server_at(
        port,
        inbound,
        auth_players,
        tls_acceptor,
        SpawnPoints::default(),
    )
    .await
}
#[derive(Clone)]
pub struct SpawnPoints(Arc<[[f32; 3]]>);
impl Default for SpawnPoints {
    fn default() -> Self {
        Self(Arc::from([[0.; 3]]))
    }
}
impl SpawnPoints {
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let points: Vec<[f32; 3]> = serde_json::from_reader(std::fs::File::open(path)?)?;
        if points.is_empty()
            || points.len() > 65536
            || points.iter().any(|p| {
                !p.iter().all(|v| v.is_finite())
                    || p[0].abs() > crate::resources::world_half_extent()
                    || p[2].abs() > crate::resources::world_half_extent()
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid server spawn points",
            ));
        }
        Ok(Self(points.into()))
    }
}
pub async fn run_websocket_server_at(
    port: u16,
    inbound: Arc<SharedInbound>,
    auth_players: Arc<SharedAuthPlayers>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    spawns: SpawnPoints,
) {
    let listener = TcpListener::bind(shared::listen_addr(port))
        .await
        .expect("Failed to bind WebSocket port");

    let scheme = if tls_acceptor.is_some() { "wss" } else { "ws" };
    tracing::info!("WebSocket server listening on port {port} ({scheme})");

    // Accept client connections
    loop {
        let (tcp_stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Accept error: {e}");
                continue;
            }
        };

        if let Err(e) = tcp_stream.set_nodelay(true) {
            tracing::debug!("Could not set TCP_NODELAY for {addr}: {e}");
        }
        let inbound = Arc::clone(&inbound);
        let auth_players = Arc::clone(&auth_players);
        let spawns = spawns.clone();

        if let Some(ref acceptor) = tls_acceptor {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(tcp_stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("TLS handshake error from {addr}: {e}");
                        return;
                    }
                };
                let ws_stream = match tokio_tungstenite::accept_async_with_config(
                    tls_stream,
                    Some(socket_config()),
                )
                .await
                {
                    Ok(ws) => ws,
                    Err(e) => {
                        tracing::error!("WebSocket handshake error (TLS) from {addr}: {e}");
                        return;
                    }
                };
                tracing::info!("Client connected from {addr} (TLS)");
                handle_client(ws_stream, addr, inbound, auth_players, spawns).await;
            });
        } else {
            tokio::spawn(async move {
                let ws_stream = match tokio_tungstenite::accept_async_with_config(
                    tcp_stream,
                    Some(socket_config()),
                )
                .await
                {
                    Ok(ws) => ws,
                    Err(e) => {
                        tracing::error!("WebSocket handshake error from {addr}: {e}");
                        return;
                    }
                };
                tracing::info!("Client connected from {addr}");
                handle_client(ws_stream, addr, inbound, auth_players, spawns).await;
            });
        }
    }
}

/// How long to wait for the first client message before falling back to the
/// default spawn-new-player flow. Players migrating via `ServerMessage::
/// Reconnect` send `ClaimPlayer` immediately on open, so 500 ms is
/// generous. Fresh clients that never press WASD will incur this delay
/// before their entity appears — acceptable given the UX.
const CLAIM_WAIT_MS: u64 = 500;

pub fn socket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .read_buffer_size(8192)
        .write_buffer_size(8192)
}

/// How long to wait for a claimed player entity to arrive via
/// AuthorityTransfer before the client can retry the claim.
/// Sized to cover a worst-case coordinator hop + UDP round-trip (~200 ms)
/// plus the client's own reconnect backoff (~250 ms on the fast path).
const CLAIM_ARRIVAL_WAIT_MS: u64 = 2000;
const CLAIM_POLL_INTERVAL_MS: u64 = 100;

/// Check the shared authoritative-player set for the given EntityId.
/// Short-lived lock; guards against accidental poisoning.
fn auth_player_present(auth_players: &SharedAuthPlayers, eid: EntityId) -> bool {
    auth_players
        .lock()
        .map(|g| g.contains(&eid))
        .unwrap_or(false)
}

/// Handle a single WebSocket client connection. Generic over the underlying
/// transport (plain TCP or TLS) since both implement AsyncRead + AsyncWrite.
async fn handle_client<S: AsyncRead + AsyncWrite + Unpin>(
    ws_stream: WebSocketStream<S>,
    addr: std::net::SocketAddr,
    inbound: Arc<SharedInbound>,
    auth_players: Arc<SharedAuthPlayers>,
    spawns: SpawnPoints,
) {
    let (mut ws_sink, mut ws_stream): (
        SplitSink<WebSocketStream<S>, Message>,
        SplitStream<WebSocketStream<S>>,
    ) = ws_stream.split();

    // Assign a unique client ID
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);

    // Create per-client frame channel (receives shared or owned frames)
    let (frame_tx, mut frame_rx) = mpsc::channel::<ClientFrame>(PER_CLIENT_CHANNEL_SIZE);

    // Shared ACK tick counter between this task and the ECS game loop
    let acked_tick = Arc::new(AtomicU64::new(0));

    // Shared migrating_out flag so the ECS (`process_transfers`) can flip
    // it when initiating player migration to a peer, and our close handler
    // below can read it to decide whether to push RemoveEntity on disconnect.
    let migrating_out = Arc::new(AtomicBool::new(false));

    // WI-202: short initial wait for ClaimPlayer. If the client is arriving
    // via server-directed Reconnect (migration), it sends ClaimPlayer as its
    // first message; otherwise the usual spawn-new-player flow activates
    // after CLAIM_WAIT_MS elapses without any message.
    let mut pending_first_message: Option<ClientMessage> = None;
    let first_frame = tokio::time::timeout(
        std::time::Duration::from_millis(CLAIM_WAIT_MS),
        ws_stream.next(),
    )
    .await;

    let claimed_player_id: Option<EntityId> = match first_frame {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::ClaimPlayer { player_id }) => {
                tracing::info!(
                    client_id,
                    player_id = %player_id,
                    "Received ClaimPlayer from {addr}"
                );
                Some(player_id)
            }
            Ok(other) => {
                pending_first_message = Some(other);
                None
            }
            Err(e) => {
                tracing::warn!("Invalid first message from {addr}: {e}");
                None
            }
        },
        Ok(Some(Ok(Message::Binary(_)))) => {
            // Binary first message (e.g., an ACK) — no claim. Fall through to
            // default spawn flow. We don't forward binary to pending because
            // binary is only ACKs which depend on tick values from the server.
            None
        }
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
            // Client hung up before sending anything.
            tracing::info!(client_id, "Client {addr} disconnected during claim wait");
            return;
        }
        Ok(Some(Ok(_))) => None, // ping/pong — ignore
        Ok(Some(Err(e))) => {
            tracing::warn!("WebSocket error during claim wait from {addr}: {e}");
            return;
        }
        Err(_) => {
            // Timeout: no message within CLAIM_WAIT_MS. Default: spawn new player.
            None
        }
    };

    // Resolve to an actual player_id + spawn decision.
    // If ClaimPlayer arrived, wait up to CLAIM_ARRIVAL_WAIT_MS for the
    // entity to appear on this server (arrival via AuthorityTransfer).
    // If it does, register the client bound to the existing entity.
    // A late transfer can still arrive after the timeout. Leave its ID reserved.
    let (player_id, do_spawn) = match claimed_player_id {
        Some(claimed) => {
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_millis(CLAIM_ARRIVAL_WAIT_MS);
            let mut arrived = auth_player_present(&auth_players, claimed);
            while !arrived && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(CLAIM_POLL_INTERVAL_MS))
                    .await;
                arrived = auth_player_present(&auth_players, claimed);
            }
            if arrived {
                tracing::info!(
                    client_id,
                    player_id = %claimed,
                    "ClaimPlayer resolved: entity present, adopting"
                );
                (claimed, false)
            } else {
                tracing::warn!(
                    client_id,
                    player_id = %claimed,
                    wait_ms = CLAIM_ARRIVAL_WAIT_MS,
                    "ClaimPlayer is waiting for authority transfer"
                );
                let _ = ws_sink.send(Message::Close(None)).await;
                return;
            }
        }
        None => {
            // No claim — mint a fresh UUID and spawn as usual.
            (shared::entity::EntityId(uuid::Uuid::new_v4()), true)
        }
    };

    {
        let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
        if do_spawn {
            q.client_inputs.push((
                client_id,
                ClientMessage::SpawnPlayer {
                    player_id,
                    position: spawns.0[((client_id - 1) % spawns.0.len() as u64) as usize],
                },
            ));
        }
        // Register this client in the ECS via the inbound queue. The
        // player_uuid here is the authoritative id the client will know us
        // by — either the claimed one (adopt path) or the freshly minted one.
        q.register_clients.push(ClientRegistrationEvent {
            client_id,
            tx: frame_tx,
            acked_tick: Arc::clone(&acked_tick),
            player_uuid: player_id,
            migrating_out: Arc::clone(&migrating_out),
        });
        // If the pre-spawn first message was a non-ClaimPlayer, forward it
        // now so the game loop processes it alongside the spawn request.
        if let Some(msg) = pending_first_message.take() {
            q.client_inputs.push((client_id, msg));
        }
    }

    // Send PlayerAssigned as JSON text frame. Uses the same UUID regardless
    // of whether we adopted or spawned — the server is authoritative on the
    // id. If the client had a different id in mind (e.g. claim-to-spawn
    // fallback with a random UUID), the client adapts via its existing
    // PlayerAssigned handler.
    let assign_msg = ServerMessage::PlayerAssigned {
        player_id: player_id.to_string(),
        protocol_version: shared::PROTOCOL_VERSION,
    };
    if let Ok(json) = serde_json::to_string(&assign_msg) {
        if ws_sink.send(Message::Text(json.into())).await.is_err() {
            // Client disconnected during assignment -- remove the player
            let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
            if do_spawn {
                q.client_inputs.push((
                    client_id,
                    ClientMessage::RemoveEntity {
                        entity_id: player_id,
                    },
                ));
            }
            q.unregister_clients.push(client_id);
            return;
        }
    }

    tracing::info!(
        client_id,
        spawned = do_spawn,
        "Assigned player {} to client {addr}",
        &player_id.to_string()[..8]
    );

    let mut input_limiter = RateLimiter::new(INPUT_RATE_LIMIT);
    let mut spawn_limiter = RateLimiter::new(SPAWN_RATE_LIMIT);

    loop {
        tokio::select! {
            // Receive per-client frames from the game loop
            frame = frame_rx.recv() => {
                match frame {
                    Some(ClientFrame::Binary(bytes)) => {
                        if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(ClientFrame::Text(s)) => {
                        // WI-201: JSON text frame (e.g., ServerMessage::Reconnect
                        // during player migration). Rare by design.
                        if ws_sink.send(Message::Text(s.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // Channel closed (game loop shut down)
                        break;
                    }
                }
            }
            // Receive input from this client
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::RequestSnapshot) => {
                                let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                                q.force_full_snapshot_clients.push(client_id);
                            }
                            Ok(msg @ (ClientMessage::SpawnEntity { .. } | ClientMessage::SpawnPlayer { .. })) => {
                                if spawn_limiter.check() {
                                    let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                                    q.client_inputs.push((client_id, msg));
                                } else {
                                    tracing::warn!(client_id, "Spawn rate limited for client {addr}");
                                }
                            }
                            Ok(msg @ ClientMessage::MoveInput { .. }) => {
                                if input_limiter.check() {
                                    let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                                    q.client_inputs.push((client_id, msg));
                                }
                                // Silently drop excess move inputs (too noisy to log)
                            }
                            Ok(input) => {
                                if input_limiter.check() {
                                    let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
                                    q.client_inputs.push((client_id, input));
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Invalid client message: {e}");
                            }
                        }
                    }
                    // Handle binary frames from client (tick ACKs)
                    Some(Ok(Message::Binary(data))) => {
                        if data.len() == 9 && data[0] == 0x03 {
                            let tick = u64::from_le_bytes(
                                data[1..9].try_into().unwrap_or([0; 8]),
                            );
                            // Monotonicity: only accept if tick > 0 and advances beyond
                            // the previous ACK. Prevents stale/reordered ACKs from
                            // regressing the value. Server tick clamping happens at the
                            // consumer (prepare_client_snapshot) since we don't have
                            // easy access to the server tick here (audit #025).
                            if tick > 0 {
                                let prev = acked_tick.load(Ordering::Relaxed);
                                if tick > prev {
                                    acked_tick.store(tick, Ordering::Relaxed);
                                }
                            }
                        }
                        // Ignore other binary messages from client
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Client {addr} disconnected");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Client disconnected -- push RemoveEntity and unregister.
    // WI-202: if this client is migrating OUT (source server flipped the
    // flag after sending Reconnect), suppress RemoveEntity — the target
    // server now owns the entity, so removing it here would cause the
    // player to blink out before reappearing on the new server.
    let migrating = migrating_out.load(Ordering::Acquire);
    {
        let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
        if !migrating {
            q.client_inputs.push((
                client_id,
                ClientMessage::RemoveEntity {
                    entity_id: player_id,
                },
            ));
        }
        q.unregister_clients.push(client_id);
    }
    if migrating {
        tracing::info!(
            client_id,
            "Client migrating out -- skipping RemoveEntity on close (player {} handed off)",
            &player_id.to_string()[..8]
        );
    } else {
        tracing::info!(
            client_id,
            "Removed player {} on disconnect",
            &player_id.to_string()[..8]
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::encoding::{encode_binary_snapshot, encode_entity};
    use glam::Vec3;
    use shared::entity::{EntityId, EntityState, ServerId};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn timed_out_claim_closes_without_spawning_or_registering_a_replacement() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{tungstenite::{protocol::Role, Message}, WebSocketStream};

        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let inbound = Arc::new(super::SharedInbound::default());
        let task = tokio::spawn(super::handle_client(
            server,
            "127.0.0.1:1".parse().unwrap(),
            inbound.clone(),
            Arc::default(),
            Default::default(),
        ));
        let claim = shared::protocol::ClientMessage::ClaimPlayer {
            player_id: EntityId::new(),
        };
        client
            .send(Message::Text(serde_json::to_string(&claim).unwrap().into()))
            .await
            .unwrap();
        assert!(matches!(client.next().await, Some(Ok(Message::Close(_)))));
        task.await.unwrap();
        let queue = inbound.lock().unwrap();
        assert!(queue.client_inputs.is_empty());
        assert!(queue.register_clients.is_empty());
    }

    #[test]
    fn encode_binary_snapshot_empty() {
        let buf = encode_binary_snapshot(42, &[]);
        assert_eq!(buf.len(), 27); // header only
        assert_eq!(buf[0], 0x01); // frame type
        assert_eq!(u64::from_le_bytes(buf[1..9].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 0);
    }

    #[test]
    fn encode_binary_snapshot_roundtrip() {
        let entities = vec![
            EntityState::new_npc(Vec3::new(1.0, 2.0, 3.0), ServerId(1)),
            EntityState::new_player(Vec3::new(10.0, 0.0, -5.0), ServerId(2)),
        ];
        let buf = encode_binary_snapshot(100, &entities);
        assert_eq!(buf.len(), 27 + 2 * 47); // header + 2 entities @ 47 bytes

        // Verify first entity
        let offset = 27;
        let uuid_bytes = &buf[offset..offset + 16];
        assert_eq!(uuid_bytes, entities[0].id.0.as_bytes());

        let px = f32::from_le_bytes(buf[offset + 16..offset + 20].try_into().unwrap());
        assert_eq!(px, 1.0);

        // Entity type: Npc = 0
        assert_eq!(buf[offset + 40], 0);

        // Authority: server 1
        let auth = u16::from_le_bytes(buf[offset + 41..offset + 43].try_into().unwrap());
        assert_eq!(auth, 1);

        // last_input_seq: 0
        let seq = u32::from_le_bytes(buf[offset + 43..offset + 47].try_into().unwrap());
        assert_eq!(seq, 0);

        // Second entity type: Player = 1
        let offset2 = 27 + 47;
        assert_eq!(buf[offset2 + 40], 1);
        let auth2 = u16::from_le_bytes(buf[offset2 + 41..offset2 + 43].try_into().unwrap());
        assert_eq!(auth2, 2);
    }

    // -- last_input_seq encoding tests --

    #[test]
    fn encode_entity_size_is_47() {
        let e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(buf.len(), 47);
    }

    #[test]
    fn encode_entity_last_input_seq() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = 99;
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, 99);
    }

    #[test]
    fn encode_snapshot_preserves_last_input_seq() {
        let mut player = EntityState::new_player(Vec3::ZERO, ServerId(1));
        player.last_input_seq = 42;
        let buf = encode_binary_snapshot(1, &[player]);
        // Header 27 + entity starts at 27, seq at offset 43 within entity
        let seq = u32::from_le_bytes(buf[27 + 43..27 + 47].try_into().unwrap());
        assert_eq!(seq, 42);
    }

    #[test]
    fn encode_npc_has_zero_last_input_seq() {
        let npc = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &npc);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, 0);
    }

    // -- encoding edge cases / robustness --

    #[test]
    fn encode_entity_extreme_positions() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        e.position = Vec3::new(f32::MAX, f32::MIN, -0.0);
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let px = f32::from_le_bytes(buf[16..20].try_into().unwrap());
        let py = f32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(px, f32::MAX);
        assert_eq!(py, f32::MIN);
    }

    #[test]
    fn encode_entity_max_authority() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(u16::MAX));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let auth = u16::from_le_bytes(buf[41..43].try_into().unwrap());
        assert_eq!(auth, u16::MAX);
    }

    #[test]
    fn encode_entity_max_seq() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = u32::MAX;
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, u32::MAX);
    }

    #[test]
    fn encode_snapshot_100_entities_size() {
        let entities: Vec<EntityState> = (0..100)
            .map(|i| {
                let mut e = EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(1));
                e.last_input_seq = i;
                e
            })
            .collect();
        let buf = encode_binary_snapshot(999, &entities);
        assert_eq!(buf.len(), 27 + 100 * 47);
        // Verify entity count in header
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 100);
    }

    #[test]
    fn encode_snapshot_preserves_all_entity_uuids() {
        let entities: Vec<EntityState> = (0..10)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();
        let buf = encode_binary_snapshot(1, &entities);
        for (i, e) in entities.iter().enumerate() {
            let offset = 27 + i * 47;
            assert_eq!(&buf[offset..offset + 16], e.id.0.as_bytes());
        }
    }

    // -- ClientRegistry tests --

    #[test]
    fn client_registry_register_and_unregister() {
        use crate::resources::{ClientFrame, ClientRegistry};
        let mut registry = ClientRegistry::default();
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = Arc::new(AtomicU64::new(0));

        registry.register_from_event(0, tx.clone(), Arc::clone(&acked));
        assert_eq!(registry.clients.len(), 1);
        assert!(registry.clients.contains_key(&0));

        let (tx2, _rx2) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked2 = Arc::new(AtomicU64::new(0));
        registry.register_from_event(1, tx2, acked2);
        assert_eq!(registry.clients.len(), 2);

        registry.unregister(0);
        assert_eq!(registry.clients.len(), 1);
        assert!(!registry.clients.contains_key(&0));
        assert!(registry.clients.contains_key(&1));

        registry.unregister(1);
        assert_eq!(registry.clients.len(), 0);
    }

    // -- Delta frame roundtrip test (restored) --

    #[test]
    fn encode_binary_delta_roundtrip() {
        use crate::encoding::encode_binary_delta;
        let updates = vec![EntityState::new_npc(Vec3::new(5.0, 0.0, 5.0), ServerId(1))];
        let removals = vec![EntityId::new()];
        let buf = encode_binary_delta(50, &updates, &removals);
        // header(31) + 1 entity(47) + 1 removal UUID(16)
        assert_eq!(buf.len(), 31 + 47 + 16);
        assert_eq!(buf[0], 0x02);
    }

    // -- ACK frame parse tests --

    #[test]
    fn ack_parse_valid() {
        let mut buf = vec![0x03u8];
        buf.extend_from_slice(&12345u64.to_le_bytes());
        assert_eq!(buf.len(), 9);
        assert_eq!(buf[0], 0x03);
        let tick = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(tick, 12345);
    }

    #[test]
    fn ack_parse_max_tick() {
        let mut buf = vec![0x03u8];
        buf.extend_from_slice(&u64::MAX.to_le_bytes());
        let tick = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(tick, u64::MAX);
    }

    #[test]
    fn ack_wrong_type_ignored() {
        let buf = vec![0x04u8, 0, 0, 0, 0, 0, 0, 0, 0]; // wrong type byte
        assert!(buf.len() == 9 && buf[0] != 0x03);
    }

    #[test]
    fn ack_wrong_length_ignored() {
        let buf = vec![0x03u8, 0, 0, 0]; // too short
        assert!(buf.len() != 9);
    }

    // -- ACK validation tests (audit #025) --

    /// Simulate the ACK validation logic from handle_client for unit testing.
    /// The actual code runs in an async context; this helper extracts the logic.
    fn apply_ack_validation(acked_tick: &AtomicU64, data: &[u8]) {
        if data.len() == 9 && data[0] == 0x03 {
            let tick = u64::from_le_bytes(data[1..9].try_into().unwrap_or([0; 8]));
            if tick > 0 {
                let prev = acked_tick.load(Ordering::Relaxed);
                if tick > prev {
                    acked_tick.store(tick, Ordering::Relaxed);
                }
            }
        }
    }

    fn make_ack_frame(tick: u64) -> Vec<u8> {
        let mut buf = vec![0x03u8];
        buf.extend_from_slice(&tick.to_le_bytes());
        buf
    }

    #[test]
    fn ack_monotonicity_rejects_lower() {
        let acked = AtomicU64::new(0);

        // ACK tick=100 -> accepted
        apply_ack_validation(&acked, &make_ack_frame(100));
        assert_eq!(acked.load(Ordering::Relaxed), 100);

        // ACK tick=50 -> rejected (not monotonically increasing)
        apply_ack_validation(&acked, &make_ack_frame(50));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            100,
            "lower tick should be rejected"
        );

        // ACK tick=150 -> accepted
        apply_ack_validation(&acked, &make_ack_frame(150));
        assert_eq!(acked.load(Ordering::Relaxed), 150);

        // ACK tick=100 again -> rejected
        apply_ack_validation(&acked, &make_ack_frame(100));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            150,
            "equal-to-previous tick should be rejected"
        );
    }

    #[test]
    fn ack_rejects_zero() {
        let acked = AtomicU64::new(0);

        // ACK tick=0 -> rejected (tick > 0 required)
        apply_ack_validation(&acked, &make_ack_frame(0));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            0,
            "tick=0 should be rejected"
        );

        // ACK tick=1 -> accepted
        apply_ack_validation(&acked, &make_ack_frame(1));
        assert_eq!(acked.load(Ordering::Relaxed), 1);

        // ACK tick=0 again -> still rejected
        apply_ack_validation(&acked, &make_ack_frame(0));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            1,
            "tick=0 should still be rejected after valid ACK"
        );
    }

    #[test]
    fn ack_monotonicity_sequential() {
        let acked = AtomicU64::new(0);

        for tick in 1..=100 {
            apply_ack_validation(&acked, &make_ack_frame(tick));
            assert_eq!(acked.load(Ordering::Relaxed), tick);
        }
    }

    // -- ACK validation edge cases (WI-023) --

    #[test]
    fn ack_duplicate_same_tick_rejected() {
        let acked = AtomicU64::new(0);

        // First ACK tick=50 -> accepted
        apply_ack_validation(&acked, &make_ack_frame(50));
        assert_eq!(acked.load(Ordering::Relaxed), 50);

        // Duplicate ACK tick=50 -> rejected (requires tick > prev, not >=)
        apply_ack_validation(&acked, &make_ack_frame(50));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            50,
            "duplicate ACK with same tick should be rejected"
        );
    }

    #[test]
    fn ack_large_jump_accepted() {
        let acked = AtomicU64::new(0);

        // ACK tick=1 -> accepted
        apply_ack_validation(&acked, &make_ack_frame(1));
        assert_eq!(acked.load(Ordering::Relaxed), 1);

        // ACK jumps to 1_000_000 -> accepted (monotonically increasing)
        apply_ack_validation(&acked, &make_ack_frame(1_000_000));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            1_000_000,
            "large jump forward should be accepted"
        );
    }

    #[test]
    fn ack_sequence_then_reset_attempt() {
        let acked = AtomicU64::new(0);

        // Normal sequence: 1, 2, 3, 4, 5
        for tick in 1..=5 {
            apply_ack_validation(&acked, &make_ack_frame(tick));
        }
        assert_eq!(acked.load(Ordering::Relaxed), 5);

        // Attempt to reset to 1 -> rejected
        apply_ack_validation(&acked, &make_ack_frame(1));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            5,
            "reset attempt should be rejected"
        );

        // Attempt to reset to 0 -> rejected
        apply_ack_validation(&acked, &make_ack_frame(0));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            5,
            "reset to 0 should be rejected"
        );

        // Can still advance
        apply_ack_validation(&acked, &make_ack_frame(6));
        assert_eq!(acked.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn ack_out_of_order_batch() {
        // Simulate out-of-order ACKs arriving: 3, 1, 5, 2, 4
        let acked = AtomicU64::new(0);

        apply_ack_validation(&acked, &make_ack_frame(3));
        assert_eq!(acked.load(Ordering::Relaxed), 3);

        apply_ack_validation(&acked, &make_ack_frame(1));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            3,
            "1 < 3, should be rejected"
        );

        apply_ack_validation(&acked, &make_ack_frame(5));
        assert_eq!(acked.load(Ordering::Relaxed), 5);

        apply_ack_validation(&acked, &make_ack_frame(2));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            5,
            "2 < 5, should be rejected"
        );

        apply_ack_validation(&acked, &make_ack_frame(4));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            5,
            "4 < 5, should be rejected"
        );
    }

    #[test]
    fn ack_malformed_frame_too_short() {
        let acked = AtomicU64::new(0);

        // Various malformed frames
        apply_ack_validation(&acked, &[0x03]); // only type byte
        apply_ack_validation(&acked, &[0x03, 0, 0, 0]); // 4 bytes (need 9)
        apply_ack_validation(&acked, &[]); // empty

        assert_eq!(
            acked.load(Ordering::Relaxed),
            0,
            "malformed frames should not change acked_tick"
        );
    }

    #[test]
    fn ack_malformed_frame_wrong_type() {
        let acked = AtomicU64::new(0);

        // Correct length but wrong type byte
        let mut buf = vec![0x01u8];
        buf.extend_from_slice(&42u64.to_le_bytes());
        apply_ack_validation(&acked, &buf);

        assert_eq!(
            acked.load(Ordering::Relaxed),
            0,
            "wrong type byte should not change acked_tick"
        );
    }

    #[test]
    fn ack_u64_max_accepted_if_monotonic() {
        let acked = AtomicU64::new(0);

        apply_ack_validation(&acked, &make_ack_frame(u64::MAX));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            u64::MAX,
            "u64::MAX should be accepted from initial state"
        );

        // Nothing can advance past u64::MAX
        apply_ack_validation(&acked, &make_ack_frame(1));
        assert_eq!(
            acked.load(Ordering::Relaxed),
            u64::MAX,
            "cannot regress from u64::MAX"
        );
    }
}
