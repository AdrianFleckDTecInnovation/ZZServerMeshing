use crate::components::*;
use crate::conversion::to_entity_state;
use crate::delta::{build_baseline, select_delta, update_baseline, FULL_SNAPSHOT_INTERVAL};
use crate::encoding::{encode_binary_delta, encode_binary_snapshot};
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityId, EntityState, ServerId};
use shared::spatial::in_aoi;

pub fn prepare_client_snapshot(
    tick: Res<TickCounter>,
    mut registry: ResMut<ClientRegistry>,
    auth_query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityTypeComp,
            &AuthorityServer,
            Option<&LastInputSeq>,
        ),
        (With<Authoritative>, Without<TransferOutbound>),
    >,
    shadow_query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityTypeComp,
            &AuthorityServer,
            Option<&LastInputSeq>,
        ),
        With<Shadow>,
    >,
    transfer_query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityTypeComp,
            &TransferOutbound,
            Option<&LastInputSeq>,
        ),
        With<Authoritative>,
    >,
) {
    if registry.clients.is_empty() {
        registry.snapshot_time = std::time::Duration::ZERO;
        return;
    }
    let start = std::time::Instant::now();
    let mut entities = Vec::with_capacity(
        auth_query.iter().count() + shadow_query.iter().count() + transfer_query.iter().count(),
    );

    // Authoritative entities first (take precedence)
    let mut origins = std::collections::HashMap::<EntityId, Vec3>::with_capacity(entities.capacity());
    let mut remember = |id, position| {
        if let std::collections::hash_map::Entry::Vacant(entry) = origins.entry(id) {
            entry.insert(position);
            true
        } else {
            false
        }
    };
    for (nid, pos, vel, kind, auth, seq) in auth_query.iter() {
        if !remember(nid.0, pos.0) {
            continue;
        }
        entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
    }

    // TransferOutbound entities: include in snapshot with target server's authority.
    // This prevents a 1-3 tick visibility gap where the entity vanishes from the
    // source's client view before the target starts broadcasting it as a shadow.
    // Clients see the entity continuously -- it just changes authority color.
    for (nid, pos, vel, kind, outbound, seq) in transfer_query.iter() {
        if !remember(nid.0, pos.0) {
            continue;
        }
        let state = to_entity_state(
            nid,
            pos,
            vel,
            kind,
            &AuthorityServer(ServerId(outbound.target_server)),
            seq,
        );
        entities.push(state);
    }

    // Shadow entities (skip if already seen as authoritative or transfer-outbound)
    for (nid, pos, vel, kind, auth, seq) in shadow_query.iter() {
        if remember(nid.0, pos.0) {
            entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
        }
    }

    // Send per-client frames (delta or full).
    // NOTE: The pre-loop force_full probe (audit #049) was removed because with
    // per-client AoI filtering, each client has a different entity set and baseline.
    // A global probe comparing full entities against an AoI-filtered baseline would
    // always fire (entities outside AoI counted as "new"). The per-client change_ratio
    // check in the delta path below handles the fallback correctly.
    // IMPORTANT: Baseline is updated ONLY after try_send succeeds (fix for audit #018).
    // If the channel is full and the frame is dropped, the baseline is NOT advanced,
    // so the next tick re-diffs against the last state the client actually received.
    // On drop, we also force the next frame to be a full snapshot (clear baseline).
    for (_client_id, handle) in registry.clients.iter_mut() {
        // AoI filter: determine which entities this client should see.
        // If the client has a player entity, only include entities within AOI_RADIUS.
        // The player's own entity is always included regardless of distance.
        // If the client has no player or the player position is unknown, send everything.
        let client_entities: Vec<EntityState> = if let Some(pid) = handle.player_id {
            if let Some(&player_pos) = origins.get(&pid) {
                entities
                    .iter()
                    .filter(|e| e.id == pid || in_aoi(player_pos, e.position))
                    .cloned()
                    .collect()
            } else {
                // Player position not found (entity not spawned yet) -- send everything
                entities.clone()
            }
        } else {
            // No player assigned yet -- send everything
            entities.clone()
        };

        // Stale-ACK detection: if the client has not ACK'd recent frames for 2 seconds,
        // force a full snapshot to resync (audit #021).
        // Clamp to current server tick to prevent malicious clients from sending
        // future ticks and bypassing stale-ACK detection (audit #025).
        let acked = handle
            .acked_tick
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(tick.0);
        let stale_ack = handle.last_sent_tick > 0
            && handle.baseline.is_some()
            && handle.last_sent_tick.saturating_sub(acked) > ACK_STALE_TICKS;

        let send_full = handle.baseline.is_none()
            || handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL
            || stale_ack;

        let delta = if send_full {
            None
        } else {
            // Send a full snapshot when more than four fifths of the visible IDs changed.
            let limit = client_entities.len() - client_entities.len().div_ceil(5);
            select_delta(&client_entities, handle.baseline.as_ref().unwrap(), limit)
        };
        if let Some(delta) = delta {
            let frame = encode_binary_delta(tick.0, &delta.updates, &delta.removals);

            match handle.tx.try_send(ClientFrame::Binary(frame)) {
                Ok(()) => {
                    // Success: advance baseline to match the delta we just sent
                    update_baseline(handle.baseline.as_mut().unwrap(), &delta);
                    handle.ticks_since_full += 1;
                    handle.last_sent_tick = tick.0;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Frame dropped: baseline NOT advanced. Clear it entirely to
                    // force a full snapshot on the next tick, guaranteeing convergence.
                    tracing::warn!(client_id = _client_id, "Client frame channel full, dropping delta -- forcing full snapshot next tick");
                    handle.baseline = None;
                    handle.ticks_since_full = 0;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Client disconnected; will be cleaned up by unregister event
                }
            }
        } else {
            if stale_ack {
                tracing::info!(
                    client_id = _client_id,
                    acked_tick = acked,
                    sent_tick = handle.last_sent_tick,
                    "ACK stale, forcing full snapshot for client"
                );
            }
            // Full snapshot path: per-client AoI-filtered encoding.
            let frame = encode_binary_snapshot(tick.0, &client_entities);
            match handle.tx.try_send(ClientFrame::Binary(frame)) {
                Ok(()) => {
                    // Success: update baseline to match what we just sent
                    handle.baseline = Some(build_baseline(&client_entities));
                    handle.ticks_since_full = 1;
                    handle.last_sent_tick = tick.0;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Frame dropped: clear baseline so next tick forces a full snapshot
                    tracing::warn!(
                        client_id = _client_id,
                        "Client frame channel full, dropping full snapshot"
                    );
                    handle.baseline = None;
                    // ticks_since_full not incremented -- will try full again next tick
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Client disconnected; will be cleaned up by unregister event
                }
            }
        }
    }
    registry.snapshot_time = start.elapsed();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use shared::entity::EntityType;
    use shared::protocol::PeerMessage;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    // Functions from sibling modules, re-exported via systems/mod.rs
    use crate::systems::{drain_client_events, drain_inbound_events};

    /// Deserialize a versioned UDP message (skip 2-byte version prefix).
    fn deserialize_versioned(bytes: &[u8]) -> PeerMessage {
        assert!(bytes.len() >= 2, "versioned message too short");
        let version = u16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(
            version,
            shared::PROTOCOL_VERSION,
            "unexpected protocol version in test"
        );
        bincode::deserialize(&bytes[2..]).unwrap()
    }

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

    fn spawn_shadow_player(world: &mut World, pos: Vec3, owner_server: u16) -> (Entity, EntityId) {
        let state = EntityState::new_player(pos, ServerId(owner_server));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, false, Some((0, 0)));
        (entity, eid)
    }

    fn register_test_client(
        world: &mut World,
        client_id: u64,
    ) -> tokio::sync::mpsc::Receiver<ClientFrame> {
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = Arc::new(AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(client_id, tx, acked);
        rx
    }

    fn register_test_client_with_ack(
        world: &mut World,
        client_id: u64,
    ) -> (tokio::sync::mpsc::Receiver<ClientFrame>, Arc<AtomicU64>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = Arc::new(AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>().register_from_event(
            client_id,
            tx,
            Arc::clone(&acked),
        );
        (rx, acked)
    }

    fn entity_count_from_full_frame(frame: &[u8]) -> u32 {
        u32::from_le_bytes(frame[9..13].try_into().unwrap())
    }

    fn register_test_client_with_player(
        world: &mut World,
        client_id: u64,
        player_eid: EntityId,
    ) -> tokio::sync::mpsc::Receiver<ClientFrame> {
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = Arc::new(AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(client_id, tx, acked);
        if let Some(handle) = world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&client_id)
        {
            handle.player_id = Some(player_eid);
        }
        rx
    }

    fn delta_counts_from_frame(frame: &[u8]) -> (u32, u32) {
        let update_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        let removal_count = u32::from_le_bytes(frame[13..17].try_into().unwrap());
        (update_count, removal_count)
    }

    #[test]
    fn spawn_shadow_player_creates_player_shadow() {
        let mut world = setup_world(1);
        let (entity, _eid) = spawn_shadow_player(&mut world, Vec3::new(100.0, 0.0, 200.0), 2);
        // Should be a shadow (not authoritative)
        assert!(world.get::<Shadow>(entity).is_some());
        assert!(world.get::<Authoritative>(entity).is_none());
        // Should be Player type
        assert_eq!(
            world.get::<EntityTypeComp>(entity).unwrap().0,
            EntityType::Player
        );
        // Should be owned by server 2
        assert_eq!(world.get::<AuthorityServer>(entity).unwrap().0, ServerId(2));
    }

    #[test]
    fn client_snapshot_merges_auth_and_shadow() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some((1, 1)));

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        // Header 27 + 3 entities * 47
        assert_eq!(frame.len(), 27 + 3 * 47);
    }

    #[test]
    fn client_snapshot_binary_format() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(1, tx, acked);

        world.resource_mut::<TickCounter>().0 = 42;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x01); // full snapshot type
        let tick = u64::from_le_bytes(frame[1..9].try_into().unwrap());
        assert_eq!(tick, 42);
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 1);
    }

    #[test]
    fn per_client_first_frame_is_full_snapshot() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        assert_eq!(
            frame[0], 0x01,
            "first frame to a new client must be a full snapshot"
        );
    }

    #[test]
    fn per_client_second_frame_is_delta() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::new(1.0, 0.0, 1.0), 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (baseline established)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let frame1 = rx.try_recv().unwrap();
        assert_eq!(frame1[0], 0x01, "first frame should be full snapshot");

        // Tick 2: no changes -> delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame2 = rx.try_recv().unwrap();
        assert_eq!(frame2[0], 0x02, "second frame should be delta");
    }

    #[test]
    fn per_client_delta_empty_when_no_changes() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::new(5.0, 0.0, 5.0), 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap(); // consume

        // Tick 2: no changes -> delta with 0 updates, 0 removals
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x02);
        let update_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        let removal_count = u32::from_le_bytes(frame[13..17].try_into().unwrap());
        assert_eq!(update_count, 0, "no entities changed");
        assert_eq!(removal_count, 0, "no entities removed");
    }

    #[test]
    fn per_client_delta_detects_entity_movement() {
        let mut world = setup_world(1);
        let (entity, _eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // Need multiple entities so moving one stays below the 80% delta fallback threshold
        spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 10.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(20.0, 0.0, 20.0), 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move only the first entity (1/3 = 33% < 80%, stays in delta path)
        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(50.0, 0.0, 50.0);

        // Tick 2: delta should include the moved entity
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame[0], 0x02);
        let update_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(update_count, 1, "moved entity should be in delta updates");
    }

    #[test]
    fn per_client_periodic_full_snapshot_at_interval() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // Use a large enough channel to hold all 101 frames without drops
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(128);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>().register_from_event(
            100,
            tx,
            std::sync::Arc::clone(&acked),
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Run 101 ticks: tick 1 = full, ticks 2-100 = delta, tick 101 = full again.
        // ACK each tick to prevent stale-ACK detection from interfering.
        for tick in 1u64..=101 {
            acked.store(tick.saturating_sub(1), std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        // Read all 101 frames and check types
        let mut full_count = 0u32;
        let mut _delta_count = 0u32;
        let mut last_frame_type = 0u8;
        for _ in 0..101 {
            let frame = rx.try_recv().unwrap();
            if frame[0] == 0x01 {
                full_count += 1;
            } else {
                _delta_count += 1;
            }
            last_frame_type = frame[0];
        }
        // First frame (tick 1) is full, tick 101 should also be full (100 ticks since full)
        assert!(
            full_count >= 2,
            "should have at least 2 full snapshots (first + periodic): got {full_count}"
        );
        assert_eq!(
            last_frame_type, 0x01,
            "tick 101 should be a full snapshot (100 ticks after first)"
        );
    }

    #[test]
    fn per_client_isolation_two_clients_get_independent_frames() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx_a = register_test_client(&mut world, 100);
        let mut rx_b = register_test_client(&mut world, 200);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: both get full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame_a = rx_a.try_recv().unwrap();
        let frame_b = rx_b.try_recv().unwrap();
        assert_eq!(frame_a[0], 0x01, "client A first frame should be full");
        assert_eq!(frame_b[0], 0x01, "client B first frame should be full");

        // Tick 2: both should get delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);

        let frame_a2 = rx_a.try_recv().unwrap();
        let frame_b2 = rx_b.try_recv().unwrap();
        assert_eq!(frame_a2[0], 0x02, "client A second frame should be delta");
        assert_eq!(frame_b2[0], 0x02, "client B second frame should be delta");
    }

    #[test]
    fn per_client_new_client_gets_full_while_existing_gets_delta() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx_a = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: client A gets full
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let fa1 = rx_a.try_recv().unwrap();
        assert_eq!(fa1[0], 0x01);

        // Register client B mid-session
        let mut rx_b = register_test_client(&mut world, 200);

        // Tick 2: client A gets delta, client B gets full
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);

        let fa2 = rx_a.try_recv().unwrap();
        let fb1 = rx_b.try_recv().unwrap();
        assert_eq!(fa2[0], 0x02, "existing client should get delta");
        assert_eq!(fb1[0], 0x01, "new client should get full snapshot");
    }

    #[test]
    fn force_full_snapshot_resets_baseline() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.add_systems(prepare_client_snapshot.after(drain_client_events));

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01);

        // Tick 2: delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02);

        // Request forced full snapshot
        world
            .resource_mut::<InboundQueue>()
            .force_full_snapshot_clients
            .push(100);

        // Tick 3: should be full (baseline was cleared by drain_client_events)
        world.resource_mut::<TickCounter>().0 = 3;
        schedule.run(&mut world);
        let f3 = rx.try_recv().unwrap();
        assert_eq!(
            f3[0], 0x01,
            "forced full snapshot should produce 0x01 frame"
        );
    }

    #[test]
    fn per_client_entity_removal_in_delta() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1); // second entity stays
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full with 2 entities
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01);
        let count1 = u32::from_le_bytes(f1[9..13].try_into().unwrap());
        assert_eq!(count1, 2);

        // Remove first entity
        crate::conversion::despawn_and_remove(&mut world, entity, eid);

        // Tick 2: delta with 1 removal
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02);
        let removal_count = u32::from_le_bytes(f2[13..17].try_into().unwrap());
        assert_eq!(removal_count, 1, "should have 1 removal in delta");
    }

    #[test]
    fn per_client_new_entity_appears_in_delta() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full with 1 entity
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Spawn a new entity
        spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 10.0), 1);

        // Tick 2: delta should contain the new entity
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02);
        let update_count = u32::from_le_bytes(f2[9..13].try_into().unwrap());
        assert_eq!(update_count, 1, "new entity should be in delta updates");
    }

    #[test]
    fn delta_fallback_all_entities_changed() {
        let mut world = setup_world(1);
        // Spawn 10 entities
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (baseline established)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01);

        // Move ALL 10 entities (100% > 80% threshold)
        for e in &entities {
            world.get_mut::<Position>(*e).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: should get full snapshot due to fallback heuristic
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x01,
            "100% entities changed should trigger delta fallback to full snapshot"
        );
    }

    #[test]
    fn delta_fallback_below_threshold() {
        let mut world = setup_world(1);
        // Spawn 10 entities
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move 7 of 10 entities (70% < 80% threshold)
        for i in 0..7 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: should get delta (below threshold)
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x02,
            "70% entities changed should send delta, not full snapshot"
        );
    }

    #[test]
    fn delta_fallback_at_boundary() {
        let mut world = setup_world(1);
        // Spawn 10 entities
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move 8 of 10 entities (80% = threshold, NOT exceeded -- strict >)
        for i in 0..8 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: should get delta (threshold is strict >)
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x02,
            "80% entities changed (at boundary, not exceeded) should send delta"
        );
    }

    #[test]
    fn delta_fallback_just_above_boundary() {
        let mut world = setup_world(1);
        // Spawn 10 entities
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move 9 of 10 entities (90% > 80% threshold)
        for i in 0..9 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: should get full snapshot (above threshold)
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x01,
            "90% entities changed should trigger delta fallback to full snapshot"
        );
    }

    #[test]
    fn ack_stale_forces_full_snapshot() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (baseline established)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01);

        // Simulate: client never ACKs (acked_tick stays at 0).
        // Advance ticks so last_sent_tick - acked > ACK_STALE_TICKS (40).
        // Run 41 more ticks (2-42) to build up the stale gap.
        for tick in 2..=42 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap(); // consume frame
        }

        // At tick 42: last_sent_tick should be 42, acked=0, gap=42 > 40. Stale!
        // Tick 43 should force a full snapshot due to stale ACK.
        world.resource_mut::<TickCounter>().0 = 43;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap();
        assert_eq!(
            frame[0], 0x01,
            "stale ACK (gap > 40 ticks) should force full snapshot"
        );
        // Verify acked_tick is still 0
        assert_eq!(acked.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn ack_fresh_sends_delta() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Client ACKs every tick -- stays fresh
        for tick in 2..=10 {
            acked.store(tick - 1, std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let frame = rx.try_recv().unwrap();
            assert_eq!(
                frame[0], 0x02,
                "fresh ACK should produce delta, not full snapshot (tick {tick})"
            );
        }
    }

    #[test]
    fn ack_stale_new_client_no_false_positive() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, _acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: new client, baseline=None, last_sent_tick=0, acked=0
        // Should get full snapshot because baseline is None, NOT because of stale ACK
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01, "new client gets full snapshot (baseline=None)");

        // Tick 2: baseline is set, last_sent_tick=1, acked=0
        // Gap: 1-0=1, which is < 40. NOT stale. Should get delta.
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x02,
            "new client should get delta on second tick (not false-positive stale)"
        );
    }

    #[test]
    fn ack_at_boundary_not_stale() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, _acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Run up to tick 41: last_sent_tick=41, acked=0, gap=41-0=41 > 40. Stale!
        // But at tick 41: last_sent_tick=40, acked=0, gap=40-0=40. NOT > 40 (strict >).
        // Run exactly 40 ticks after baseline set (ticks 2-41).
        for tick in 2..=41 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap(); // consume
        }
        // Now last_sent_tick=41, acked=0, gap=41 > 40 => stale
        // Tick 42 should be full
        world.resource_mut::<TickCounter>().0 = 42;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap();
        assert_eq!(
            frame[0], 0x01,
            "gap of 41 > 40 should trigger stale ACK full snapshot"
        );
    }

    #[test]
    fn last_sent_tick_updated_on_send() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 50: full snapshot
        world.resource_mut::<TickCounter>().0 = 50;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        let handle = world
            .resource::<ClientRegistry>()
            .clients
            .get(&100)
            .unwrap();
        assert_eq!(
            handle.last_sent_tick, 50,
            "last_sent_tick should be 50 after sending at tick 50"
        );

        // Tick 51: delta
        world.resource_mut::<TickCounter>().0 = 51;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        let handle = world
            .resource::<ClientRegistry>()
            .clients
            .get(&100)
            .unwrap();
        assert_eq!(
            handle.last_sent_tick, 51,
            "last_sent_tick should be 51 after sending at tick 51"
        );
    }

    #[test]
    fn ack_stale_ticks_constant_value() {
        assert_eq!(
            ACK_STALE_TICKS, 40,
            "ACK_STALE_TICKS should be 40 (2 seconds at 20Hz)"
        );
    }

    #[test]
    fn ack_clamp_to_server_tick() {
        // A malicious client sends ACK with u64::MAX. The server clamps it to the
        // current tick when reading it, so stale-ACK detection still works.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Malicious client sets acked_tick to u64::MAX
        acked.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);

        // Tick 2: the server should clamp acked to tick.0 (2), not use u64::MAX
        // If not clamped, the stale-ACK check would see acked > last_sent_tick
        // and never trigger, even if the client stops processing frames.
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        // Should get a delta (acked clamped to 2, gap = 1 - 2 = 0, not stale)
        assert_eq!(
            f2[0], 0x02,
            "clamped ACK should allow normal delta flow, not cause issues"
        );

        // Now set acked to 0 and advance 42 ticks -- should trigger stale even with
        // a prior u64::MAX ACK in the atomic (stale detection uses clamped value)
        acked.store(0, std::sync::atomic::Ordering::Relaxed);
        for tick in 3..=44 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap();
        }
        world.resource_mut::<TickCounter>().0 = 45;
        schedule.run(&mut world);
        let f_stale = rx.try_recv().unwrap();
        assert_eq!(
            f_stale[0], 0x01,
            "after resetting ACK to 0 and advancing 42 ticks, stale detection should fire"
        );
    }

    #[test]
    fn ack_stale_recovery_resumes_delta() {
        // After stale ACK triggers full snapshot, client resumes ACKing
        // and should get deltas again (not stuck in full snapshot loop).
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (baseline established)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Client stops ACKing for 42 ticks (stale gap > 40)
        for tick in 2..=43 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap();
        }

        // Tick 44: stale triggers full snapshot
        world.resource_mut::<TickCounter>().0 = 44;
        schedule.run(&mut world);
        let stale_frame = rx.try_recv().unwrap();
        assert_eq!(
            stale_frame[0], 0x01,
            "stale ACK should trigger full snapshot"
        );

        // Client resumes ACKing (catches up to tick 44)
        acked.store(44, std::sync::atomic::Ordering::Relaxed);

        // Tick 45: should get delta (not another full)
        world.resource_mut::<TickCounter>().0 = 45;
        schedule.run(&mut world);
        let recovery_frame = rx.try_recv().unwrap();
        assert_eq!(
            recovery_frame[0], 0x02,
            "after client resumes ACKing, should get delta (not stuck in full snapshot loop)"
        );
    }

    #[test]
    fn ack_stale_exact_boundary_40_not_stale() {
        // Gap of exactly 40 should NOT trigger stale (strict >).
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, _acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Run ticks 2 through 41 without ACKing.
        // After tick 41: last_sent_tick=41, acked=0.
        // But stale-ACK check at tick 41 uses last_sent_tick from the PREVIOUS tick (40).
        // At tick 41: we check last_sent_tick=40, gap=40-0=40. Strict > means NOT stale.
        for tick in 2..=41 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let frame = rx.try_recv().unwrap();
            assert_eq!(
                frame[0], 0x02,
                "gap <= 40 should produce delta at tick {tick}"
            );
        }
    }

    #[test]
    fn ack_stale_boundary_41_triggers() {
        // Gap of 41 should trigger stale.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx, _acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Run ticks 2-41: all delta
        for tick in 2..=41 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap();
        }

        // Tick 42: last_sent_tick=41, acked=0, gap=41 > 40. Stale!
        world.resource_mut::<TickCounter>().0 = 42;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap();
        assert_eq!(
            frame[0], 0x01,
            "gap of 41 (> 40) should trigger stale ACK full snapshot"
        );
    }

    #[test]
    fn delta_fallback_empty_entities_no_panic() {
        // When entity count is 0, change_ratio should be 0.0 (guard against division by zero).
        let mut world = setup_world(1);
        // No entities spawned.
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (empty, but no baseline yet)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(
            f1[0], 0x01,
            "first frame with no entities should be full snapshot"
        );

        // Tick 2: delta path with 0 entities. change_ratio = 0.0 / 0.0 => guarded to 0.0.
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x02,
            "empty entity set should produce delta (no fallback to full)"
        );
    }

    #[test]
    fn delta_fallback_single_entity_100_percent() {
        // 1 entity changed = 100% > 80%. Should trigger fallback.
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move the single entity (100% > 80%)
        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(50.0, 0.0, 50.0);

        // Tick 2: should get full snapshot via fallback
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x01,
            "single entity changed (100%) should trigger delta fallback to full snapshot"
        );
    }

    #[test]
    fn delta_fallback_resets_baseline_and_ticks_since_full() {
        // After a fallback full snapshot, the baseline should be rebuilt and
        // ticks_since_full should be reset. Next tick should be delta.
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move the entity (100% > 80%, triggers fallback)
        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(50.0, 0.0, 50.0);

        // Tick 2: fallback full snapshot
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x01, "should be fallback full snapshot");

        // Verify baseline was rebuilt: ticks_since_full should be 1
        let handle = world
            .resource::<ClientRegistry>()
            .clients
            .get(&100)
            .unwrap();
        assert!(
            handle.baseline.is_some(),
            "baseline should be set after fallback full"
        );
        assert_eq!(
            handle.ticks_since_full, 1,
            "ticks_since_full should be 1 after fallback full"
        );

        // Tick 3: no changes -> delta (not another full)
        world.resource_mut::<TickCounter>().0 = 3;
        schedule.run(&mut world);
        let f3 = rx.try_recv().unwrap();
        assert_eq!(
            f3[0], 0x02,
            "after fallback full, next unchanged tick should produce delta"
        );
    }

    #[test]
    fn delta_fallback_with_precise_fraction() {
        // Use a number of entities where we can precisely control the fraction.
        // 5 entities, 5 changed = 100%. 4 changed = 80% (exactly at boundary, not exceeded).
        let mut world = setup_world(1);
        let mut entities = Vec::new();
        for i in 0..5 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 20.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap();

        // Move 4/5 = 80% (at boundary, strict > means delta)
        for i in 0..4 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02, "4/5 = 80% (not exceeded) should produce delta");

        // Now move all 5 again (rebuild baseline first by consuming tick 2)
        // Need to rebuild baseline -- trick: wait for tick 3 with 5/5 changed
        for i in 0..5 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }
        world.resource_mut::<TickCounter>().0 = 3;
        schedule.run(&mut world);
        let f3 = rx.try_recv().unwrap();
        assert_eq!(
            f3[0], 0x01,
            "5/5 = 100% (> 80%) should trigger fallback to full"
        );
    }

    #[test]
    fn integration_delta_fallback_and_ack_lifecycle() {
        // Simulate a full lifecycle:
        // 1. Client connects, gets full snapshot
        // 2. Normal deltas while ACKing
        // 3. Client stops ACKing -> stale -> full snapshot
        // 4. Client resumes ACKing -> deltas again
        // 5. Mass movement -> fallback full snapshot
        // 6. Normal deltas resume
        let mut world = setup_world(1);
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 20.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Phase 1: First frame = full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f = rx.try_recv().unwrap();
        assert_eq!(f[0], 0x01, "phase 1: first frame should be full");

        // Phase 2: Normal deltas while ACKing (ticks 2-10)
        for tick in 2..=10 {
            acked.store(tick - 1, std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let f = rx.try_recv().unwrap();
            assert_eq!(
                f[0], 0x02,
                "phase 2: normal ACK should produce delta at tick {tick}"
            );
        }

        // Phase 3: Client stops ACKing. ACK stays at tick 10.
        // Run 42 more ticks (11-52) to build up stale gap: 52 - 10 = 42 > 40.
        for tick in 11..=52 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx.try_recv().unwrap(); // consume
        }
        // Tick 53: stale gap triggers full
        world.resource_mut::<TickCounter>().0 = 53;
        schedule.run(&mut world);
        let f_stale = rx.try_recv().unwrap();
        assert_eq!(
            f_stale[0], 0x01,
            "phase 3: stale ACK should trigger full snapshot"
        );

        // Phase 4: Client resumes ACKing -> deltas
        acked.store(53, std::sync::atomic::Ordering::Relaxed);
        world.resource_mut::<TickCounter>().0 = 54;
        schedule.run(&mut world);
        let f_resume = rx.try_recv().unwrap();
        assert_eq!(
            f_resume[0], 0x02,
            "phase 4: resumed ACK should produce delta"
        );

        // Phase 5: Mass movement -> fallback
        acked.store(54, std::sync::atomic::Ordering::Relaxed);
        for e in &entities {
            world.get_mut::<Position>(*e).unwrap().0 += Vec3::new(100.0, 0.0, 100.0);
        }
        world.resource_mut::<TickCounter>().0 = 55;
        schedule.run(&mut world);
        let f_fallback = rx.try_recv().unwrap();
        assert_eq!(
            f_fallback[0], 0x01,
            "phase 5: 100% entities changed should trigger fallback full snapshot"
        );

        // Phase 6: Normal deltas resume
        acked.store(55, std::sync::atomic::Ordering::Relaxed);
        world.resource_mut::<TickCounter>().0 = 56;
        schedule.run(&mut world);
        let f_normal = rx.try_recv().unwrap();
        assert_eq!(
            f_normal[0], 0x02,
            "phase 6: normal operation should produce delta"
        );
    }

    #[test]
    fn integration_two_clients_independent_stale_detection() {
        // Two clients: one ACKs, one does not. Only the stale client gets full snapshot.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let (mut rx_a, acked_a) = register_test_client_with_ack(&mut world, 100);
        let (mut rx_b, _acked_b) = register_test_client_with_ack(&mut world, 200);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: both get full
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx_a.try_recv().unwrap();
        rx_b.try_recv().unwrap();

        // Run 42 ticks: client A ACKs, client B does not
        for tick in 2..=43 {
            acked_a.store(tick - 1, std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            rx_a.try_recv().unwrap();
            rx_b.try_recv().unwrap();
        }

        // Tick 44: client A should get delta, client B should get full (stale)
        acked_a.store(43, std::sync::atomic::Ordering::Relaxed);
        world.resource_mut::<TickCounter>().0 = 44;
        schedule.run(&mut world);

        let fa = rx_a.try_recv().unwrap();
        let fb = rx_b.try_recv().unwrap();
        assert_eq!(fa[0], 0x02, "client A (ACKing) should get delta");
        assert_eq!(
            fb[0], 0x01,
            "client B (not ACKing) should get full snapshot due to stale"
        );
    }

    #[test]
    fn per_client_ticks_since_full_resets_on_full() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // Use a large enough channel to hold all frames without drops
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(128);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>().register_from_event(
            100,
            tx,
            std::sync::Arc::clone(&acked),
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Run to just before periodic full snapshot.
        // ACK each tick to prevent stale-ACK detection from interfering.
        for tick in 1u64..=100 {
            acked.store(tick.saturating_sub(1), std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }
        // Consume 100 frames
        for _ in 0..100 {
            rx.try_recv().unwrap();
        }

        // Tick 101: should be full (periodic reset)
        acked.store(100, std::sync::atomic::Ordering::Relaxed);
        world.resource_mut::<TickCounter>().0 = 101;
        schedule.run(&mut world);
        let f = rx.try_recv().unwrap();
        assert_eq!(f[0], 0x01, "tick 101 should be periodic full snapshot");

        // Tick 102: should be delta again (counter reset)
        acked.store(101, std::sync::atomic::Ordering::Relaxed);
        world.resource_mut::<TickCounter>().0 = 102;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(
            f2[0], 0x02,
            "tick after periodic full should be delta again"
        );
    }

    #[test]
    fn per_client_client_disconnect_no_panic() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Drop the receiver, simulating a disconnected client
        drop(rx);

        // Should not panic -- try_send returns Closed error and is handled
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
    }

    #[test]
    fn per_client_channel_full_no_panic() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Create a tiny channel that will fill up
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(1);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event(100, tx, acked);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Fill the channel
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        // Second tick should not panic even though channel is full
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
    }

    #[test]
    fn client_snapshot_includes_transfer_outbound_with_target_authority() {
        let mut world = setup_world(1);

        // Spawn 2 normal auth entities
        spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 10.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(20.0, 0.0, 20.0), 1);

        // Spawn 1 entity mid-transfer to server 2
        let (entity, _eid) = spawn_auth_npc(&mut world, Vec3::new(30.0, 0.0, 30.0), 1);
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 10,
            retry_count: 0,
        });

        let mut rx = register_test_client(&mut world, 1);

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();

        // Header 27 bytes + 3 entities * 47 bytes = 168
        assert_eq!(
            frame.len(),
            27 + 3 * 47,
            "transfer outbound entity should be included in snapshot"
        );

        let entity_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(entity_count, 3);

        // Verify the transfer entity has the target server (2) as authority,
        // not the source server (1). Parse all entities from the binary frame.
        let header_size = 27;
        let entity_size = 47;
        let mut found_target_auth = false;
        for i in 0..entity_count as usize {
            let offset = header_size + i * entity_size;
            // authority_server is at offset 41 (after uuid:16 + pos:12 + vel:12 + type:1 = 41)
            let auth = u16::from_le_bytes(frame[offset + 41..offset + 43].try_into().unwrap());
            if auth == 2 {
                found_target_auth = true;
            }
        }
        assert!(
            found_target_auth,
            "transfer outbound entity should report target server as authority"
        );
    }

    #[test]
    fn client_snapshot_deduplicates_auth_and_shadow_same_entity() {
        let mut world = setup_world(1);

        // Create an entity that exists as both auth and shadow (shouldn't happen
        // in normal operation, but tests the deduplication safety)
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let auth_entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Manually spawn a shadow with same NetworkId (bypassing lookup, which overwrites)
        let shadow_entity = world
            .spawn((
                NetworkId(eid),
                Position(Vec3::new(99.0, 0.0, 99.0)),
                Velocity(Vec3::ZERO),
                EntityTypeComp(EntityType::Npc),
                AuthorityServer(ServerId(2)),
                Shadow {
                    last_peer_tick: 10,
                    last_local_tick: 10,
                },
            ))
            .id();

        let mut rx = register_test_client(&mut world, 1);

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let entity_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        // Should only include the entity once (auth takes precedence)
        assert_eq!(entity_count, 1);

        // Cleanup
        world.despawn(auth_entity);
        world.despawn(shadow_entity);
    }

    #[test]
    fn transfer_ack_message_deserializes_correctly() {
        let mut world = setup_world(2);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(1, "127.0.0.1:9101".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 42;

        // Spawn entity and push a transfer
        let state = EntityState::new_npc(Vec3::new(10.0, 0.0, 20.0), ServerId(2));
        let eid = state.id;

        let transfer = InboundTransfer {
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        };
        world
            .resource_mut::<InboundQueue>()
            .authority_transfers
            .push(transfer);

        drain_inbound_events(&mut world);

        // Verify the outbound ACK message deserializes to TransferAck
        let transfers = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(transfers.len(), 1);

        let (addr, bytes) = &transfers[0];
        assert_eq!(addr.to_string(), "127.0.0.1:9101");

        let decoded: PeerMessage = deserialize_versioned(bytes);
        match decoded {
            PeerMessage::TransferAck {
                entity_id, tick, ..
            } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 42);
            }
            _ => panic!("expected TransferAck, got {:?}", decoded),
        }
    }

    #[test]
    fn force_full_skips_delta_for_all_clients() {
        // With 10 entities and 3 clients, moving all entities should trigger
        // the pre-loop probe and send full snapshots to ALL clients without
        // calling compute_delta for any of them.
        let mut world = setup_world(1);
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx1 = register_test_client(&mut world, 100);
        let mut rx2 = register_test_client(&mut world, 200);
        let mut rx3 = register_test_client(&mut world, 300);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshots for all (baseline established)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        assert_eq!(rx1.try_recv().unwrap()[0], 0x01);
        assert_eq!(rx2.try_recv().unwrap()[0], 0x01);
        assert_eq!(rx3.try_recv().unwrap()[0], 0x01);

        // Move ALL 10 entities (100% > 80%)
        for e in &entities {
            world.get_mut::<Position>(*e).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: all 3 clients should get full snapshots via force_full
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        assert_eq!(
            rx1.try_recv().unwrap()[0],
            0x01,
            "client 1 should get full snapshot via force_full"
        );
        assert_eq!(
            rx2.try_recv().unwrap()[0],
            0x01,
            "client 2 should get full snapshot via force_full"
        );
        assert_eq!(
            rx3.try_recv().unwrap()[0],
            0x01,
            "client 3 should get full snapshot via force_full"
        );
    }

    #[test]
    fn force_full_not_triggered_when_below_threshold() {
        // With 10 entities and 2 clients, moving 7 (70% < 80%) should NOT
        // trigger force_full -- clients should get deltas.
        let mut world = setup_world(1);
        let mut entities = Vec::new();
        for i in 0..10 {
            let (e, _) = spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
            entities.push(e);
        }
        let mut rx1 = register_test_client(&mut world, 100);
        let mut rx2 = register_test_client(&mut world, 200);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshots
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx1.try_recv().unwrap();
        rx2.try_recv().unwrap();

        // Move 7 of 10 (70%)
        for i in 0..7 {
            world.get_mut::<Position>(entities[i]).unwrap().0 += Vec3::new(50.0, 0.0, 50.0);
        }

        // Tick 2: both clients should get deltas
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        assert_eq!(
            rx1.try_recv().unwrap()[0],
            0x02,
            "client 1 should get delta when below threshold"
        );
        assert_eq!(
            rx2.try_recv().unwrap()[0],
            0x02,
            "client 2 should get delta when below threshold"
        );
    }

    #[test]
    fn force_full_no_clients_with_baseline_skips_probe() {
        // When no clients have baselines (all new), force_full should be false.
        // All clients get full snapshots via the baseline.is_none() path, not force_full.
        let mut world = setup_world(1);
        for i in 0..5 {
            spawn_auth_npc(&mut world, Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1);
        }
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: no baseline exists, so force_full probe returns false (unwrap_or(false))
        // Client still gets full snapshot via baseline.is_none()
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        assert_eq!(
            rx.try_recv().unwrap()[0],
            0x01,
            "new client with no baseline should get full snapshot"
        );
    }

    #[test]
    fn snapshot_filters_by_aoi() {
        // 3 entities: player at origin, NPC at (100,0,0), NPC at (3000,0,0).
        // Client with player at origin should only see 2 entities.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1);

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(count, 2, "AoI should filter out the entity at 3000 units");
    }

    #[test]
    fn snapshot_always_includes_player() {
        // Player at origin, NPC at 3000 units away. Player's own entity
        // must always be included regardless of AoI distance.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1);

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(
            count, 1,
            "only player should be in frame (NPC is outside AoI)"
        );
    }

    #[test]
    fn snapshot_no_player_sends_all() {
        // Client with no player_id set should receive all entities.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(4000.0, 0.0, 4000.0), 1);

        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(count, 3, "no player_id means all entities are sent");
    }

    #[test]
    fn snapshot_all_within_aoi_sends_all() {
        // All entities within 500 units. All should appear in the snapshot.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(-200.0, 0.0, 300.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(400.0, 0.0, 0.0), 1);

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(count, 4, "all entities within AoI + player = 4 total");
    }

    #[test]
    fn aoi_entity_leaving_produces_removal() {
        // Tick 1: entity at (400,0,0) is within AoI. Full snapshot + baseline.
        // Then move entity to (3000,0,0) -- outside AoI.
        // Tick 2: delta should contain a removal.
        let mut world = setup_world(1);
        let (player_entity, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        let (npc_entity, _npc_id) = spawn_auth_npc(&mut world, Vec3::new(400.0, 0.0, 0.0), 1);

        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .player_id = Some(player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (2 entities: player + NPC)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01, "first frame should be full");
        assert_eq!(entity_count_from_full_frame(&f1), 2);
        acked.store(1, std::sync::atomic::Ordering::Relaxed);

        // Move NPC outside AoI
        world.get_mut::<Position>(npc_entity).unwrap().0 = Vec3::new(3000.0, 0.0, 0.0);

        // Tick 2: delta should have a removal for the NPC
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02, "second frame should be delta");
        let (updates, removals) = delta_counts_from_frame(&f2);
        assert_eq!(
            removals, 1,
            "NPC moved outside AoI should produce 1 removal"
        );
        // The player may have an update (unchanged position still in baseline),
        // but the NPC should be gone.
        let _ = updates; // player update is fine
        let _ = player_entity; // keep the compiler happy
    }

    #[test]
    fn aoi_entity_entering_produces_update() {
        // Tick 1: entity at (3000,0,0) is outside AoI. Full snapshot (player only).
        // Then move entity to (100,0,0) -- inside AoI.
        // Tick 2: delta should contain an update (new entity).
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        let (npc_entity, _npc_id) = spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1);

        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .player_id = Some(player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (only player, NPC outside AoI)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(
            entity_count_from_full_frame(&f1),
            1,
            "only player should be in first frame"
        );
        acked.store(1, std::sync::atomic::Ordering::Relaxed);

        // Move NPC inside AoI
        world.get_mut::<Position>(npc_entity).unwrap().0 = Vec3::new(100.0, 0.0, 0.0);

        // Tick 2: delta should have the NPC as an update (new entity)
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x02, "second frame should be delta");
        let (updates, removals) = delta_counts_from_frame(&f2);
        assert!(
            updates >= 1,
            "NPC entering AoI should produce at least 1 update"
        );
        assert_eq!(removals, 0, "no entities should be removed");
    }

    #[test]
    fn aoi_boundary_no_flicker() {
        // Entity at exactly 500 units from player (on boundary).
        // Run 5 ticks. Entity should be consistently included (no removal+re-add).
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        // Exactly on boundary: 500 units in X direction
        spawn_auth_npc(&mut world, Vec3::new(500.0, 0.0, 0.0), 1);

        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .player_id = Some(player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot (2 entities: player + boundary NPC)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(
            entity_count_from_full_frame(&f1),
            2,
            "boundary entity should be included"
        );
        acked.store(1, std::sync::atomic::Ordering::Relaxed);

        // Ticks 2-5: deltas should have 0 removals (entity stays stable)
        for tick in 2..=5 {
            acked.store(tick - 1, std::sync::atomic::Ordering::Relaxed);
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let frame = rx.try_recv().unwrap();
            assert_eq!(frame[0], 0x02, "tick {tick} should be delta");
            let (_updates, removals) = delta_counts_from_frame(&frame);
            assert_eq!(
                removals, 0,
                "tick {tick}: boundary entity should not flicker out"
            );
        }
    }

    #[test]
    fn aoi_full_snapshot_is_filtered() {
        // Force a full snapshot (set ticks_since_full to FULL_SNAPSHOT_INTERVAL).
        // Verify the full snapshot only contains AoI-visible entities.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1); // outside AoI

        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .player_id = Some(player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: first frame is full (no baseline)
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(f1[0], 0x01);
        assert_eq!(
            entity_count_from_full_frame(&f1),
            2,
            "first full should have 2 AoI entities"
        );
        acked.store(1, std::sync::atomic::Ordering::Relaxed);

        // Force a periodic full snapshot by setting ticks_since_full = FULL_SNAPSHOT_INTERVAL
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .ticks_since_full = FULL_SNAPSHOT_INTERVAL;

        // Tick 2: forced full snapshot should still be AoI-filtered
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap();
        assert_eq!(f2[0], 0x01, "periodic refresh should be full snapshot");
        assert_eq!(
            entity_count_from_full_frame(&f2),
            2,
            "periodic full snapshot should be AoI-filtered (not all 3 entities)"
        );
    }

    #[test]
    fn aoi_filters_shadow_entities() {
        // Shadow entities should also be filtered by AoI. A shadow at 3000 units
        // should NOT appear in the client's snapshot.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1); // auth, inside AoI

        // Shadow entity inside AoI (from server 2)
        let shadow_near = EntityState::new_npc(Vec3::new(200.0, 0.0, 0.0), ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow_near, false, Some((1, 1)));

        // Shadow entity outside AoI (from server 2)
        let shadow_far = EntityState::new_npc(Vec3::new(3000.0, 0.0, 0.0), ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow_far, false, Some((1, 1)));

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        // player (0,0,0) + auth NPC (100,0,0) + shadow near (200,0,0) = 3
        // shadow far (3000,0,0) should be excluded
        assert_eq!(
            count, 3,
            "shadow entity at 3000 units should be filtered out"
        );
    }

    #[test]
    fn aoi_filters_transfer_outbound_entities() {
        // TransferOutbound entities should also be filtered by AoI.
        use crate::components::TransferOutbound;

        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Auth NPC inside AoI
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);

        // TransferOutbound NPC inside AoI
        let state_near = EntityState::new_npc(Vec3::new(200.0, 0.0, 0.0), ServerId(1));
        let e_near = spawn_from_entity_state_direct(&mut world, &state_near, true, None);
        world.entity_mut(e_near).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // TransferOutbound NPC outside AoI
        let state_far = EntityState::new_npc(Vec3::new(3000.0, 0.0, 0.0), ServerId(1));
        let e_far = spawn_from_entity_state_direct(&mut world, &state_far, true, None);
        world.entity_mut(e_far).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        // player (0,0,0) + auth NPC (100,0,0) + transfer near (200,0,0) = 3
        // transfer far (3000,0,0) should be excluded
        assert_eq!(
            count, 3,
            "transfer-outbound entity at 3000 units should be filtered out"
        );
    }

    #[test]
    fn aoi_player_not_spawned_sends_unfiltered() {
        // If player_id is set but the player entity does not exist in the ECS
        // (e.g., not yet spawned), the system should send all entities unfiltered.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(4500.0, 0.0, 4500.0), 1);

        // Create a fake player_id that does NOT exist in the ECS
        let fake_player_id = EntityId::new();
        let mut rx = register_test_client_with_player(&mut world, 100, fake_player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(
            count, 3,
            "player entity not in ECS means unfiltered: all 3 entities sent"
        );
    }

    #[test]
    fn aoi_two_clients_different_windows() {
        // Two clients with players at different positions should receive different entity sets.
        let mut world = setup_world(1);

        // Player A at origin
        let (_pe_a, player_a_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        // Player B at (4000, 0, 0)
        let (_pe_b, player_b_id) = spawn_auth_player(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1);

        // NPC near player A only (100, 0, 0)
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);
        // NPC near player B only (3900, 0, 0)
        spawn_auth_npc(&mut world, Vec3::new(3900.0, 0.0, 0.0), 1);
        // NPC far from both (2000, 0, 2000) - 2828 from A, 2236 from B
        spawn_auth_npc(&mut world, Vec3::new(2000.0, 0.0, 2000.0), 1);

        let mut rx_a = register_test_client_with_player(&mut world, 100, player_a_id);
        let mut rx_b = register_test_client_with_player(&mut world, 200, player_b_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame_a = rx_a.try_recv().unwrap();
        let frame_b = rx_b.try_recv().unwrap();
        let count_a = entity_count_from_full_frame(&frame_a);
        let count_b = entity_count_from_full_frame(&frame_b);

        // Client A should see: player A + NPC at (100,0,0) + player B is at 4000 (outside AoI)
        // Wait -- player B is an entity too. Player B at (4000,0,0) is outside A's AoI.
        // So client A sees: player A (self, always included) + NPC(100,0,0) = 2
        assert_eq!(count_a, 2, "client A: own player + nearby NPC");

        // Client B should see: player B (self) + NPC at (3900,0,0) (100 away) = 2
        // Player A at origin is 4000 units away -- outside. NPC at (2000,0,2000) is too far.
        assert_eq!(count_b, 2, "client B: own player + nearby NPC");
    }

    #[test]
    fn aoi_stale_ack_full_snapshot_is_filtered() {
        // When stale ACK triggers a full snapshot, it should still be AoI-filtered.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1); // inside AoI
        spawn_auth_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1); // outside AoI

        let (mut rx, acked) = register_test_client_with_ack(&mut world, 100);
        world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&100)
            .unwrap()
            .player_id = Some(player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: initial full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        let f1 = rx.try_recv().unwrap();
        assert_eq!(
            entity_count_from_full_frame(&f1),
            2,
            "initial: player + nearby NPC"
        );
        acked.store(1, std::sync::atomic::Ordering::Relaxed);

        // Advance ticks without updating acked_tick to trigger stale ACK.
        // ACK_STALE_TICKS = 40, so advance to tick 43 (last_sent_tick - acked > 40).
        for tick in 2..=43 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let _ = rx.try_recv(); // drain frames
        }

        // Tick 44: stale ACK should trigger full snapshot
        world.resource_mut::<TickCounter>().0 = 44;
        schedule.run(&mut world);
        let stale_frame = rx.try_recv().unwrap();
        assert_eq!(
            stale_frame[0], 0x01,
            "stale ACK should trigger full snapshot"
        );
        assert_eq!(
            entity_count_from_full_frame(&stale_frame),
            2,
            "stale ACK full snapshot should be AoI-filtered (not 3)"
        );
    }

    #[test]
    fn aoi_player_at_world_edge() {
        // Player at the edge of the world. Entities near them should be visible.
        let mut world = setup_world(1);
        let (_pe, player_id) = spawn_auth_player(&mut world, Vec3::new(4800.0, 0.0, 4800.0), 1);
        // NPC within 500 of player
        spawn_auth_npc(&mut world, Vec3::new(4900.0, 0.0, 4700.0), 1);
        // NPC far from player (at opposite edge)
        spawn_auth_npc(&mut world, Vec3::new(-4800.0, 0.0, -4800.0), 1);

        let mut rx = register_test_client_with_player(&mut world, 100, player_id);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        // player + nearby NPC = 2 (far NPC is ~13577 units away)
        assert_eq!(
            count, 2,
            "player at world edge should see only nearby entities"
        );
    }

    #[test]
    fn aoi_zero_entities_no_panic() {
        // No entities in the world. Client with no player_id. Should produce a valid
        // empty snapshot without panicking.
        let mut world = setup_world(1);
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        let frame = rx.try_recv().unwrap();
        let count = entity_count_from_full_frame(&frame);
        assert_eq!(count, 0, "empty world should produce frame with 0 entities");
    }
}
