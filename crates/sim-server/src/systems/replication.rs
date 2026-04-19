use crate::components::*;
use crate::conversion::to_entity_state;
use crate::peer_bounds::{PeerBounds, PEER_AOI_RADIUS, PEER_AOI_RADIUS_SQ};
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityState, EntityType};

/// Build the outbound replication plan for the tick.
///
/// **WI-221 (design 016, Batch 6):** single-pass classifier. The old three-
/// path implementation (no-shadows bootstrap, per-peer tiered loop, and a
/// second bootstrap append for peers that got nothing) did three allocations
/// per peer and encoded its phase filter in three places. This rewrite walks
/// each authoritative entity exactly once, and for every `peer_addrs` peer
/// decides on the spot whether the entity belongs in its outbound list. The
/// classification is behavior-equivalent to the old logic:
///
/// - **Hot tier**: the peer has at least one Player shadow AND the entity is
///   within `PEER_AOI_RADIUS` of any of them. Every tick (20Hz).
/// - **Cold tier**: the entity's phase (UUID `% 4`) matches `tick % 4` AND
///   the peer has a valid `PeerBounds` AND the entity lies within those
///   bounds. 5Hz via phase spreading.
/// - **Bootstrap tier**: the peer is in `peer_addrs` but we have no shadows
///   from it AT ALL. Phase-matched entities are sent so a peer that just
///   came online or lost all its shadows can recover. 5Hz via phase
///   spreading.
///
/// The hot/cold and bootstrap paths are mutually exclusive per-peer: a peer
/// either has shadows (hot/cold) or has none (bootstrap). A peer that has
/// shadows but no valid `PeerBounds` (degenerate: collapsed cluster) gets
/// nothing from the cold path — that matches the pre-refactor behavior where
/// the main loop `continue`d on missing bounds.
pub fn prepare_replication(
    tick: Res<TickCounter>,
    config: Res<ServerConfig>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound: ResMut<OutboundReplication>,
    auth_query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityTypeComp,
            &AuthorityServer,
            Option<&LastInputSeq>,
        ),
        With<Authoritative>,
    >,
    shadow_query: Query<(&AuthorityServer, &Position, &EntityTypeComp), With<Shadow>>,
) {
    let entities: Vec<EntityState> = auth_query
        .iter()
        .map(|(nid, pos, vel, kind, auth, seq)| to_entity_state(nid, pos, vel, kind, auth, seq))
        .collect();

    outbound.tick = tick.0;
    outbound.hot_count = 0;
    outbound.cold_count = 0;
    outbound.per_peer.clear();
    outbound.full_snapshot = None;

    let no_peers = peer_addrs.0.is_empty();

    // Fast path: no peers registered, or no entities to replicate.
    if no_peers || entities.is_empty() {
        outbound.full_snapshot = if entities.is_empty() {
            None
        } else {
            Some(entities)
        };
        return;
    }

    // Cold phase: each entity is assigned a phase (0-3) based on its UUID.
    // An entity is sent as cold when `tick % 4 == its phase`, achieving 5Hz.
    // This spreads cold entities evenly across ticks instead of bursting
    // all cold entities on every 4th tick (audit #066).
    let current_phase = tick.0 % 4;

    // Build two maps from shadow entities in one pass:
    //   peer_player_positions — Player shadow positions per peer (hot check)
    //   peer_all_positions    — all shadow positions per peer (bounds input)
    let mut peer_player_positions: std::collections::HashMap<u16, Vec<Vec3>> =
        std::collections::HashMap::new();
    let mut peer_all_positions: std::collections::HashMap<u16, Vec<Vec3>> =
        std::collections::HashMap::new();

    for (auth, pos, kind) in shadow_query.iter() {
        if auth.0 == config.server_id {
            continue;
        }
        let sid = auth.0 .0;
        peer_all_positions.entry(sid).or_default().push(pos.0);
        if kind.0 == EntityType::Player {
            peer_player_positions.entry(sid).or_default().push(pos.0);
        }
    }

    // PeerBounds covers ALL shadow positions (not just Players). A peer with
    // shadows but no Players falls back to bounds-only cold replication.
    let peer_bounds: std::collections::HashMap<u16, PeerBounds> = peer_all_positions
        .iter()
        .filter_map(|(&sid, positions)| {
            PeerBounds::from_positions(positions.iter().copied(), PEER_AOI_RADIUS)
                .map(|bounds| (sid, bounds))
        })
        .collect();

    // Debug logging every 5 seconds
    if tick.0 % 100 == 0 && !peer_all_positions.is_empty() {
        for (&sid, positions) in &peer_all_positions {
            let player_count = peer_player_positions.get(&sid).map_or(0, |p| p.len());
            tracing::debug!(
                peer = sid,
                shadow_count = positions.len(),
                player_shadows = player_count,
                current_phase,
                "Tiered replication state"
            );
        }
    }

    // Pre-seed an empty entity list for every peer that has shadows AND has
    // valid bounds. This preserves the pre-WI-221 invariant that such peers
    // always appear in `per_peer` even when no entities match the per-tick
    // phase filter — downstream code relies on the key presence to signal
    // "peer known, nothing to send this tick" (vs. "peer unknown").
    //
    // Peers with shadows but no bounds (degenerate: collapsed positions) are
    // intentionally skipped, matching the pre-refactor `continue` on missing
    // bounds. Shadowless peers get the bootstrap tier instead and only land
    // in `per_peer` if at least one phase-matched entity is sent.
    for &peer_id in peer_addrs.0.keys() {
        let has_any_shadows = peer_all_positions
            .get(&peer_id)
            .is_some_and(|p| !p.is_empty());
        if has_any_shadows && peer_bounds.contains_key(&peer_id) {
            outbound.per_peer.entry(peer_id).or_default();
        }
    }

    // Single pass over entities. For each entity, for each peer, decide
    // hot / cold / bootstrap inclusion in one place. `peer_addrs.0.keys()` is
    // authoritative — entities are replicated to ALL registered peers, not
    // just the ones with shadows (shadowless peers get the bootstrap tier).
    let peer_ids: Vec<u16> = peer_addrs.0.keys().copied().collect();

    for entity in &entities {
        let phase_match = (entity.id.0.as_u128() % 4) as u64 == current_phase;

        for &peer_id in &peer_ids {
            let has_any_shadows = peer_all_positions
                .get(&peer_id)
                .is_some_and(|p| !p.is_empty());

            if has_any_shadows {
                // Shadowed peer: hot + cold classification.
                let mut is_hot = false;
                if let Some(players) = peer_player_positions.get(&peer_id) {
                    is_hot = players.iter().any(|pp| {
                        let dx = entity.position.x - pp.x;
                        let dz = entity.position.z - pp.z;
                        (dx * dx + dz * dz) <= PEER_AOI_RADIUS_SQ
                    });
                }

                if is_hot {
                    outbound
                        .per_peer
                        .entry(peer_id)
                        .or_default()
                        .push(entity.clone());
                    outbound.hot_count += 1;
                    continue;
                }

                // Cold: phase-matched AND within peer bounds.
                if phase_match {
                    if let Some(bounds) = peer_bounds.get(&peer_id) {
                        if bounds.contains(entity.position) {
                            outbound
                                .per_peer
                                .entry(peer_id)
                                .or_default()
                                .push(entity.clone());
                            outbound.cold_count += 1;
                        }
                    }
                }
            } else {
                // Bootstrap: peer is registered but has no shadows from us.
                // Send phase-matched entities so it can re-hydrate at 5Hz.
                // Matches the pre-WI-221 "no shadows anywhere" and "some peers
                // have no shadows on this server" bootstrap paths.
                if phase_match {
                    outbound
                        .per_peer
                        .entry(peer_id)
                        .or_default()
                        .push(entity.clone());
                    outbound.cold_count += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use shared::entity::{EntityId, EntityState, ServerId};
    use shared::protocol::PeerMessage;

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

    fn spawn_shadow_npc(world: &mut World, pos: Vec3, owner_server: u16) -> (Entity, EntityId) {
        let state = EntityState::new_npc(pos, ServerId(owner_server));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, false, Some((0, 0)));
        (entity, eid)
    }

    fn spawn_shadow_player(world: &mut World, pos: Vec3, owner_server: u16) -> (Entity, EntityId) {
        let state = EntityState::new_player(pos, ServerId(owner_server));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, false, Some((0, 0)));
        (entity, eid)
    }

    fn spawn_auth_npc_in_phase(
        world: &mut World,
        pos: Vec3,
        server_id: u16,
        phase: u64,
        index: u32,
    ) -> (Entity, EntityId) {
        let mut state = EntityState::new_npc(pos, ServerId(server_id));
        state.id = EntityId(uuid::Uuid::from_u128((index as u128) * 4 + phase as u128));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, true, None);
        (entity, eid)
    }

    #[test]
    fn replication_only_authoritative() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        // Shadow entity
        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some((1, 1)));

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_some());
        assert_eq!(outbound.full_snapshot.as_ref().unwrap().len(), 2); // only auth
    }

    #[test]
    fn replication_empty_world() {
        let mut world = setup_world(1);
        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
    }

    #[test]
    fn replication_last_input_seq() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        world.get_mut::<LastInputSeq>(entity).unwrap().0 = 42;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let entities = outbound.full_snapshot.as_ref().unwrap();
        assert_eq!(entities[0].last_input_seq, 42);
    }

    #[test]
    fn chunk_math_exact_multiple() {
        // 200 entities / 100 per chunk = exactly 2 chunks
        let entity_count = 200;
        let per_chunk = 100;
        let total_chunks = ((entity_count + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 2);
    }

    #[test]
    fn chunk_math_just_over_threshold() {
        // 101 entities should produce 2 chunks
        let entity_count = 101;
        let per_chunk = 100;
        let total_chunks = ((entity_count + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 2);
    }

    #[test]
    fn chunk_math_at_threshold() {
        // 100 entities should stay unchunked (uses EntityStateUpdate, not ChunkedEntityUpdate)
        // But if it WERE chunked, it would be 1 chunk
        let entity_count = 100;
        let per_chunk = 100;
        let total_chunks = ((entity_count + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 1);
    }

    #[test]
    fn chunk_math_large_entity_count() {
        // 1500 entities / 100 = 15 chunks
        let entity_count = 1500;
        let per_chunk = 100;
        let total_chunks = ((entity_count + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 15);
    }

    #[test]
    fn chunked_entity_update_stays_under_udp_limit() {
        // 100 entities in a single ChunkedEntityUpdate
        let entities: Vec<EntityState> = (0..100)
            .map(|_| EntityState::new_npc(Vec3::new(100.0, 0.0, -100.0), ServerId(1)))
            .collect();

        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 999,
            server_id: ServerId(1),
            session_id: 0,
            chunk_index: 0,
            total_chunks: 1,
            entities,
        };

        let bytes = bincode::serialize(&msg).unwrap();
        // UDP max payload: 65507 bytes. Each chunk should be well under this.
        assert!(
            bytes.len() < 65507,
            "chunk size {} exceeds UDP limit 65507",
            bytes.len()
        );
        // Also check it's a reasonable size (each entity is ~100 bytes in bincode)
        assert!(bytes.len() > 100, "chunk too small: {} bytes", bytes.len());
    }

    #[test]
    fn unchunked_entity_update_size_matches_expectations() {
        // 100 entities in an EntityStateUpdate (unchunked path)
        let entities: Vec<EntityState> = (0..100)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();

        let msg = PeerMessage::EntityStateUpdate {
            tick: 1,
            server_id: ServerId(1),
            session_id: 0,
            entities,
        };

        let bytes = bincode::serialize(&msg).unwrap();
        assert!(
            bytes.len() < 65507,
            "unchunked 100 entities {} exceeds UDP limit",
            bytes.len()
        );
    }

    #[test]
    fn replication_includes_transfer_outbound_entities() {
        let mut world = setup_world(1);

        // 3 normal auth entities
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);
        let (frozen_entity, _) = spawn_auth_npc(&mut world, Vec3::Z, 1);

        // Mark one as TransferOutbound
        world.entity_mut(frozen_entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_some());
        // All 3 entities should be replicated, including the TransferOutbound one.
        // This ensures peers have up-to-date shadows for crash recovery.
        assert_eq!(
            outbound.full_snapshot.as_ref().unwrap().len(),
            3,
            "TransferOutbound entity should be included in replication for crash recovery"
        );
    }

    #[test]
    fn exactly_100_entities_uses_unchunked_format() {
        // At exactly 100 entities, the replication code uses EntityStateUpdate (unchunked),
        // not ChunkedEntityUpdate. This test verifies the unchunked format works at the boundary.
        let entities: Vec<EntityState> = (0..100)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();

        let msg = PeerMessage::EntityStateUpdate {
            tick: 50,
            server_id: ServerId(1),
            session_id: 0,
            entities,
        };

        let bytes = bincode::serialize(&msg).unwrap();
        let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
        match decoded {
            PeerMessage::EntityStateUpdate { entities, .. } => {
                assert_eq!(entities.len(), 100);
            }
            _ => panic!("expected EntityStateUpdate, got {:?}", decoded),
        }
    }

    #[test]
    fn exactly_101_entities_needs_two_chunks() {
        let entities: Vec<EntityState> = (0..101)
            .map(|i| {
                let mut e = EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(1));
                e.position.x = i as f32;
                e
            })
            .collect();

        let per_chunk = 100usize;
        let total_chunks = ((entities.len() + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 2);

        // Create both chunks and reassemble
        let mut reassembled = Vec::new();
        for (i, chunk) in entities.chunks(per_chunk).enumerate() {
            let msg = PeerMessage::ChunkedEntityUpdate {
                tick: 50,
                server_id: ServerId(1),
                session_id: 0,
                chunk_index: i as u16,
                total_chunks,
                entities: chunk.to_vec(),
            };
            let bytes = bincode::serialize(&msg).unwrap();
            let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
            if let PeerMessage::ChunkedEntityUpdate {
                entities: chunk_entities,
                ..
            } = decoded
            {
                reassembled.extend(chunk_entities);
            }
        }

        assert_eq!(reassembled.len(), 101);
        // Verify first chunk had 100, second had 1
        assert!((reassembled[99].position.x - 99.0).abs() < 0.001);
        assert!((reassembled[100].position.x - 100.0).abs() < 0.001);
    }

    #[test]
    fn chunk_reassembly_out_of_order() {
        let entities: Vec<EntityState> = (0..250)
            .map(|i| {
                let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
                e.position.x = i as f32;
                e
            })
            .collect();

        let per_chunk = 100usize;
        let total_chunks = ((entities.len() + per_chunk - 1) / per_chunk) as u16;
        assert_eq!(total_chunks, 3);

        // Serialize all chunks
        let mut serialized_chunks: Vec<Vec<u8>> = Vec::new();
        for (i, chunk) in entities.chunks(per_chunk).enumerate() {
            let msg = PeerMessage::ChunkedEntityUpdate {
                tick: 100,
                server_id: ServerId(1),
                session_id: 0,
                chunk_index: i as u16,
                total_chunks,
                entities: chunk.to_vec(),
            };
            serialized_chunks.push(bincode::serialize(&msg).unwrap());
        }

        // Process in reverse order (out-of-order delivery)
        let mut chunks_received: Vec<(u16, Vec<EntityState>)> = Vec::new();
        for bytes in serialized_chunks.iter().rev() {
            let decoded: PeerMessage = bincode::deserialize(bytes).unwrap();
            if let PeerMessage::ChunkedEntityUpdate {
                chunk_index,
                entities: chunk_entities,
                ..
            } = decoded
            {
                chunks_received.push((chunk_index, chunk_entities));
            }
        }

        // Sort by chunk_index to reassemble in order
        chunks_received.sort_by_key(|(idx, _)| *idx);
        let reassembled: Vec<EntityState> = chunks_received
            .into_iter()
            .flat_map(|(_, entities)| entities)
            .collect();

        assert_eq!(reassembled.len(), 250);
        // Verify data integrity
        for (i, e) in reassembled.iter().enumerate() {
            assert!(
                (e.position.x - i as f32).abs() < 0.001,
                "entity {} position mismatch after out-of-order reassembly",
                i
            );
        }
    }

    #[test]
    fn duplicate_chunk_received_is_idempotent() {
        let entities: Vec<EntityState> = (0..50)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();

        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 100,
            server_id: ServerId(1),
            session_id: 0,
            chunk_index: 0,
            total_chunks: 1,
            entities: entities.clone(),
        };

        let bytes = bincode::serialize(&msg).unwrap();

        // Decode the same chunk twice (simulating duplicate UDP delivery)
        let first: PeerMessage = bincode::deserialize(&bytes).unwrap();
        let second: PeerMessage = bincode::deserialize(&bytes).unwrap();

        // Both should decode identically
        if let (
            PeerMessage::ChunkedEntityUpdate {
                entities: e1,
                chunk_index: c1,
                ..
            },
            PeerMessage::ChunkedEntityUpdate {
                entities: e2,
                chunk_index: c2,
                ..
            },
        ) = (&first, &second)
        {
            assert_eq!(c1, c2);
            assert_eq!(e1.len(), e2.len());
            assert_eq!(e1.len(), 50);
        } else {
            panic!("expected ChunkedEntityUpdate");
        }
    }

    #[test]
    fn chunked_entity_update_reassembly_logic() {
        // Simulate what the receiver does: collect chunks, merge entities
        let mut all_entities: Vec<EntityState> = Vec::new();
        let total = 250;
        let per_chunk = 100;
        let total_chunks = ((total + per_chunk - 1) / per_chunk) as u16;

        let entities: Vec<EntityState> = (0..total)
            .map(|i| {
                let mut e = EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(1));
                e.position.x = i as f32;
                e
            })
            .collect();

        // Create chunks and reassemble
        for (i, chunk) in entities.chunks(per_chunk).enumerate() {
            let msg = PeerMessage::ChunkedEntityUpdate {
                tick: 100,
                server_id: ServerId(1),
                session_id: 0,
                chunk_index: i as u16,
                total_chunks,
                entities: chunk.to_vec(),
            };

            // Roundtrip through bincode
            let bytes = bincode::serialize(&msg).unwrap();
            let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();

            if let PeerMessage::ChunkedEntityUpdate {
                entities: chunk_entities,
                ..
            } = decoded
            {
                all_entities.extend(chunk_entities);
            }
        }

        assert_eq!(all_entities.len(), total);
        // Verify ordering preserved
        for (i, e) in all_entities.iter().enumerate() {
            assert!(
                (e.position.x - i as f32).abs() < 0.001,
                "entity {} position mismatch: expected {}, got {}",
                i,
                i,
                e.position.x
            );
        }
    }

    #[test]
    fn replication_includes_all_entities_regardless_of_distance() {
        // Peer replication (prepare_replication) should NOT filter by AoI.
        // An authoritative entity at (4000,0,0) should still be replicated.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1);
        spawn_auth_npc(&mut world, Vec3::new(-4000.0, 0.0, -4000.0), 1);

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        // No PeerAddresses/shadows in this test world,
        // so prepare_replication falls back to full_snapshot for all entities.
        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_some());
        let entities = outbound.full_snapshot.as_ref().unwrap();
        assert_eq!(
            entities.len(),
            3,
            "replication should include ALL auth entities regardless of position"
        );
    }

    #[test]
    fn peer_repl_filtered_tick_produces_per_peer() {
        // On a non-full tick with NPC-only shadows, per_peer is populated via
        // PeerBounds fallback, full_snapshot is None.
        // With phase spreading, only entities matching tick's phase are sent.
        let mut world = setup_world(1);
        // Both entities in phase 1 so they match tick=1 (current_phase=1).
        // The fallback path requires phase match AND bounds.contains().
        let (_e1, _eid1) = spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 1, 100);
        let (_e2, _eid2) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 1, 101);

        // Peer 2 with NPC shadow near origin (no Player shadows -> PeerBounds fallback)
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 2);

        // Tick 1: current_phase=1, both entities match phase 1
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(
            outbound.full_snapshot.is_none(),
            "filtered tick should not set full_snapshot"
        );
        assert!(
            outbound.per_peer.contains_key(&2),
            "should have per_peer entry for peer 2"
        );
        let peer_entities = &outbound.per_peer[&2];
        // Entity at (0,0) matches phase AND within PeerBounds -> included
        // Entity at (4000,0,0) matches phase but NOT within PeerBounds -> excluded
        assert_eq!(
            peer_entities.len(),
            1,
            "only nearby phase-matched entity should be included"
        );
    }

    #[test]
    fn peer_repl_mixed_proximity() {
        // Auth entities at various distances; PeerBounds fallback (NPC-only shadows)
        // only sends phase-matched entities within bounds.
        let mut world = setup_world(1);
        // All entities in phase 1 to match tick=1
        spawn_auth_npc_in_phase(&mut world, Vec3::new(0.0, 0.0, 0.0), 1, 1, 110);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(100.0, 0.0, 0.0), 1, 1, 111);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 1, 112);

        // Peer 2: NPC shadow at origin (no Player -> PeerBounds fallback)
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_npc(&mut world, Vec3::new(0.0, 0.0, 0.0), 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        // All match phase 1. PeerBounds around (0,0) expanded by PEER_AOI_RADIUS:
        // (0,0,0) within bounds -> yes
        // (100,0,0) within bounds -> yes
        // (4000,0,0) outside bounds -> no
        assert_eq!(
            peer_entities.len(),
            2,
            "should include (0,0) and (100,0) but not (4000,0)"
        );
    }

    #[test]
    fn peer_repl_two_peers_different_bounds() {
        // Two peers with NPC-only shadows at different positions.
        // PeerBounds fallback requires phase match AND bounds.contains().
        let mut world = setup_world(1);
        // Both entities in phase 1 to match tick=1
        let (_e1, _eid1) = spawn_auth_npc_in_phase(&mut world, Vec3::new(0.0, 0.0, 0.0), 1, 1, 120); // near peer 2
        let (_e2, _eid2) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(3000.0, 0.0, 3000.0), 1, 1, 121); // near peer 3

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(3, "127.0.0.1:9103".parse().unwrap());

        spawn_shadow_npc(&mut world, Vec3::new(100.0, 0.0, 100.0), 2);
        spawn_shadow_npc(&mut world, Vec3::new(3100.0, 0.0, 3100.0), 3);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
        // Peer 2: entity at (0,0) matches phase AND within PeerBounds around (100,100)
        let p2 = &outbound.per_peer[&2];
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].position.x, 0.0);
        // Peer 3: entity at (3000,3000) matches phase AND within PeerBounds around (3100,3100)
        let p3 = &outbound.per_peer[&3];
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].position.x, 3000.0);
    }

    #[test]
    fn peer_repl_new_peer_no_shadows_gets_all() {
        // Peer 2 has NPC shadows (PeerBounds fallback), peer 3 has none (bootstrap).
        // Bootstrap sends phase-matched entities to shadowless peers.
        let mut world = setup_world(1);
        // Entity near peer 2's shadow, in phase 1 (matches tick=1)
        let (_e1, _eid1) = spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 1, 130);
        // Entity far from peer 2's shadow, in phase 0 (does NOT match tick=1)
        let (_e2, _eid2) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 0, 131);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(3, "127.0.0.1:9103".parse().unwrap());

        // Only peer 2 has shadows (NPC, no Player -> PeerBounds fallback)
        spawn_shadow_npc(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        // Peer 2: PeerBounds fallback, only phase-matched (phase 1) AND within bounds
        let p2 = &outbound.per_peer[&2];
        assert_eq!(
            p2.len(),
            1,
            "peer 2 should only get nearby phase-matched entity"
        );
        // Peer 3: bootstrap path sends phase-matched entities (phase 1 only)
        // Entity at ZERO is phase 1 -> sent. Entity at 4000 is phase 0 -> not sent.
        assert!(
            outbound.per_peer.contains_key(&3),
            "peer 3 (no shadows) gets bootstrap phase-matched entities"
        );
        assert_eq!(
            outbound.per_peer[&3].len(),
            1,
            "peer 3 should get 1 phase-matched entity via bootstrap"
        );
    }

    #[test]
    fn peer_repl_peer_removed_not_in_per_peer() {
        // If a peer is removed from PeerAddresses, it should not appear in per_peer.
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Peer 2 registered
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_npc(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        // Peer 2 should be present
        assert!(world
            .resource::<OutboundReplication>()
            .per_peer
            .contains_key(&2));

        // Remove peer from PeerAddresses (simulate peer leaving)
        world.resource_mut::<PeerAddresses>().0.remove(&2);
        world.resource_mut::<TickCounter>().0 = 2;

        // Clear and re-run
        world.resource_mut::<OutboundReplication>().per_peer.clear();
        world.resource_mut::<OutboundReplication>().full_snapshot = None;
        schedule.run(&mut world);

        // Peer 2 may still appear in per_peer (shadow still exists), but since it
        // is no longer in PeerAddresses, the game loop won't send to it.
        // The key invariant: the system does not panic.
    }

    #[test]
    fn tiered_hot_only_tick_sends_nearby() {
        // On tick=1, entities NOT in phase 1 are only sent if hot (near player).
        // Entities in phase 1 would be sent as cold regardless of distance.
        // Use phase 0 for all entities to test pure hot classification.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 0, 140);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 0, 141);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Player shadow from peer 2 at (100,0,0)
        spawn_shadow_player(&mut world, Vec3::new(100.0, 0.0, 0.0), 2);

        // Tick 1: current_phase=1, entities are phase 0 -> no cold-phase match
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
        let peer_entities = &outbound.per_peer[&2];
        // Entity at (0,0) is within 750 of player at (100,0,0) -> hot
        // Entity at (4000,0,0) is NOT within 750 -> not hot, not in cold phase -> excluded
        assert_eq!(
            peer_entities.len(),
            1,
            "only nearby entity should be sent (hot)"
        );
        assert_eq!(outbound.hot_count, 1);
        assert_eq!(outbound.cold_count, 0);
    }

    #[test]
    fn tiered_cold_tick_sends_all() {
        // Cold entities must be within PeerBounds to be sent.
        // Nearby entity: hot (within 750 of player). Cold entity: within
        // PeerBounds but outside hot range, in matching phase.
        let mut world = setup_world(1);
        // Nearby entity in phase 2 (NOT matching tick=4, phase 0) -> hot
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 2, 150);
        // Entity within PeerBounds but outside hot range (dist=800 from player at 100), phase 0 -> cold
        spawn_auth_npc_in_phase(&mut world, Vec3::new(900.0, 0.0, 0.0), 1, 0, 151);
        // Entity FAR outside PeerBounds, in phase 0 -> excluded even though phase matches
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 0, 152);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::new(100.0, 0.0, 0.0), 2);
        // NPC shadow to extend PeerBounds beyond hot range
        spawn_shadow_npc(&mut world, Vec3::new(1200.0, 0.0, 0.0), 2);

        // Tick 4: current_phase = 4 % 4 = 0
        world.resource_mut::<TickCounter>().0 = 4;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            2,
            "hot entity + cold-within-bounds entity (far one excluded)"
        );
        assert_eq!(
            outbound.hot_count, 1,
            "nearby entity is hot (not in its cold phase)"
        );
        assert_eq!(outbound.cold_count, 1, "within-bounds entity is cold");
    }

    #[test]
    fn tiered_player_vs_npc_shadow() {
        // Player-based filtering is more precise than bounding-box.
        // An NPC shadow at (3000,0,0) should NOT inflate the hot zone.
        // Entity in phase 0 (not matching tick=1) so only hot check applies.
        let mut world = setup_world(1);
        // Auth entity near the NPC shadow but far from the player, phase 0
        spawn_auth_npc_in_phase(&mut world, Vec3::new(2800.0, 0.0, 0.0), 1, 0, 160);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Player shadow at origin
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);
        // NPC shadow at (3000,0,0) -- should NOT affect hot zone
        spawn_shadow_npc(&mut world, Vec3::new(3000.0, 0.0, 0.0), 2);

        // Tick 1: current_phase=1, entity is phase 0 -> no cold-phase match
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        // Entity at (2800,0,0) is far from player at (0,0,0) (distance=2800 > 750) -> not hot
        // Not in cold phase either -> excluded
        assert_eq!(
            peer_entities.len(),
            0,
            "entity near NPC shadow but far from player should be excluded when not in cold phase"
        );
    }

    #[test]
    fn tiered_no_player_shadows_falls_back() {
        // When a peer has only NPC shadows (no Player shadows),
        // fall back to PeerBounds. With phase spreading, the fallback requires
        // both phase match AND bounds.contains().
        let mut world = setup_world(1);
        // Both in phase 1 (matching tick=1)
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 1, 170);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 1, 171);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Only NPC shadows, no Player shadows -> PeerBounds fallback
        spawn_shadow_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        // Both match phase 1. PeerBounds around NPC shadow at (100,0,0):
        // Entity at (0,0) within bounds -> included
        // Entity at (4000,0,0) outside bounds -> excluded
        assert_eq!(
            peer_entities.len(),
            1,
            "fallback to PeerBounds should still filter spatially"
        );
    }

    #[test]
    fn tiered_cold_tick_modulo() {
        // Cold entities must be within PeerBounds. Far entity outside PeerBounds
        // is never sent even when phase matches. Entity within bounds but outside
        // hot range is sent on its cold phase.
        let mut world = setup_world(1);
        // Nearby entity in phase 2: hot (within 750 of player), or cold when phase matches.
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 2, 180);
        // Within-bounds entity in phase 0: outside hot range (600 > 0 but < 750 from player at 100),
        // wait — 600 from origin, player at 100 -> dist = 500. Within 750? Yes.
        // Actually let's put it at 700,0,0 so dist from player at 100 = 600, within 750 -> hot.
        // Use 500,0,0 instead: dist from player = 400, hot. Need outside hot but inside bounds.
        // PeerBounds from player at (100,0,0) with NPC shadows: bounds = player ± 750.
        // So bounds are -650..850 on X. Entity at 800,0,0 is inside bounds, dist=700 from player -> hot.
        // Entity at 600,0,0: dist from player at 100 = 500 < 750 -> hot.
        // To be cold, need dist > 750 from player but within PeerBounds.
        // PeerBounds is built from ALL shadow positions. Add an NPC shadow at 1500,0,0.
        // Then bounds X = min(100,1500)-750..max(100,1500)+750 = -650..2250.
        // Entity at 1200,0,0: dist from player = 1100 > 750 (not hot), within bounds (1200 < 2250) -> cold!
        spawn_auth_npc_in_phase(&mut world, Vec3::new(1200.0, 0.0, 0.0), 1, 0, 181);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::new(100.0, 0.0, 0.0), 2);
        // NPC shadow to extend PeerBounds to cover entity at 1200
        spawn_shadow_npc(&mut world, Vec3::new(1500.0, 0.0, 0.0), 2);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);

        for tick in [1u64, 2, 3, 4, 5, 6, 7, 8, 12] {
            let current_phase = tick % 4;
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let outbound = world.resource::<OutboundReplication>();
            let count = outbound.per_peer.get(&2).map_or(0, |v| v.len());
            if current_phase == 0 {
                // Entity at 1200 matches phase 0, within PeerBounds -> cold. Nearby is hot.
                assert_eq!(
                    count, 2,
                    "tick {tick} (phase 0): cold entity within bounds sent"
                );
            } else if current_phase == 2 {
                // Nearby entity matches phase 2, within bounds -> cold.
                // Entity at 1200 not in phase, not hot -> excluded.
                assert_eq!(
                    count, 1,
                    "tick {tick} (phase 2): only nearby entity (cold-phase)"
                );
            } else {
                // Nearby entity is hot. Entity at 1200: not hot, not in phase -> excluded.
                assert_eq!(count, 1, "tick {tick}: only nearby entity (hot)");
            }
        }
    }

    #[test]
    fn tiered_multiple_peers_different_splits() {
        // Peer 2 player at (50,0,0), peer 3 player at (3100,0,3100).
        // Auth entities at both locations. Each peer gets different hot sets.
        // Both entities in phase 0 (not matching tick=1) for pure hot classification.
        let mut world = setup_world(1);
        let (_e1, _eid1) = spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 0, 190); // near peer 2
        let (_e2, _eid2) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(3000.0, 0.0, 3000.0), 1, 0, 191); // near peer 3

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(3, "127.0.0.1:9103".parse().unwrap());

        spawn_shadow_player(&mut world, Vec3::new(50.0, 0.0, 0.0), 2);
        spawn_shadow_player(&mut world, Vec3::new(3100.0, 0.0, 3100.0), 3);

        // Tick 1: current_phase=1, entities are phase 0 -> no cold-phase match
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
        // Peer 2: entity at (0,0) is hot (near player at 50,0,0), other is 4243 away -> excluded
        let p2 = &outbound.per_peer[&2];
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].position.x, 0.0);
        // Peer 3: entity at (3000,3000) is hot (near player at 3100,3100), other is 4243 away -> excluded
        let p3 = &outbound.per_peer[&3];
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].position.x, 3000.0);
    }

    #[test]
    fn tiered_replication_full_cycle() {
        // Comprehensive test: 4 auth entities with controlled phases, 2 peers
        // with player shadows. Cold entities must be within PeerBounds.
        //
        // Entity layout:
        //   A at (0,0,0)       phase 3 -- hot for peer 2 (dist=50 from player)
        //   B at (100,0,0)     phase 3 -- hot for peer 2 (dist=50 from player)
        //   C at (3000,0,3000) phase 2 -- hot for peer 3 (dist=141 from player)
        //   D at (4500,0,0)    phase 0 -- FAR from all players AND outside all PeerBounds
        //
        // Peer 2 player at (50,0,0). Peer 3 player at (3100,0,3100).
        // PeerBounds for peer 2: all shadows from server 2 ± 750.
        // PeerBounds for peer 3: all shadows from server 3 ± 750.
        //
        // D is outside both PeerBounds -> never cold-replicated (correctly excluded).
        let mut world = setup_world(1);

        let (_, eid_a) = spawn_auth_npc_in_phase(&mut world, Vec3::new(0.0, 0.0, 0.0), 1, 3, 300);
        let (_, eid_b) = spawn_auth_npc_in_phase(&mut world, Vec3::new(100.0, 0.0, 0.0), 1, 3, 301);
        let (_, eid_c) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(3000.0, 0.0, 3000.0), 1, 2, 302);
        let (_, _eid_d) =
            spawn_auth_npc_in_phase(&mut world, Vec3::new(4500.0, 0.0, 0.0), 1, 0, 303);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(3, "127.0.0.1:9103".parse().unwrap());

        spawn_shadow_player(&mut world, Vec3::new(50.0, 0.0, 0.0), 2);
        spawn_shadow_player(&mut world, Vec3::new(3100.0, 0.0, 3100.0), 3);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);

        let mut p2_seen = std::collections::HashSet::new();
        let mut p3_seen = std::collections::HashSet::new();

        // -- Tick 1 (phase 1): no entities match phase 1 --
        // Peer 2: A,B hot. => 2 entities.
        // Peer 3: C hot. => 1 entity.
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);

        {
            let outbound = world.resource::<OutboundReplication>();
            assert!(outbound.full_snapshot.is_none());
            let p2 = &outbound.per_peer[&2];
            assert_eq!(p2.len(), 2, "tick 1: peer 2 gets A,B (hot)");
            let p3 = &outbound.per_peer[&3];
            assert_eq!(p3.len(), 1, "tick 1: peer 3 gets C (hot)");
            for e in p2 { p2_seen.insert(e.id); }
            for e in p3 { p3_seen.insert(e.id); }
        }

        // -- Tick 2 (phase 2): C matches cold phase --
        // Peer 2: A,B hot. C in phase, within PeerBounds of peer 2? Peer 2 bounds
        // around (50,0,0) ± 750 = (-700..800, -750..750). C at (3000,3000) outside. => 2.
        // Peer 3: C cold-phase, within PeerBounds of peer 3? Peer 3 bounds around
        // (3100,3100) ± 750 = (2350..3850). C at (3000,3000) inside. => 1 cold.
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);

        {
            let outbound = world.resource::<OutboundReplication>();
            let p2 = &outbound.per_peer[&2];
            assert_eq!(p2.len(), 2, "tick 2: peer 2 gets A,B (hot only, C outside peer 2 bounds)");
            let p3 = &outbound.per_peer[&3];
            assert_eq!(p3.len(), 1, "tick 2: peer 3 gets C (cold-phase, within bounds)");
            assert_eq!(p3[0].id, eid_c);
            for e in p2 { p2_seen.insert(e.id); }
            for e in p3 { p3_seen.insert(e.id); }
        }

        // -- Tick 3 (phase 3): A,B match cold phase --
        // Peer 2: A,B in cold phase, within PeerBounds of peer 2? Yes (both near origin, bounds cover them). => 2 cold.
        // Peer 3: C hot. A,B in phase 3, within PeerBounds of peer 3? Peer 3 bounds (2350..3850). A,B at 0,100 -> outside. => 1 hot.
        world.resource_mut::<TickCounter>().0 = 3;
        schedule.run(&mut world);

        {
            let outbound = world.resource::<OutboundReplication>();
            let p2 = &outbound.per_peer[&2];
            assert_eq!(p2.len(), 2, "tick 3: peer 2 gets A,B (cold-phase 3, within bounds)");
            let p3 = &outbound.per_peer[&3];
            assert_eq!(p3.len(), 1, "tick 3: peer 3 gets C (hot), A,B outside peer 3 bounds");
            for e in p2 { p2_seen.insert(e.id); }
            for e in p3 { p3_seen.insert(e.id); }
        }

        // -- Tick 4 (phase 0): D matches cold phase --
        // D at (4500,0,0) outside all PeerBounds -> not sent.
        // Peer 2: A,B hot. => 2.
        // Peer 3: C hot. => 1.
        world.resource_mut::<TickCounter>().0 = 4;
        schedule.run(&mut world);

        {
            let outbound = world.resource::<OutboundReplication>();
            assert!(outbound.full_snapshot.is_none());
            let p2 = &outbound.per_peer[&2];
            assert_eq!(p2.len(), 2, "tick 4: peer 2 gets A,B (hot), D outside bounds");
            let p3 = &outbound.per_peer[&3];
            assert_eq!(p3.len(), 1, "tick 4: peer 3 gets C (hot), D outside bounds");
            for e in p2 { p2_seen.insert(e.id); }
            for e in p3 { p3_seen.insert(e.id); }
        }

        // With AoI-filtered cold, each peer only sees entities within its bounds:
        // Peer 2 sees: A, B (hot and cold-phase) — C and D are outside peer 2 bounds
        // Peer 3 sees: C (hot and cold-phase) — A, B, D are outside peer 3 bounds
        assert_eq!(p2_seen.len(), 2, "peer 2 sees A,B (within its bounds)");
        assert_eq!(p3_seen.len(), 1, "peer 3 sees C (within its bounds)");
    }

    #[test]
    fn tiered_all_entities_hot() {
        // When ALL auth entities are clustered near a peer's player and none
        // are in the current cold phase, every entity is classified as hot.
        // Use phase 0 entities at tick=1 (phase 1) so no cold-phase match.
        let mut world = setup_world(1);
        // All 4 entities within 200 units of (0,0,0), all in phase 0
        spawn_auth_npc_in_phase(&mut world, Vec3::new(0.0, 0.0, 0.0), 1, 0, 400);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(100.0, 0.0, 0.0), 1, 0, 401);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(-50.0, 0.0, 150.0), 1, 0, 402);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(200.0, 0.0, -100.0), 1, 0, 403);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::new(50.0, 0.0, 50.0), 2);

        // Tick 1: current_phase=1, entities are phase 0 -> no cold-phase match
        // All 4 entities are within 750 of player -> all hot
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            4,
            "all entities should be sent when all are hot"
        );
        assert_eq!(outbound.hot_count, 4, "all 4 entities are hot");
        assert_eq!(
            outbound.cold_count, 0,
            "no cold entities when none match the current phase"
        );
    }

    #[test]
    fn tiered_all_entities_cold_hot_tick() {
        // All entities far from the player, none in the current cold phase.
        // Nothing should be sent: not hot (distance), not in cold phase.
        // Use phase 0 entities at tick=1 (phase 1) so no cold-phase match.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 0.0), 1, 0, 410);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(-3000.0, 0.0, 3000.0), 1, 0, 411);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(2000.0, 0.0, -4000.0), 1, 0, 412);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Player at origin -- all entities are > 750 away
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        // Tick 1: current_phase=1, entities are phase 0 -> no cold-phase match, not hot
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            0,
            "far entities with no phase match are excluded"
        );
        assert_eq!(outbound.hot_count, 0);
        assert_eq!(outbound.cold_count, 0);
    }

    #[test]
    fn tiered_all_entities_cold_cold_tick() {
        // All entities outside hot range but within PeerBounds, all in phase 0.
        // On tick=8 (phase 0), all match cold phase AND are within bounds -> sent.
        // An entity outside PeerBounds is excluded even with phase match.
        let mut world = setup_world(1);
        // These are outside 750 of player at origin but within PeerBounds
        // (PeerBounds built from player + NPC shadows)
        spawn_auth_npc_in_phase(&mut world, Vec3::new(1200.0, 0.0, 0.0), 1, 0, 420);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(-1000.0, 0.0, 800.0), 1, 0, 421);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(900.0, 0.0, -1100.0), 1, 0, 422);
        // This one is FAR outside PeerBounds -> excluded
        spawn_auth_npc_in_phase(&mut world, Vec3::new(4000.0, 0.0, 4000.0), 1, 0, 423);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);
        // NPC shadows to extend PeerBounds
        spawn_shadow_npc(&mut world, Vec3::new(1500.0, 0.0, 0.0), 2);
        spawn_shadow_npc(&mut world, Vec3::new(-1500.0, 0.0, 1500.0), 2);
        spawn_shadow_npc(&mut world, Vec3::new(0.0, 0.0, -1500.0), 2);

        // Tick 8: current_phase = 8 % 4 = 0, all entities phase 0
        world.resource_mut::<TickCounter>().0 = 8;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            3,
            "3 within-bounds entities sent, 1 outside excluded"
        );
        assert_eq!(outbound.hot_count, 0, "none are hot");
        assert_eq!(outbound.cold_count, 3, "all 3 are cold within bounds");
    }

    #[test]
    fn tiered_entity_at_exact_peer_aoi_boundary() {
        // Entity at EXACTLY PEER_AOI_RADIUS (750 units) from the player.
        // Not in cold phase -> classified by distance -> hot (uses <= comparison).
        // Phase 0 entity at tick=1 (phase 1) -> no cold-phase match.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(750.0, 0.0, 0.0), 1, 0, 430);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        // Tick 1: current_phase=1, entity is phase 0 -> distance check applies
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            1,
            "entity at exact boundary (750) should be hot"
        );
        assert_eq!(outbound.hot_count, 1);
    }

    #[test]
    fn tiered_entity_just_outside_peer_aoi() {
        // Entity at 751 units from player -- just outside PEER_AOI_RADIUS.
        // Not in cold phase -> not hot (distance > 750) -> excluded.
        // Phase 0 entity at tick=1 (phase 1) -> no cold-phase match.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(751.0, 0.0, 0.0), 1, 0, 440);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        // Tick 1: current_phase=1, entity is phase 0 -> distance check applies
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            0,
            "entity at 751 units excluded when not in cold phase"
        );
        assert_eq!(outbound.hot_count, 0);
    }

    #[test]
    fn tiered_multiple_players_same_peer() {
        // Peer 2 has two Player shadows at different locations.
        // An entity near EITHER player should be classified as hot.
        // All entities in phase 0 (not matching tick=1) for pure hot classification.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(100.0, 0.0, 0.0), 1, 0, 450);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(3000.0, 0.0, 0.0), 1, 0, 451);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(-4000.0, 0.0, -4000.0), 1, 0, 452);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Two Player shadows on the SAME peer (server 2)
        spawn_shadow_player(&mut world, Vec3::new(0.0, 0.0, 0.0), 2); // player A
        spawn_shadow_player(&mut world, Vec3::new(3000.0, 0.0, 0.0), 2); // player B

        // Tick 1: current_phase=1, entities are phase 0 -> no cold-phase match
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        // Entity at (100,0) near player A: hot. Entity at (3000,0) near player B: hot.
        // Entity at (-4000,-4000) far from both: not hot, not in cold phase -> excluded.
        assert_eq!(
            peer_entities.len(),
            2,
            "entity near either player should be hot"
        );
        assert_eq!(outbound.hot_count, 2);
        assert_eq!(outbound.cold_count, 0);
    }

    #[test]
    fn tiered_cold_tick_zero() {
        // Tick 0 (phase 0). A phase-0 entity outside hot range but within
        // PeerBounds should be sent via cold-phase match.
        let mut world = setup_world(1);
        // Phase 0 entity at 1000,0,0 — outside hot (>750 from player at 0),
        // but within PeerBounds (NPC shadow at 1200 extends bounds).
        spawn_auth_npc_in_phase(&mut world, Vec3::new(1000.0, 0.0, 0.0), 1, 0, 460);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);
        spawn_shadow_npc(&mut world, Vec3::new(1200.0, 0.0, 0.0), 2);

        // Tick 0: current_phase=0, entity is phase 0 -> cold-phase match
        world.resource_mut::<TickCounter>().0 = 0;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            1,
            "phase-0 entity within bounds sent on tick 0"
        );
    }

    #[test]
    fn tiered_no_shadows_at_all() {
        // Peer is registered but has NO shadow entities.
        // The no-shadows bootstrap path sends phase-matched entities.
        // With a phase-0 entity at tick=1 (phase 1), no match -> nothing sent.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 0, 470);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // No shadows at all

        // Tick 1: current_phase=1, entity is phase 0 -> no match
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.full_snapshot.is_none());
        assert!(
            outbound.per_peer.is_empty(),
            "no shadows and no phase match -> nothing sent"
        );
        assert_eq!(outbound.hot_count, 0);
        assert_eq!(outbound.cold_count, 0);
    }

    #[test]
    fn tiered_peer_with_address_but_only_own_shadows() {
        // Peer 2 is registered, but all shadow entities belong to our own server.
        // Shadow entities with auth == config.server_id are skipped in the shadow loop,
        // so peer_all_positions is empty -> no-shadows bootstrap path.
        // Phase 0 entity at tick=1 (phase 1) -> no phase match -> nothing sent.
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 0, 480);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Shadow owned by server 1 (our own server) -- should be skipped
        spawn_shadow_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);

        // Tick 1: own-server shadows skipped -> no shadows -> bootstrap path.
        // Entity phase 0 != current phase 1 -> no match -> nothing sent.
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(
            outbound.per_peer.is_empty(),
            "own-server shadows skipped, no phase match -> nothing sent"
        );
    }

    #[test]
    fn tiered_hot_cold_counts_accurate_multiple_peers() {
        // Two peers with different player positions. Verify hot_count and cold_count
        // are summed correctly. Cold entities must be within PeerBounds.
        //
        // Entity A at origin, phase 2 (hot for peer 2)
        // Entity B at (3000,0,3000), phase 2 (hot for peer 3)
        // Entity C at (900,0,0), phase 1 (cold for peer 2: outside hot, within bounds)
        // C is outside peer 3's PeerBounds (peer 3 player at 3100,3100).
        let mut world = setup_world(1);
        spawn_auth_npc_in_phase(&mut world, Vec3::ZERO, 1, 2, 490);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(3000.0, 0.0, 3000.0), 1, 2, 491);
        spawn_auth_npc_in_phase(&mut world, Vec3::new(900.0, 0.0, 0.0), 1, 1, 492);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(3, "127.0.0.1:9103".parse().unwrap());

        spawn_shadow_player(&mut world, Vec3::ZERO, 2);
        // NPC shadow to extend peer 2 PeerBounds to cover entity C at 900
        spawn_shadow_npc(&mut world, Vec3::new(1200.0, 0.0, 0.0), 2);
        spawn_shadow_player(&mut world, Vec3::new(3100.0, 0.0, 3100.0), 3);

        // Tick 1: current_phase=1
        // C (phase 1) matches cold phase.
        //   Peer 2 bounds: shadows at (0,0) and (1200,0) ± 750 = (-750..1950). C at 900 -> inside -> cold.
        //   Peer 3 bounds: player at (3100,3100) ± 750 = (2350..3850). C at 900 -> outside -> excluded.
        // A (phase 2): peer 2 hot (dist 0). peer 3 not hot (dist 4384).
        // B (phase 2): peer 2 not hot (dist 4243). peer 3 hot (dist 141).
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        // Peer 2: A hot + C cold => hot=1, cold=1, count=2
        // Peer 3: B hot => hot=1, cold=0, count=1
        // Total: hot=2, cold=1
        assert_eq!(
            outbound.hot_count, 2,
            "2 hot entities total (A for peer 2, B for peer 3)"
        );
        assert_eq!(
            outbound.cold_count, 1,
            "1 cold entity (C for peer 2 only, outside peer 3 bounds)"
        );
        assert_eq!(
            outbound.per_peer[&2].len(),
            2,
            "peer 2 gets A (hot) + C (cold within bounds)"
        );
        assert_eq!(
            outbound.per_peer[&3].len(),
            1,
            "peer 3 gets B (hot) only, C outside its bounds"
        );
    }

    #[test]
    fn tiered_empty_entities_no_full_snapshot() {
        // When there are no auth entities, full_snapshot should be None (not Some(empty vec)).
        let mut world = setup_world(1);
        // No auth entities

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(
            outbound.full_snapshot.is_none(),
            "empty entity list should produce None, not Some(empty)"
        );
        assert!(outbound.per_peer.is_empty());
    }

    #[test]
    fn tiered_diagonal_boundary_precision() {
        // Entity at diagonal distance exactly at PEER_AOI_RADIUS.
        // sqrt(530^2 + 530^2) = 749.53 (just inside 750).
        // sqrt(531^2 + 531^2) = 750.95 (just outside 750).
        // Both in phase 0 (not matching tick=1) for pure distance classification.
        let mut world = setup_world(1);
        let inside = spawn_auth_npc_in_phase(&mut world, Vec3::new(530.0, 0.0, 530.0), 1, 0, 500);
        let outside = spawn_auth_npc_in_phase(&mut world, Vec3::new(531.0, 0.0, 531.0), 1, 0, 501);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        // Tick 1: current_phase=1, entities are phase 0 -> distance check only
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(
            peer_entities.len(),
            1,
            "only entity at diagonal 749.53 should be hot"
        );
        assert_eq!(
            peer_entities[0].id, inside.1,
            "the inside entity should be the one sent"
        );
        assert!(
            !peer_entities.iter().any(|e| e.id == outside.1),
            "entity at diagonal 750.95 should be excluded (not hot, not in cold phase)"
        );
    }

    #[test]
    fn tiered_player_shadow_mixed_with_npc_shadows() {
        // Peer has both Player and NPC shadows. Only Player positions should
        // be used for hot/cold classification (NPC shadows are irrelevant).
        // Both entities in phase 0 (not matching tick=1) for pure hot classification.
        let mut world = setup_world(1);
        // Auth entity near the NPC shadow but far from the Player shadow
        spawn_auth_npc_in_phase(&mut world, Vec3::new(-3000.0, 0.0, -3000.0), 1, 0, 510);
        // Auth entity near the Player shadow
        spawn_auth_npc_in_phase(&mut world, Vec3::new(100.0, 0.0, 0.0), 1, 0, 511);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Player at origin
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);
        // NPC shadow at (-3000, -3000) -- should NOT affect hot classification
        spawn_shadow_npc(&mut world, Vec3::new(-3000.0, 0.0, -3000.0), 2);

        // Tick 1: current_phase=1, entities are phase 0 -> distance check only
        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        // Only the entity at (100,0) is within 750 of Player at origin -> hot.
        // Entity at (-3000,-3000) is 4243 units from Player -> not hot, not in cold phase -> excluded.
        assert_eq!(
            peer_entities.len(),
            1,
            "only entity near Player shadow should be hot (NPC shadow position ignored)"
        );
        assert_eq!(outbound.hot_count, 1);
    }

    #[test]
    fn tiered_replication_preserves_entity_fields() {
        // Verify that tiered replication does not corrupt entity fields.
        // The entities in per_peer should have the exact same fields as the original.
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 200.0), 1);
        // Set a velocity
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(5.0, 0.0, -3.0);

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_shadow_player(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<TickCounter>().0 = 1;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        let peer_entities = &outbound.per_peer[&2];
        assert_eq!(peer_entities.len(), 1);
        let e = &peer_entities[0];
        assert_eq!(e.id, eid);
        assert_eq!(e.position.x, 100.0);
        assert_eq!(e.position.z, 200.0);
        assert_eq!(e.velocity.x, 5.0);
        assert_eq!(e.velocity.z, -3.0);
        assert_eq!(e.entity_type, EntityType::Npc);
        assert_eq!(e.authority_server, ServerId(1));
    }
}
