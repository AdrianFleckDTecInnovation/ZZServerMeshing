use crate::components::*;
use crate::conversion::{despawn_and_remove, to_entity_state};
use crate::game_loop::serialize_versioned;
use crate::resources::*;
use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityType, ServerId};
use shared::protocol::{CoordClientMsg, PeerMessage};

/// Ticks before a pending authority transfer is considered timed out.
const TRANSFER_TIMEOUT_TICKS: u64 = 10; // 500ms at 20Hz

/// Maximum number of retries for a timed-out authority transfer.
/// 3 retries = 4 total attempts (initial + 3) over ~2 seconds at 20Hz.
/// Reduces probability of permanent entity duplication from lost ACKs
/// to ~0.0001% on WiFi (assuming 5% per-packet loss).
pub(super) const TRANSFER_MAX_RETRIES: u8 = 3;

/// Process inbound TransferAck messages. For each ACK, look up the entity
/// and despawn it if it has a TransferOutbound component (two-phase transfer complete).
pub fn process_transfer_acks(world: &mut World) {
    let acks = std::mem::take(&mut world.resource_mut::<InboundQueue>().transfer_acks);

    for ack in acks {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&ack.entity_id) {
            if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                let target_server = outbound.target_server;
                tracing::info!(
                    entity = %ack.entity_id,
                    tick = ack.tick,
                    target = target_server,
                    "Transfer ACK received, despawning entity"
                );
                // Report success to coordinator (closed-loop rebalancing)
                world.resource_mut::<OutboundCoordMessages>().messages.push(
                    CoordClientMsg::TransferResult {
                        entity_id: ack.entity_id.to_string(),
                        target_server,
                        success: true,
                    },
                );
                despawn_and_remove(world, bevy_entity, ack.entity_id);
            } else {
                tracing::debug!(
                    entity = %ack.entity_id,
                    "Transfer ACK for entity without TransferOutbound, ignoring"
                );
            }
        } else {
            tracing::debug!(
                entity = %ack.entity_id,
                "Transfer ACK for unknown entity, ignoring"
            );
        }
    }
}

/// Check for timed-out authority transfers. Retry once, then abort (keep entity).
/// On retry, re-serializes the entity's current ECS state instead of using stale data.
pub fn process_transfer_timeouts(world: &mut World) {
    let current_tick = world.resource::<TickCounter>().0;
    let server_id = world.resource::<ServerConfig>().server_id;

    // Collect entities with TransferOutbound that need action
    let mut to_retry: Vec<(Entity, EntityId, std::net::SocketAddr, u16)> = Vec::new();
    let mut to_abort: Vec<(Entity, EntityId)> = Vec::new();

    let mut query = world.query::<(Entity, &NetworkId, &TransferOutbound)>();
    for (entity, nid, outbound) in query.iter(&world) {
        if current_tick.saturating_sub(outbound.sent_tick) > TRANSFER_TIMEOUT_TICKS {
            if outbound.retry_count < TRANSFER_MAX_RETRIES {
                to_retry.push((
                    entity,
                    nid.0,
                    outbound.target_addr,
                    outbound.target_server,
                ));
            } else {
                to_abort.push((entity, nid.0));
            }
        }
    }

    // Process retries: re-serialize current ECS state for each entity
    for (entity, eid, target_addr, target_server) in to_retry {
        // Build fresh AuthorityTransfer from current ECS components
        let nid = world.get::<NetworkId>(entity).unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        let vel = world.get::<Velocity>(entity).unwrap();
        let kind = world.get::<EntityKind>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let entity_state = to_entity_state(
            nid, pos, vel, kind,
            &AuthorityServer(ServerId(target_server)),
            seq,
        );

        let msg = PeerMessage::AuthorityTransfer {
            tick: current_tick,
            entity_id: eid,
            from_server: server_id,
            to_server: ServerId(target_server),
            full_state: entity_state,
        };

        if let Some(bytes) = serialize_versioned(&msg) {
            world
                .resource_mut::<OutboundTransfers>()
                .messages
                .push((target_addr, bytes));
        }

        if let Some(mut outbound) = world.get_mut::<TransferOutbound>(entity) {
            outbound.retry_count += 1;
            outbound.sent_tick = current_tick;
            let target = outbound.target_server;
            let retry = outbound.retry_count;
            tracing::warn!(
                entity = %eid,
                target = target,
                retry = retry,
                "Retrying transfer with fresh state (attempt {})",
                retry,
            );
        }
    }

    // Process aborts: remove TransferOutbound, entity keeps Authoritative
    for (entity, eid) in to_abort {
        let target = world
            .get::<TransferOutbound>(entity)
            .map(|o| o.target_server)
            .unwrap_or(0);
        let retries = world
            .get::<TransferOutbound>(entity)
            .map(|o| o.retry_count)
            .unwrap_or(0);
        world.entity_mut(entity).remove::<TransferOutbound>();
        // Report failure to coordinator (closed-loop rebalancing)
        world.resource_mut::<OutboundCoordMessages>().messages.push(
            CoordClientMsg::TransferResult {
                entity_id: eid.to_string(),
                target_server: target,
                success: false,
            },
        );
        tracing::error!(
            entity = %eid,
            target = target,
            retries = retries,
            "Transfer failed after {} retries, keeping entity",
            retries,
        );
    }
}

/// Process pending authority transfers from coordinator rebalance directives.
pub fn process_transfers(world: &mut World) {
    let transfer = world.resource_mut::<PendingTransfers>().transfers.take();
    let Some((_entity_ids, target_server)) = transfer else {
        return;
    };

    let server_id = world.resource::<ServerConfig>().server_id;
    let tick = world.resource::<TickCounter>().0;

    // Look up target server's UDP address
    let target_addr = match world.resource::<PeerAddresses>().0.get(&target_server) {
        Some(addr) => *addr,
        None => {
            tracing::warn!("Rebalance target S{target_server} not in peer list");
            return;
        }
    };

    // Collect NPC entities sorted by distance from origin (descending).
    // Skip entities already in transfer (TransferOutbound).
    let mut npc_candidates: Vec<(Entity, EntityId, f32)> = Vec::new();
    let mut total_auth = 0usize;

    let mut query = world.query_filtered::<(Entity, &NetworkId, &Position, &EntityKind), With<Authoritative>>();
    for (entity, nid, pos, kind) in query.iter(&world) {
        total_auth += 1;
        if kind.0 == EntityType::Npc && world.get::<TransferOutbound>(entity).is_none() {
            npc_candidates.push((entity, nid.0, pos.0.length_squared()));
        }
    }

    // Sort by distance from origin (descending) -- transfer peripheral entities first
    npc_candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Transfer up to 10% of our entities
    let transfer_count = (total_auth / 10).max(1).min(npc_candidates.len());

    let mut transferred = 0;
    for (entity, eid, _) in npc_candidates.iter().take(transfer_count) {
        // Build EntityState from components
        let nid = world.get::<NetworkId>(*entity).unwrap();
        let pos = world.get::<Position>(*entity).unwrap();
        let vel = world.get::<Velocity>(*entity).unwrap();
        let kind = world.get::<EntityKind>(*entity).unwrap();
        let seq = world.get::<LastInputSeq>(*entity);

        let entity_state = to_entity_state(nid, pos, vel, kind, &AuthorityServer(ServerId(target_server)), seq);

        let msg = PeerMessage::AuthorityTransfer {
            tick,
            entity_id: *eid,
            from_server: server_id,
            to_server: ServerId(target_server),
            full_state: entity_state,
        };

        if let Some(bytes) = serialize_versioned(&msg) {
            world
                .resource_mut::<OutboundTransfers>()
                .messages
                .push((target_addr, bytes));

            // Two-phase: mark entity as TransferOutbound instead of despawning.
            // Entity keeps Authoritative but is frozen (excluded from sim/replication via query filters).
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server,
                target_addr,
                sent_tick: tick,
                retry_count: 0,
            });
            transferred += 1;
        }
        // DO NOT despawn -- entity will be despawned when process_transfer_acks receives the ACK.
    }

    if transferred > 0 {
        let remaining = total_auth - transferred;
        tracing::info!(
            "Transferred {transferred} entities to S{target_server} ({remaining} remaining)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::world::World;
    use glam::Vec3;
    use shared::entity::{EntityState, ServerId};
    use shared::protocol::PeerMessage;

    /// Deserialize a versioned UDP message (skip 2-byte version prefix).
    fn deserialize_versioned(bytes: &[u8]) -> PeerMessage {
        assert!(bytes.len() >= 2, "versioned message too short");
        let version = u16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(version, shared::PROTOCOL_VERSION, "unexpected protocol version in test");
        bincode::deserialize(&bytes[2..]).unwrap()
    }

    fn setup_world(server_id: u16) -> World {
        let mut world = World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(server_id),
        });
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

    // -- process_transfers tests --

    #[test]
    fn transfer_only_npcs_not_players() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // 3 NPCs and 1 Player
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);
        spawn_auth_npc(&mut world, Vec3::Z, 1);
        let player_state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let player_entity = spawn_from_entity_state_direct(&mut world, &player_state, true, None);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // Player should NOT have TransferOutbound
        assert!(world.get::<TransferOutbound>(player_entity).is_none());
    }

    #[test]
    fn transfer_produces_outbound_messages() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        assert!(!world.resource::<OutboundTransfers>().messages.is_empty());
    }

    // -- process_transfers: no peer address for target --

    #[test]
    fn transfer_no_peer_address_noop() {
        let mut world = setup_world(1);
        // Do NOT add peer address for server 2
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // No transfers should have happened
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
        // No TransferOutbound components
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 0);
    }

    // -- process_transfers: peripheral entities transferred first --

    #[test]
    fn transfer_peripheral_entities_first() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // 10 entities: inner at (1,0,1) to (10,0,10), one outer at (4000,0,4000)
        for i in 1..=9 {
            spawn_auth_npc(&mut world, Vec3::new(i as f32, 0.0, i as f32), 1);
        }
        let (outer_entity, _) = spawn_auth_npc(&mut world, Vec3::new(4000.0, 0.0, 4000.0), 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // The outer entity should be transferred first
        assert!(world.get::<TransferOutbound>(outer_entity).is_some(),
            "peripheral entity should be transferred first");
    }

    // -- EntityLookup consistency after transfer out --

    #[test]
    fn entity_lookup_consistent_after_transfer() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // Entity should still be in lookup (not despawned yet -- waiting for ACK)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));

        // Now process the ACK
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 6 }
        );
        process_transfer_acks(&mut world);

        // Now it should be gone
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- process_transfer_acks tests --

    #[test]
    fn process_transfer_acks_despawns_outbound_entity() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        process_transfer_acks(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn process_transfer_acks_ignores_unknown_entity() {
        let mut world = setup_world(1);

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: EntityId::new(), tick: 5 }
        );

        process_transfer_acks(&mut world);
    }

    #[test]
    fn process_transfer_acks_ignores_entity_without_outbound() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        process_transfer_acks(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- process_transfer_timeouts tests --

    #[test]
    fn transfer_timeout_retries_once() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        world.resource_mut::<TickCounter>().0 = 12;

        process_transfer_timeouts(&mut world);

        assert_eq!(world.resource::<OutboundTransfers>().messages.len(), 1);
        let outbound = world.get::<TransferOutbound>(entity).unwrap();
        assert_eq!(outbound.retry_count, 1);
        assert_eq!(outbound.sent_tick, 12);
    }

    #[test]
    fn transfer_timeout_aborts_after_max_retries() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: TRANSFER_MAX_RETRIES,
        });

        world.resource_mut::<TickCounter>().0 = 22;

        process_transfer_timeouts(&mut world);

        assert!(world.get::<TransferOutbound>(entity).is_none());
        assert!(world.get::<Authoritative>(entity).is_some());
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
    }

    #[test]
    fn transfer_timeout_no_action_before_timeout() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        world.resource_mut::<TickCounter>().0 = 10;

        process_transfer_timeouts(&mut world);

        assert!(world.get::<TransferOutbound>(entity).is_some());
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 0);
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
    }

    #[test]
    fn process_transfer_acks_duplicate_ack_no_panic() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 6 }
        );

        process_transfer_acks(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn process_transfers_skips_entities_with_transfer_outbound() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let mut entities = Vec::new();
        for _ in 0..5 {
            let state = EntityState::new_npc(Vec3::new(100.0, 0.0, 100.0), ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            entities.push((entity, eid));
        }

        for (entity, _) in entities.iter().take(2) {
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 1,
                retry_count: 0,
            });
        }

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        let new_outbound_count = entities.iter()
            .filter(|(e, _)| world.get::<TransferOutbound>(*e).is_some())
            .count();
        assert!(new_outbound_count >= 2 && new_outbound_count <= 3);

        for (entity, _) in entities.iter().take(2) {
            let outbound = world.get::<TransferOutbound>(*entity).unwrap();
            assert_eq!(outbound.sent_tick, 1);
        }
    }

    #[test]
    fn process_transfer_acks_drains_entire_queue() {
        let mut world = setup_world(1);

        let mut eids = Vec::new();
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            world.entity_mut(entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 1,
                retry_count: 0,
            });
            eids.push(eid);
        }

        for eid in &eids {
            world.resource_mut::<InboundQueue>().transfer_acks.push(
                InboundTransferAck { entity_id: *eid, tick: 5 }
            );
        }

        process_transfer_acks(&mut world);

        for eid in &eids {
            assert!(!world.resource::<EntityLookup>().0.contains_key(eid));
        }
        assert!(world.resource::<InboundQueue>().transfer_acks.is_empty());
    }

    #[test]
    fn transfer_timeout_exact_boundary_no_action() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        world.resource_mut::<TickCounter>().0 = 15;

        process_transfer_timeouts(&mut world);

        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 0);
    }

    #[test]
    fn transfer_timeout_one_past_boundary_retries() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        world.resource_mut::<TickCounter>().0 = 16;

        process_transfer_timeouts(&mut world);

        assert_eq!(world.resource::<OutboundTransfers>().messages.len(), 1);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 1);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().sent_tick, 16);
    }

    #[test]
    fn transfer_ack_for_already_despawned_entity_is_noop() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        despawn_and_remove(&mut world, entity, eid);

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        process_transfer_acks(&mut world);
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn multiple_simultaneous_transfers_get_independent_acks() {
        let mut world = setup_world(1);

        let mut eids = Vec::new();
        let mut entities = Vec::new();
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            world.entity_mut(entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 1,
                retry_count: 0,
            });
            eids.push(eid);
            entities.push(entity);
        }

        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eids[0], tick: 5 }
        );
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eids[1], tick: 5 }
        );

        process_transfer_acks(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eids[0]));
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eids[1]));
        assert!(world.resource::<EntityLookup>().0.contains_key(&eids[2]));
        assert!(world.get::<TransferOutbound>(entities[2]).is_some());
    }

    #[test]
    fn transfer_retry_uses_current_position() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(99.0, 0.0, 99.0);

        world.resource_mut::<TickCounter>().0 = 12;
        process_transfer_timeouts(&mut world);

        let messages = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(messages.len(), 1);

        let msg: PeerMessage = deserialize_versioned(&messages[0].1);
        match msg {
            PeerMessage::AuthorityTransfer { full_state, entity_id, tick, .. } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 12);
                assert!((full_state.position.x - 99.0).abs() < 0.001);
            }
            _ => panic!("expected AuthorityTransfer, got {:?}", msg),
        }
    }

    #[test]
    fn transfer_timeout_full_retry_cycle() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        // Retry 1
        world.resource_mut::<TickCounter>().0 = 11;
        process_transfer_timeouts(&mut world);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 1);
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Retry 2
        world.resource_mut::<TickCounter>().0 = 22;
        process_transfer_timeouts(&mut world);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 2);
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Retry 3
        world.resource_mut::<TickCounter>().0 = 33;
        process_transfer_timeouts(&mut world);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 3);
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Abort
        world.resource_mut::<TickCounter>().0 = 44;
        process_transfer_timeouts(&mut world);
        assert!(world.get::<TransferOutbound>(entity).is_none());
        assert!(world.get::<Authoritative>(entity).is_some());
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
    }

    // -- Duplicate rebalance directive --

    #[test]
    fn duplicate_rebalance_directive_does_not_double_transfer() {
        let mut world = setup_world(1);
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::new(200.0, 0.0, 200.0), 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 10;

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        process_transfers(&mut world);

        let first_outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert!(first_outbound > 0);

        world.resource_mut::<OutboundTransfers>().messages.clear();

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        process_transfers(&mut world);

        let total = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(total, 5);

        let mut query = world.query::<&TransferOutbound>();
        for outbound in query.iter(&world) {
            assert!(outbound.sent_tick >= 10);
        }
    }
}
