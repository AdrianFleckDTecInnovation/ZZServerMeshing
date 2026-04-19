use crate::components::*;
use crate::conversion::{despawn_and_remove, to_entity_state};
use crate::resources::*;
use bevy_ecs::prelude::*;
use shared::entity::{EntityId, EntityType, ServerId};
use shared::protocol::{CoordClientMsg, PeerMessage, ServerMessage};

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
                        entity_id: ack.entity_id,
                        target_server: ServerId(target_server),
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
    let session_id = world.resource::<SessionId>().0;

    // Collect entities with TransferOutbound that need action
    let mut to_retry: Vec<(Entity, EntityId, std::net::SocketAddr, u16)> = Vec::new();
    let mut to_abort: Vec<(Entity, EntityId)> = Vec::new();

    let mut query = world.query::<(Entity, &NetworkId, &TransferOutbound)>();
    for (entity, nid, outbound) in query.iter(&world) {
        if current_tick.saturating_sub(outbound.sent_tick) > TRANSFER_TIMEOUT_TICKS {
            if outbound.retry_count < TRANSFER_MAX_RETRIES {
                to_retry.push((entity, nid.0, outbound.target_addr, outbound.target_server));
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
        let kind = world.get::<EntityTypeComp>(entity).unwrap();
        let seq = world.get::<LastInputSeq>(entity);

        let entity_state = to_entity_state(
            nid,
            pos,
            vel,
            kind,
            &AuthorityServer(ServerId(target_server)),
            seq,
        );

        let msg = PeerMessage::AuthorityTransfer {
            tick: current_tick,
            session_id,
            entity_id: eid,
            from_server: server_id,
            to_server: ServerId(target_server),
            full_state: entity_state,
        };

        super::enqueue_outbound(
            &mut world.resource_mut::<OutboundTransfers>().messages,
            target_addr,
            &msg,
        );

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
                entity_id: eid,
                target_server: ServerId(target),
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
///
/// WI-203 (design 016, Batch 4): both NPC and Player entities are eligible.
/// NPCs are preferred (no client handshake required) but Players can migrate
/// when the load gap requires it or when only Players remain. Each Player
/// migration sends a `ServerMessage::Reconnect { url }` to the owning client
/// BEFORE the UDP `AuthorityTransfer` so the client has time to close the old
/// WebSocket and re-assign via the new URL. The source server flips the
/// client's `migrating_out` flag so its close handler suppresses
/// `RemoveEntity` (the target now owns the entity).
pub fn process_transfers(world: &mut World) {
    let transfer = world.resource_mut::<PendingTransfers>().transfers.take();
    let Some((_entity_ids, target_server)) = transfer else {
        return;
    };

    let server_id = world.resource::<ServerConfig>().server_id;
    let tick = world.resource::<TickCounter>().0;
    let session_id = world.resource::<SessionId>().0;

    // Look up target server's UDP address
    let target_addr = match world.resource::<PeerAddresses>().0.get(&target_server.0) {
        Some(addr) => *addr,
        None => {
            tracing::warn!("Rebalance target {target_server} not in peer list");
            return;
        }
    };

    // Collect all authoritative entities as candidates (NPC + Player).
    // Skip entities already in transfer (TransferOutbound).
    let mut candidates: Vec<(Entity, EntityId, EntityType, f32)> = Vec::new();
    let mut total_auth = 0usize;

    let mut query = world
        .query_filtered::<(Entity, &NetworkId, &Position, &EntityTypeComp), With<Authoritative>>();
    for (entity, nid, pos, kind) in query.iter(&world) {
        total_auth += 1;
        if world.get::<TransferOutbound>(entity).is_none() {
            candidates.push((entity, nid.0, kind.0, pos.0.length_squared()));
        }
    }

    // Sort: NPCs first, then Players (NPCs are cheaper to migrate — no client
    // handshake). Within each class, sort by descending distance so peripheral
    // entities move first (keeps players near origin on busy servers).
    candidates.sort_by(|a, b| {
        let a_is_player = matches!(a.2, EntityType::Player) as u8;
        let b_is_player = matches!(b.2, EntityType::Player) as u8;
        a_is_player
            .cmp(&b_is_player)
            .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
    });

    // Transfer up to 10% of our entities (at least one).
    let transfer_count = (total_auth / 10).max(1).min(candidates.len());

    let mut transferred = 0;
    for (entity, eid, kind, _) in candidates.iter().take(transfer_count) {
        // Player migrations require a Reconnect handshake first. If we can't
        // look up the target's WS URL or can't find the owning client, skip
        // the Player — rebalance falls through to the next candidate.
        if matches!(kind, EntityType::Player) {
            let target_url = match world.resource::<PeerWsAddresses>().0.get(&target_server.0) {
                Some(u) => u.clone(),
                None => {
                    tracing::warn!(
                        entity = %eid,
                        target = target_server.0,
                        "Skipping player migration: no ws url for target (PeerList may not have propagated)"
                    );
                    continue;
                }
            };

            let reconnect_json = match serde_json::to_string(&ServerMessage::Reconnect {
                url: target_url.clone(),
            }) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(
                        entity = %eid,
                        error = %e,
                        "Skipping player migration: failed to serialize Reconnect"
                    );
                    continue;
                }
            };

            // Find the client handle that owns this player. If there is no
            // client (e.g. the player entity was adopted via crash recovery
            // and no one's connected to it), skip — migration is meaningless
            // without someone to Reconnect.
            let mut found_client = false;
            {
                let mut registry = world.resource_mut::<ClientRegistry>();
                for (_cid, handle) in registry.clients.iter_mut() {
                    if handle.player_id == Some(*eid) {
                        // Enqueue Reconnect as a text frame BEFORE the UDP
                        // AuthorityTransfer (same-tick ordering guarantees
                        // the client receives this first).
                        match handle.tx.try_send(ClientFrame::Text(reconnect_json.clone())) {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::warn!(
                                    entity = %eid,
                                    error = ?e,
                                    "Failed to enqueue Reconnect for migrating client"
                                );
                                // Still proceed: the client channel may be full or
                                // the client already disconnected. The
                                // AuthorityTransfer below is still safe (target
                                // adopts the entity; the old client re-assigns
                                // via /assign). Mark migrating_out anyway so
                                // the (eventual) close handler suppresses
                                // RemoveEntity.
                            }
                        }
                        handle
                            .migrating_out
                            .store(true, std::sync::atomic::Ordering::Release);
                        found_client = true;
                        tracing::info!(
                            entity = %eid,
                            target = target_server.0,
                            target_url = %target_url,
                            "Sent Reconnect to migrating client"
                        );
                        break;
                    }
                }
            }

            if !found_client {
                tracing::warn!(
                    entity = %eid,
                    "Skipping player migration: no client owns this player entity"
                );
                continue;
            }
        }

        // Build EntityState from components
        let nid = world.get::<NetworkId>(*entity).unwrap();
        let pos = world.get::<Position>(*entity).unwrap();
        let vel = world.get::<Velocity>(*entity).unwrap();
        let kind_comp = world.get::<EntityTypeComp>(*entity).unwrap();
        let seq = world.get::<LastInputSeq>(*entity);

        let entity_state =
            to_entity_state(nid, pos, vel, kind_comp, &AuthorityServer(target_server), seq);

        let msg = PeerMessage::AuthorityTransfer {
            tick,
            session_id,
            entity_id: *eid,
            from_server: server_id,
            to_server: target_server,
            full_state: entity_state,
        };

        super::enqueue_outbound(
            &mut world.resource_mut::<OutboundTransfers>().messages,
            target_addr,
            &msg,
        );

        // Two-phase: mark entity as TransferOutbound instead of despawning.
        // Entity keeps Authoritative but is frozen (excluded from sim/replication via query filters).
        world.entity_mut(*entity).insert(TransferOutbound {
            target_server: target_server.0,
            target_addr,
            sent_tick: tick,
            retry_count: 0,
        });
        transferred += 1;
        // DO NOT despawn -- entity will be despawned when process_transfer_acks receives the ACK.
    }

    if transferred > 0 {
        let remaining = total_auth - transferred;
        tracing::info!(
            "Transferred {transferred} entities to {target_server} ({remaining} remaining)"
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
        world.init_resource::<PeerWsAddresses>();
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
    fn transfer_prefers_npcs_over_players_with_single_slot() {
        // WI-203: With 3 NPCs + 1 Player and a transfer budget of 1
        // ((total_auth/10).max(1) = 1), an NPC should migrate first
        // because the sort prefers NPCs over Players.
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());

        // 3 NPCs and 1 Player
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);
        spawn_auth_npc(&mut world, Vec3::Z, 1);
        let player_state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let player_entity = spawn_from_entity_state_direct(&mut world, &player_state, true, None);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // Player should NOT have TransferOutbound: NPCs are taken first.
        assert!(
            world.get::<TransferOutbound>(player_entity).is_none(),
            "Player must not migrate when an NPC candidate is available in the single slot"
        );
    }

    #[test]
    fn transfer_produces_outbound_messages() {
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
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

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // No transfers should have happened
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
        // No TransferOutbound components
        let outbound = world
            .query_filtered::<(), With<TransferOutbound>>()
            .iter(&world)
            .count();
        assert_eq!(outbound, 0);
    }

    // -- process_transfers: peripheral entities transferred first --

    #[test]
    fn transfer_peripheral_entities_first() {
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());

        // 10 entities: inner at (1,0,1) to (10,0,10), one outer at (4000,0,4000)
        for i in 1..=9 {
            spawn_auth_npc(&mut world, Vec3::new(i as f32, 0.0, i as f32), 1);
        }
        let (outer_entity, _) = spawn_auth_npc(&mut world, Vec3::new(4000.0, 0.0, 4000.0), 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // The outer entity should be transferred first
        assert!(
            world.get::<TransferOutbound>(outer_entity).is_some(),
            "peripheral entity should be transferred first"
        );
    }

    // -- EntityLookup consistency after transfer out --

    #[test]
    fn entity_lookup_consistent_after_transfer() {
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());

        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // Entity should still be in lookup (not despawned yet -- waiting for ACK)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));

        // Now process the ACK
        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 6,
            });
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

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 5,
            });

        process_transfer_acks(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn process_transfer_acks_ignores_unknown_entity() {
        let mut world = setup_world(1);

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: EntityId::new(),
                tick: 5,
            });

        process_transfer_acks(&mut world);
    }

    #[test]
    fn process_transfer_acks_ignores_entity_without_outbound() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, true, None);

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 5,
            });

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
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            0
        );
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

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 5,
            });
        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 6,
            });

        process_transfer_acks(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn process_transfers_skips_entities_with_transfer_outbound() {
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());

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

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        let new_outbound_count = entities
            .iter()
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
            world
                .resource_mut::<InboundQueue>()
                .transfer_acks
                .push(InboundTransferAck {
                    entity_id: *eid,
                    tick: 5,
                });
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
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            0
        );
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
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            1
        );
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

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eid,
                tick: 5,
            });

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

        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eids[0],
                tick: 5,
            });
        world
            .resource_mut::<InboundQueue>()
            .transfer_acks
            .push(InboundTransferAck {
                entity_id: eids[1],
                tick: 5,
            });

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
            PeerMessage::AuthorityTransfer {
                full_state,
                entity_id,
                tick,
                ..
            } => {
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
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            1
        );
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Retry 2
        world.resource_mut::<TickCounter>().0 = 22;
        process_transfer_timeouts(&mut world);
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            2
        );
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Retry 3
        world.resource_mut::<TickCounter>().0 = 33;
        process_transfer_timeouts(&mut world);
        assert_eq!(
            world.get::<TransferOutbound>(entity).unwrap().retry_count,
            3
        );
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

        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 10;

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        process_transfers(&mut world);

        let first_outbound = world
            .query_filtered::<(), With<TransferOutbound>>()
            .iter(&world)
            .count();
        assert!(first_outbound > 0);

        world.resource_mut::<OutboundTransfers>().messages.clear();

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        process_transfers(&mut world);

        let total = world
            .query_filtered::<(), With<Authoritative>>()
            .iter(&world)
            .count();
        assert_eq!(total, 5);

        let mut query = world.query::<&TransferOutbound>();
        for outbound in query.iter(&world) {
            assert!(outbound.sent_tick >= 10);
        }
    }

    // -- WI-132: Real SessionId flows into PeerMessage producers --

    #[test]
    fn process_transfers_stamps_session_id_from_resource() {
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Override the SessionId the setup_world helper inserted as 0.
        world.insert_resource(SessionId(0xF00D_CAFE_1234_5678));
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        let messages = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(messages.len(), 1);
        let msg: PeerMessage = deserialize_versioned(&messages[0].1);
        match msg {
            PeerMessage::AuthorityTransfer { session_id, .. } => {
                assert_eq!(session_id, 0xF00D_CAFE_1234_5678);
            }
            _ => panic!("expected AuthorityTransfer, got {:?}", msg),
        }
    }

    #[test]
    fn process_transfer_timeouts_stamps_session_id_on_retry() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        world.insert_resource(SessionId(0xAA_BB_CC_DD));
        world.resource_mut::<TickCounter>().0 = 11; // past TRANSFER_TIMEOUT_TICKS

        process_transfer_timeouts(&mut world);

        let messages = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(messages.len(), 1);
        let msg: PeerMessage = deserialize_versioned(&messages[0].1);
        match msg {
            PeerMessage::AuthorityTransfer { session_id, .. } => {
                assert_eq!(session_id, 0xAA_BB_CC_DD);
            }
            _ => panic!("expected AuthorityTransfer, got {:?}", msg),
        }
    }

    // =========================================================================
    // WI-203: Source-side player migration (process_transfers includes Players)
    // =========================================================================

    /// Register a test client owning the given player_id. Returns the frame
    /// receiver (so the test can inspect Reconnect frames) and the shared
    /// migrating_out flag (so the test can assert its state post-process).
    fn register_test_client_for_player(
        world: &mut World,
        client_id: u64,
        player_id: EntityId,
    ) -> (
        tokio::sync::mpsc::Receiver<crate::resources::ClientFrame>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::Arc;
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::resources::ClientFrame>(8);
        let migrating = Arc::new(AtomicBool::new(false));
        let acked = Arc::new(AtomicU64::new(0));
        world
            .resource_mut::<ClientRegistry>()
            .register_from_event_with_migrating(
                client_id,
                tx,
                acked,
                Arc::clone(&migrating),
            );
        if let Some(handle) = world
            .resource_mut::<ClientRegistry>()
            .clients
            .get_mut(&client_id)
        {
            handle.player_id = Some(player_id);
        }
        (rx, migrating)
    }

    fn spawn_auth_player(world: &mut World, pos: Vec3, server_id: u16) -> (Entity, EntityId) {
        let state = EntityState::new_player(pos, ServerId(server_id));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, true, None);
        (entity, eid)
    }

    #[test]
    fn process_transfers_player_skipped_when_no_ws_url() {
        // WI-203: if PeerWsAddresses has no url for the target, skip the
        // player — Reconnect can't be constructed. NPCs still migrate as
        // usual via the same directive.
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        // Intentionally NOT inserting into PeerWsAddresses.

        let (player_entity, pid) = spawn_auth_player(&mut world, Vec3::new(50.0, 0.0, 50.0), 1);
        let (_rx, _migrating) = register_test_client_for_player(&mut world, 42, pid);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;

        process_transfers(&mut world);

        // Player must NOT have been migrated — the candidate was skipped.
        assert!(
            world.get::<TransferOutbound>(player_entity).is_none(),
            "player must not migrate when no ws_url is available for target"
        );
    }

    #[test]
    fn process_transfers_player_sends_reconnect_and_flips_migrating() {
        // WI-203: happy path. A player is the only candidate, target has a
        // ws url, client is registered. Expect:
        //   (a) ClientFrame::Text(Reconnect JSON) enqueued on the client's tx
        //   (b) migrating_out atomic flipped to true
        //   (c) TransferOutbound component set on the entity
        //   (d) AuthorityTransfer UDP message queued
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerWsAddresses>()
            .0
            .insert(2, "ws://127.0.0.1:7879".to_string());

        let (player_entity, pid) = spawn_auth_player(&mut world, Vec3::new(50.0, 0.0, 50.0), 1);
        let (mut rx, migrating) = register_test_client_for_player(&mut world, 42, pid);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        // (a) Reconnect text frame enqueued.
        let frame = rx.try_recv().expect("client should have received Reconnect");
        match frame {
            crate::resources::ClientFrame::Text(json) => {
                assert!(
                    json.contains("\"type\":\"Reconnect\""),
                    "expected Reconnect json, got: {json}"
                );
                assert!(
                    json.contains("ws://127.0.0.1:7879"),
                    "expected target ws url in payload, got: {json}"
                );
            }
            other => panic!("expected ClientFrame::Text, got {other:?}"),
        }

        // (b) migrating_out flipped.
        assert!(
            migrating.load(std::sync::atomic::Ordering::Acquire),
            "migrating_out should be true after process_transfers"
        );

        // (c) TransferOutbound set.
        assert!(
            world.get::<TransferOutbound>(player_entity).is_some(),
            "player entity should be marked TransferOutbound"
        );

        // (d) AuthorityTransfer enqueued.
        let outbound = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(outbound.len(), 1, "one AuthorityTransfer should be queued");
        let (_, bytes) = &outbound[0];
        let msg = deserialize_versioned(bytes);
        match msg {
            PeerMessage::AuthorityTransfer {
                entity_id,
                to_server,
                ..
            } => {
                assert_eq!(entity_id, pid);
                assert_eq!(to_server, ServerId(2));
            }
            other => panic!("expected AuthorityTransfer, got {other:?}"),
        }
    }

    #[test]
    fn process_transfers_player_uses_wss_scheme_from_tls_peer() {
        // WI-203: the Reconnect URL should use whatever scheme the peer
        // advertised via PeerInfo.tls. This test seeds a wss:// url to
        // confirm it propagates through.
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerWsAddresses>()
            .0
            .insert(2, "wss://192.168.0.173:7879".to_string());

        let (_, pid) = spawn_auth_player(&mut world, Vec3::new(50.0, 0.0, 50.0), 1);
        let (mut rx, _migrating) = register_test_client_for_player(&mut world, 42, pid);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 10;
        process_transfers(&mut world);

        let frame = rx.try_recv().unwrap();
        match frame {
            crate::resources::ClientFrame::Text(json) => {
                assert!(
                    json.contains("wss://192.168.0.173:7879"),
                    "expected wss:// scheme in reconnect url, got: {json}"
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn process_transfers_player_with_no_client_is_skipped() {
        // WI-203: A player entity exists but no client owns it (e.g. the
        // entity was adopted via crash recovery, or the browser tab died
        // without removing the entity). Migration is meaningless without
        // someone to Reconnect — skip.
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerWsAddresses>()
            .0
            .insert(2, "ws://127.0.0.1:7879".to_string());
        let (player_entity, _pid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);
        // No client registered.

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;
        process_transfers(&mut world);

        assert!(
            world.get::<TransferOutbound>(player_entity).is_none(),
            "player with no client must not be migrated"
        );
    }

    #[test]
    fn process_transfers_prefers_npcs_over_players() {
        // WI-203: sort order. With enough NPCs to fill the transfer
        // budget, players remain untouched. The sort puts NPCs (is_player=0)
        // before Players (is_player=1) regardless of distance.
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerWsAddresses>()
            .0
            .insert(2, "ws://127.0.0.1:7879".to_string());

        // 20 NPCs so transfer_count = (21/10).max(1) = 2.
        for i in 0..20 {
            spawn_auth_npc(
                &mut world,
                Vec3::new(i as f32 * 100.0, 0.0, 0.0), // spread
                1,
            );
        }
        // 1 Player in the middle.
        let (player_entity, pid) = spawn_auth_player(&mut world, Vec3::new(500.0, 0.0, 500.0), 1);
        let (_rx, migrating) = register_test_client_for_player(&mut world, 42, pid);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;
        process_transfers(&mut world);

        // Player must NOT be picked when NPCs fill the budget.
        assert!(
            world.get::<TransferOutbound>(player_entity).is_none(),
            "player must not migrate when NPCs fill the budget"
        );
        assert!(
            !migrating.load(std::sync::atomic::Ordering::Acquire),
            "migrating_out must stay false if the player didn't migrate"
        );

        // Two NPCs should have migrated (the budget).
        let npc_out = world
            .query_filtered::<(), (With<TransferOutbound>, With<Authoritative>)>()
            .iter(&world)
            .count();
        assert_eq!(npc_out, 2, "two NPCs should have been picked");
    }

    #[test]
    fn process_transfers_falls_back_to_player_when_no_npcs() {
        // WI-203: if the only candidate is a Player, the migration system
        // must migrate it (this is the whole point of 080).
        let mut world = setup_world(1);
        world
            .resource_mut::<PeerAddresses>()
            .0
            .insert(2, "127.0.0.1:9102".parse().unwrap());
        world
            .resource_mut::<PeerWsAddresses>()
            .0
            .insert(2, "ws://127.0.0.1:7879".to_string());

        let (player_entity, pid) = spawn_auth_player(&mut world, Vec3::new(100.0, 0.0, 100.0), 1);
        let (mut rx, migrating) = register_test_client_for_player(&mut world, 42, pid);

        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], ServerId(2)));
        world.resource_mut::<TickCounter>().0 = 5;
        process_transfers(&mut world);

        assert!(
            world.get::<TransferOutbound>(player_entity).is_some(),
            "player must migrate when it's the only candidate"
        );
        assert!(
            migrating.load(std::sync::atomic::Ordering::Acquire),
            "migrating_out must be flipped"
        );
        let frame = rx.try_recv().expect("client must receive Reconnect");
        matches!(frame, crate::resources::ClientFrame::Text(_));
    }
}
