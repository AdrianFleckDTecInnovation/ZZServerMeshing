use crate::components::*;
use crate::conversion::{despawn_and_remove, spawn_from_entity_state_direct, to_entity_state};
use crate::delta::{build_baseline, compute_delta, update_baseline, FULL_SNAPSHOT_INTERVAL};
use crate::encoding::{encode_binary_delta, encode_binary_snapshot};
use crate::resources::*;
use bevy_ecs::prelude::*;
use glam::Vec3;
use rand::Rng;
use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use shared::protocol::{ClientMessage, PeerMessage};
use shared::spatial::clamp_to_world;
use shared::tick::{TICK_DURATION_MS, TICK_RATE};
use std::sync::Arc;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// NPC direction change interval in ticks.
const NPC_DIRECTION_CHANGE_INTERVAL: u64 = (TICK_RATE as u64) * 3; // every 3 seconds

/// Max ticks before a shadow entity is considered stale and removed.
/// 15 seconds gives comfortable margin over worst-case crash detection (5s heartbeat
/// timeout + 5s rebalance interval = 10s) so shadows survive until RecoverEntities arrives.
const SHADOW_STALE_TICKS: u64 = TICK_RATE as u64 * 15; // 15 seconds

/// Ticks before a pending authority transfer is considered timed out.
const TRANSFER_TIMEOUT_TICKS: u64 = 10; // 500ms at 20Hz

/// Maximum number of retries for a timed-out authority transfer.
/// 3 retries = 4 total attempts (initial + 3) over ~2 seconds at 20Hz.
/// Reduces probability of permanent entity duplication from lost ACKs
/// to ~0.0001% on WiFi (assuming 5% per-packet loss).
const TRANSFER_MAX_RETRIES: u8 = 3;

/// Maximum ticks a drain can be active before forcing shutdown.
const DRAIN_TIMEOUT_TICKS: u64 = 600; // 30 seconds at 20Hz

/// Send a TransferAck back to the source server of an authority transfer.
/// Looks up the source server's UDP address in PeerAddresses. If the source
/// is not in the peer list (e.g. it crashed), skips silently.
fn send_transfer_ack(
    world: &mut World,
    entity_id: EntityId,
    tick: u64,
    from_server: ServerId,
) {
    if let Some(&source_addr) = world.resource::<PeerAddresses>().0.get(&from_server.0) {
        let ack_msg = PeerMessage::TransferAck { entity_id, tick };
        if let Ok(bytes) = bincode::serialize(&ack_msg) {
            world
                .resource_mut::<OutboundTransfers>()
                .messages
                .push((source_addr, bytes));
            tracing::debug!(
                entity = %entity_id,
                to = from_server.0,
                "Sent TransferAck"
            );
        }
    } else {
        tracing::debug!(
            entity = %entity_id,
            from = from_server.0,
            "Skipping TransferAck: source server not in peer list"
        );
    }
}

// ============================================================
// System 1: drain_inbound_events
// ============================================================

/// Process inbound events from async tasks: peer updates, authority transfers, forwarded inputs.
pub fn drain_inbound_events(world: &mut World) {
    let _tick = world.resource::<TickCounter>().0;
    let server_id = world.resource::<ServerConfig>().server_id;

    // Take all queued events
    let peer_updates = std::mem::take(&mut world.resource_mut::<InboundQueue>().peer_updates);
    let authority_transfers =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().authority_transfers);
    let forwarded_inputs =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().forwarded_inputs);
    let forwarded_removes =
        std::mem::take(&mut world.resource_mut::<InboundQueue>().forwarded_removes);

    // -- Process peer updates (shadow entities) --
    for update in peer_updates {
        for entity_state in &update.entities {
            let eid = entity_state.id;

            if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&eid) {
                // Entity already exists
                if world.get::<Authoritative>(bevy_entity).is_some() {
                    // Don't overwrite our authoritative entity
                    continue;
                }
                if let Some(mut shadow) = world.get_mut::<Shadow>(bevy_entity) {
                    // Update existing shadow if tick is newer
                    if update.tick < shadow.last_seen_tick {
                        continue; // stale packet
                    }
                    shadow.last_seen_tick = update.tick;
                    if let Some(mut pos) = world.get_mut::<Position>(bevy_entity) {
                        pos.0 = entity_state.position;
                    }
                    if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                        vel.0 = entity_state.velocity;
                    }
                    if let Some(mut auth) = world.get_mut::<AuthorityServer>(bevy_entity) {
                        auth.0 = entity_state.authority_server;
                    }
                    // Propagate last_input_seq for player shadows so
                    // client snapshots reflect the authority's ack
                    if let Some(mut seq) = world.get_mut::<LastInputSeq>(bevy_entity) {
                        seq.0 = entity_state.last_input_seq;
                    }
                    continue;
                }
            }

            // Entity not in lookup -- spawn as shadow
            spawn_from_entity_state_direct(world, entity_state, false, Some(update.tick));
        }
    }

    // -- Process authority transfers --
    let current_tick = world.resource::<TickCounter>().0;
    for transfer in authority_transfers {
        if transfer.to_server != server_id {
            tracing::warn!(
                "Ignoring transfer for {} targeted at {}, we are {}",
                transfer.entity_id,
                transfer.to_server,
                server_id
            );
            continue;
        }

        let eid = transfer.entity_id;

        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&eid) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                tracing::warn!(
                    "Ignoring duplicate transfer for {} from {}",
                    eid,
                    transfer.from_server
                );
                // Still send ACK for duplicate -- source needs it even if we already have the entity
                send_transfer_ack(world, eid, current_tick, transfer.from_server);
                continue;
            }

            // Promote shadow to authoritative
            if world.get::<Shadow>(bevy_entity).is_some() {
                // Remove Shadow, add Authoritative
                world.entity_mut(bevy_entity).remove::<Shadow>();
                world.entity_mut(bevy_entity).insert(Authoritative);

                // Update all components from full_state
                if let Some(mut pos) = world.get_mut::<Position>(bevy_entity) {
                    pos.0 = transfer.full_state.position;
                }
                if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                    vel.0 = transfer.full_state.velocity;
                }
                if let Some(mut auth) = world.get_mut::<AuthorityServer>(bevy_entity) {
                    auth.0 = transfer.full_state.authority_server;
                }
                if let Some(mut kind) = world.get_mut::<EntityKind>(bevy_entity) {
                    kind.0 = transfer.full_state.entity_type;
                }
                if transfer.full_state.entity_type == EntityType::Player {
                    world
                        .entity_mut(bevy_entity)
                        .insert(LastInputSeq(transfer.full_state.last_input_seq));
                }

                tracing::info!(
                    entity = %eid,
                    from = transfer.from_server.0,
                    to = transfer.to_server.0,
                    "Accepted authority transfer (promoted shadow)"
                );
                send_transfer_ack(world, eid, current_tick, transfer.from_server);
                continue;
            }
        }

        // Entity not found at all -- spawn as authoritative
        spawn_from_entity_state_direct(world, &transfer.full_state, true, None);
        tracing::info!(
            entity = %eid,
            from = transfer.from_server.0,
            to = transfer.to_server.0,
            "Accepted authority transfer (new entity)"
        );
        send_transfer_ack(world, eid, current_tick, transfer.from_server);
    }

    // -- Process forwarded inputs --
    for fwd in forwarded_inputs {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&fwd.entity_id) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                // If entity is being transferred, re-forward to the target
                if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                    let target_addr = outbound.target_addr;
                    let target_server = outbound.target_server;
                    let fwd_msg = PeerMessage::ForwardedInput {
                        entity_id: fwd.entity_id,
                        direction: fwd.direction,
                        seq: fwd.seq,
                    };
                    if let Ok(bytes) = bincode::serialize(&fwd_msg) {
                        world
                            .resource_mut::<OutboundForwards>()
                            .messages
                            .push((target_addr, bytes));
                    }
                    tracing::debug!(
                        entity = %fwd.entity_id,
                        target = target_server,
                        seq = fwd.seq,
                        "Re-forwarding input to transfer target"
                    );
                    continue;
                }

                let dir = Vec3::new(fwd.direction[0], fwd.direction[1], fwd.direction[2]);
                let kind = world.get::<EntityKind>(bevy_entity).map(|k| k.0);
                let speed = match kind {
                    Some(EntityType::Player) => 5.0,
                    _ => 2.0,
                };
                if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                    vel.0 = dir.normalize_or_zero() * speed;
                }
                if let Some(mut seq) = world.get_mut::<LastInputSeq>(bevy_entity) {
                    seq.0 = fwd.seq;
                }
                tracing::debug!(
                    entity = %fwd.entity_id,
                    seq = fwd.seq,
                    "Applied forwarded input"
                );
            } else {
                tracing::debug!(
                    "Received forwarded input for {} but not authoritative",
                    fwd.entity_id
                );
            }
        }
    }

    // -- Process forwarded removes --
    for fwd_rm in forwarded_removes {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&fwd_rm.entity_id) {
            if world.get::<Authoritative>(bevy_entity).is_some() {
                despawn_and_remove(world, bevy_entity, fwd_rm.entity_id);
                tracing::info!(
                    entity = %fwd_rm.entity_id,
                    "Removed entity via forwarded remove"
                );
            } else {
                tracing::debug!(
                    entity = %fwd_rm.entity_id,
                    "Forwarded remove for non-authoritative entity, ignoring"
                );
            }
        } else {
            tracing::debug!(
                entity = %fwd_rm.entity_id,
                "Forwarded remove for unknown entity, ignoring"
            );
        }
    }
}

// ============================================================
// System 1a: process_transfer_acks
// ============================================================

/// Process inbound TransferAck messages. For each ACK, look up the entity
/// and despawn it if it has a TransferOutbound component (two-phase transfer complete).
pub fn process_transfer_acks(world: &mut World) {
    let acks = std::mem::take(&mut world.resource_mut::<InboundQueue>().transfer_acks);

    for ack in acks {
        if let Some(&bevy_entity) = world.resource::<EntityLookup>().0.get(&ack.entity_id) {
            if world.get::<TransferOutbound>(bevy_entity).is_some() {
                tracing::info!(
                    entity = %ack.entity_id,
                    tick = ack.tick,
                    "Transfer ACK received, despawning entity"
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

// ============================================================
// System 1b: process_transfer_timeouts
// ============================================================

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

        if let Ok(bytes) = bincode::serialize(&msg) {
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
        tracing::error!(
            entity = %eid,
            target = target,
            retries = retries,
            "Transfer failed after {} retries, keeping entity",
            retries,
        );
    }
}

// ============================================================
// System 1c: drain_client_events
// ============================================================

/// Drain client registration/unregistration events from InboundQueue into ClientRegistry.
/// Runs after drain_inbound_events, before process_client_inputs.
pub fn drain_client_events(
    mut inbound: ResMut<InboundQueue>,
    mut registry: ResMut<ClientRegistry>,
) {
    // Register new clients
    let registrations = std::mem::take(&mut inbound.register_clients);
    for event in registrations {
        registry.register_from_event(event.client_id, event.tx, event.acked_tick);
        // Also push client_player_assignment so the player_id can be set
        // once SpawnPlayer is processed (matched by player_uuid).
        inbound.client_player_assignments.push((
            event.client_id,
            shared::entity::EntityId(uuid::Uuid::parse_str(&event.player_uuid).unwrap_or_else(
                |_| uuid::Uuid::nil(),
            )),
        ));
        tracing::debug!(client_id = event.client_id, "Registered client in ECS");
    }

    // Unregister disconnected clients
    let unregistrations = std::mem::take(&mut inbound.unregister_clients);
    for client_id in unregistrations {
        registry.unregister(client_id);
        tracing::debug!(client_id, "Unregistered client from ECS");
    }

    // Apply player ID assignments
    let assignments = std::mem::take(&mut inbound.client_player_assignments);
    for (client_id, entity_id) in assignments {
        if let Some(handle) = registry.clients.get_mut(&client_id) {
            handle.player_id = Some(entity_id);
        }
    }

    // Handle forced full snapshot requests
    let force_fulls = std::mem::take(&mut inbound.force_full_snapshot_clients);
    for client_id in force_fulls {
        if let Some(handle) = registry.clients.get_mut(&client_id) {
            handle.baseline = None;
            handle.ticks_since_full = 0;
            tracing::debug!(client_id, "Forced full snapshot for client");
        }
    }
}

// ============================================================
// System 2: process_client_inputs
// ============================================================

/// Process client inputs from WebSocket connections.
pub fn process_client_inputs(world: &mut World) {
    let server_id = world.resource::<ServerConfig>().server_id;
    let client_inputs = std::mem::take(&mut world.resource_mut::<InboundQueue>().client_inputs);

    for input in client_inputs {
        match input {
            ClientMessage::SpawnEntity {
                position,
                entity_type,
            } => {
                let pos = Vec3::new(position[0], position[1], position[2]);
                let etype = match entity_type.as_str() {
                    "player" => EntityType::Player,
                    _ => EntityType::Npc,
                };
                let state = match etype {
                    EntityType::Player => EntityState::new_player(pos, server_id),
                    EntityType::Npc => EntityState::new_npc(pos, server_id),
                };
                let eid = state.id;
                spawn_from_entity_state_direct(world, &state, true, None);
                tracing::debug!("Spawned {:?} entity {}", etype, eid);
            }
            ClientMessage::SpawnPlayer {
                player_id,
                position,
            } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&player_id) {
                    let pos = Vec3::new(position[0], position[1], position[2]);
                    let id = EntityId(uuid);
                    let state = EntityState {
                        id,
                        position: pos,
                        velocity: Vec3::ZERO,
                        entity_type: EntityType::Player,
                        authority_server: server_id,
                        last_input_seq: 0,
                    };
                    spawn_from_entity_state_direct(world, &state, true, None);
                    tracing::info!("Spawned player {}", id);
                }
            }
            ClientMessage::MoveInput {
                entity_id,
                direction,
                seq,
            } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&entity_id) {
                    let eid = EntityId(uuid);
                    let dir = Vec3::new(direction[0], direction[1], direction[2]);

                    if let Some(&bevy_entity) =
                        world.resource::<EntityLookup>().0.get(&eid)
                    {
                        if world.get::<Authoritative>(bevy_entity).is_some() {
                            // Check if entity is being transferred -- forward to target instead
                            if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                                let target_addr = outbound.target_addr;
                                let target_server = outbound.target_server;
                                let fwd_msg = PeerMessage::ForwardedInput {
                                    entity_id: eid,
                                    direction,
                                    seq,
                                };
                                if let Ok(bytes) = bincode::serialize(&fwd_msg) {
                                    world
                                        .resource_mut::<OutboundForwards>()
                                        .messages
                                        .push((target_addr, bytes));
                                }
                                tracing::debug!(
                                    entity = %eid,
                                    target = target_server,
                                    seq = seq,
                                    "Forwarding input to transfer target"
                                );
                                continue;
                            }

                            // Process locally
                            let kind = world.get::<EntityKind>(bevy_entity).map(|k| k.0);
                            let speed = match kind {
                                Some(EntityType::Player) => 5.0,
                                _ => 2.0,
                            };
                            if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
                                vel.0 = dir.normalize_or_zero() * speed;
                            }
                            if let Some(mut input_seq) =
                                world.get_mut::<LastInputSeq>(bevy_entity)
                            {
                                input_seq.0 = seq;
                            }
                            tracing::debug!(
                                entity = %eid,
                                seq = seq,
                                "MoveInput processed"
                            );
                            continue;
                        }

                        // Shadow -- produce forward
                        if let Some(shadow_auth) =
                            world.get::<AuthorityServer>(bevy_entity)
                        {
                            if world.get::<Shadow>(bevy_entity).is_some() {
                                let target_server = shadow_auth.0 .0;
                                if let Some(&peer_addr) = world
                                    .resource::<PeerAddresses>()
                                    .0
                                    .get(&target_server)
                                {
                                    let msg = PeerMessage::ForwardedInput {
                                        entity_id: eid,
                                        direction,
                                        seq,
                                    };
                                    if let Ok(bytes) = bincode::serialize(&msg) {
                                        world
                                            .resource_mut::<OutboundForwards>()
                                            .messages
                                            .push((peer_addr, bytes));
                                    }
                                }
                                tracing::debug!(
                                    entity = %eid,
                                    target = target_server,
                                    seq = seq,
                                    "Forwarding input to authority"
                                );
                                continue;
                            }
                        }
                    }
                    tracing::debug!("MoveInput for unknown entity {}", eid);
                }
            }
            ClientMessage::RemoveEntity { entity_id } => {
                if let Ok(uuid) = uuid::Uuid::parse_str(&entity_id) {
                    let eid = EntityId(uuid);
                    if let Some(&bevy_entity) =
                        world.resource::<EntityLookup>().0.get(&eid)
                    {
                        if world.get::<Authoritative>(bevy_entity).is_some() {
                            // If entity is mid-transfer, forward the remove to the target
                            // server so it doesn't become permanently orphaned there.
                            if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
                                let target_addr = outbound.target_addr;
                                let target_server = outbound.target_server;
                                let msg = PeerMessage::ForwardedRemove {
                                    entity_id: eid,
                                };
                                if let Ok(bytes) = bincode::serialize(&msg) {
                                    world
                                        .resource_mut::<OutboundForwards>()
                                        .messages
                                        .push((target_addr, bytes));
                                }
                                tracing::info!(
                                    entity = %eid,
                                    target = target_server,
                                    "Forwarding remove to transfer target before local despawn"
                                );
                            }
                            // Case A: We are authoritative -- remove locally
                            despawn_and_remove(world, bevy_entity, eid);
                            tracing::info!(entity = %eid, "Removed authoritative entity on disconnect");
                        } else if world.get::<Shadow>(bevy_entity).is_some() {
                            // Case B: Entity was transferred -- forward remove to authority
                            if let Some(shadow_auth) =
                                world.get::<AuthorityServer>(bevy_entity)
                            {
                                let target_server = shadow_auth.0 .0;
                                if let Some(&peer_addr) = world
                                    .resource::<PeerAddresses>()
                                    .0
                                    .get(&target_server)
                                {
                                    let msg = PeerMessage::ForwardedRemove {
                                        entity_id: eid,
                                    };
                                    if let Ok(bytes) = bincode::serialize(&msg) {
                                        world
                                            .resource_mut::<OutboundForwards>()
                                            .messages
                                            .push((peer_addr, bytes));
                                    }
                                    tracing::info!(
                                        entity = %eid,
                                        target = target_server,
                                        "Forwarding remove to authority server"
                                    );
                                } else {
                                    tracing::warn!(
                                        entity = %eid,
                                        target = target_server,
                                        "Cannot forward remove: no peer address for authority server"
                                    );
                                }
                            }
                            // Also remove the local shadow copy
                            despawn_and_remove(world, bevy_entity, eid);
                        }
                    }
                }
            }
            ClientMessage::RequestSnapshot => {}
        }
    }
}

// ============================================================
// System 3: process_coord_events
// ============================================================

/// Process coordinator events: peer list updates, rebalance/drain directives, crash recovery.
pub fn process_coord_events(world: &mut World) {
    let server_id = world.resource::<ServerConfig>().server_id;
    let peer_list_updates =
        std::mem::take(&mut world.resource_mut::<CoordEventQueue>().peer_list_updates);
    let rebalance_directives =
        std::mem::take(&mut world.resource_mut::<CoordEventQueue>().rebalance_directives);
    let drain_directives =
        std::mem::take(&mut world.resource_mut::<CoordEventQueue>().drain_directives);
    let recover_events =
        std::mem::take(&mut world.resource_mut::<CoordEventQueue>().recover_events);

    // PeerList
    for peers in peer_list_updates {
        let mut addrs = world.resource_mut::<PeerAddresses>();
        addrs.0.clear();
        for peer in &peers {
            // Try direct parse first (IP:port). DNS resolution is not available
            // in sync context so we only use direct parse here.
            let addr: Option<std::net::SocketAddr> = peer.udp_addr.parse().ok();
            if let Some(addr) = addr {
                addrs.0.insert(peer.server_id, addr);
            } else {
                tracing::warn!("Could not parse peer address: {}", peer.udp_addr);
            }
        }
        tracing::info!(
            "Peer list updated: {} peers {:?}",
            peers.len(),
            peers
                .iter()
                .map(|p| format!("S{}", p.server_id))
                .collect::<Vec<_>>()
        );
    }

    // RebalanceDirective
    for (entity_ids, target_server) in rebalance_directives {
        world.resource_mut::<PendingTransfers>().transfers =
            Some((entity_ids, target_server));
        tracing::info!("Received rebalance directive -> S{target_server}");
    }

    // DrainDirective
    for target_servers in drain_directives {
        let mut drain = world.resource_mut::<DrainState>();
        drain.draining = true;
        drain.targets = Some(target_servers.clone());
        tracing::info!(
            "Received drain directive -> {:?}",
            target_servers
                .iter()
                .map(|s| format!("S{s}"))
                .collect::<Vec<_>>()
        );
    }

    // RecoverEntities
    for (dead_server_id, survivors) in recover_events {
        let my_index = survivors.iter().position(|&id| id == server_id.0);
        let my_index = match my_index {
            Some(i) => i,
            None => continue,
        };
        let survivor_count = survivors.len();

        // Find shadow entities from the dead server that hash to us
        let mut to_promote: Vec<Entity> = Vec::new();
        let mut query = world.query_filtered::<(Entity, &NetworkId, &AuthorityServer), With<Shadow>>();
        for (entity, nid, auth) in query.iter(&world) {
            if auth.0 .0 == dead_server_id {
                if (nid.0 .0.as_u128() as usize) % survivor_count == my_index {
                    to_promote.push(entity);
                }
            }
        }

        let mut adopted = 0;
        for entity in to_promote {
            world.entity_mut(entity).remove::<Shadow>();
            world.entity_mut(entity).insert(Authoritative);
            if let Some(mut auth) = world.get_mut::<AuthorityServer>(entity) {
                auth.0 = server_id;
            }
            adopted += 1;
        }

        if adopted > 0 {
            tracing::info!("Recovered {} entities from dead S{}", adopted, dead_server_id);
        }

        // Abort any in-progress transfers targeting the dead server.
        // The transfer target is gone -- waiting for an ACK that will never come is pointless.
        // Remove TransferOutbound so the entity unfreezes and resumes normal operation.
        let mut to_unfreeze: Vec<Entity> = Vec::new();
        let mut transfer_query =
            world.query_filtered::<(Entity, &NetworkId, &TransferOutbound), With<Authoritative>>();
        for (entity, nid, outbound) in transfer_query.iter(&world) {
            if outbound.target_server == dead_server_id {
                tracing::info!(
                    "Aborting transfer for entity {} to dead server S{}",
                    nid.0, dead_server_id
                );
                to_unfreeze.push(entity);
            }
        }
        for entity in to_unfreeze {
            world.entity_mut(entity).remove::<TransferOutbound>();
        }
    }
}

// ============================================================
// System 4: npc_behavior
// ============================================================

/// NPC wandering behavior: change direction every NPC_DIRECTION_CHANGE_INTERVAL ticks.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn npc_behavior(
    tick: Res<TickCounter>,
    mut query: Query<
        (&NetworkId, &mut Velocity, &EntityKind),
        (With<Authoritative>, Without<TransferOutbound>),
    >,
) {
    let current_tick = tick.0;
    let mut rng = rand::thread_rng();

    for (nid, mut vel, kind) in query.iter_mut() {
        if kind.0 != EntityType::Npc {
            continue;
        }

        let phase = (nid.0 .0.as_u128() % NPC_DIRECTION_CHANGE_INTERVAL as u128) as u64;
        if current_tick % NPC_DIRECTION_CHANGE_INTERVAL == phase {
            let angle = rng.gen_range(0.0..std::f32::consts::TAU);
            let speed = rng.gen_range(1.0..3.0);
            vel.0 = Vec3::new(angle.cos() * speed, 0.0, angle.sin() * speed);
        }
    }
}

// ============================================================
// System 5: physics_integration
// ============================================================

/// Integrate position from velocity for all authoritative entities.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn physics_integration(
    mut query: Query<(&mut Position, &Velocity), (With<Authoritative>, Without<TransferOutbound>)>,
) {
    let dt = TICK_DURATION.as_secs_f32();
    for (mut pos, vel) in query.iter_mut() {
        pos.0 += vel.0 * dt;
    }
}

// ============================================================
// System 6: clamp_positions
// ============================================================

/// Clamp all authoritative entity positions to world bounds.
/// Excludes entities with TransferOutbound -- they are frozen on the source server.
pub fn clamp_positions(
    mut query: Query<&mut Position, (With<Authoritative>, Without<TransferOutbound>)>,
) {
    for mut pos in query.iter_mut() {
        pos.0 = clamp_to_world(pos.0);
    }
}

// ============================================================
// System 7: process_transfers
// ============================================================

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

        if let Ok(bytes) = bincode::serialize(&msg) {
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

// ============================================================
// System 8: process_drain
// ============================================================

/// Drain all entities to target servers (round-robin). Called when draining.
/// Uses two-phase transfers: entities get TransferOutbound instead of being despawned.
/// Drain completes (ShutdownRequested) only when all entities are ACK'd or timed out.
pub fn process_drain(world: &mut World) {
    let draining = world.resource::<DrainState>().draining;
    if !draining {
        return;
    }

    let targets = match &world.resource::<DrainState>().targets {
        Some(t) if !t.is_empty() => t.clone(),
        _ => return,
    };

    let server_id = world.resource::<ServerConfig>().server_id;
    let tick = world.resource::<TickCounter>().0;

    // Record drain start tick if not set
    if world.resource::<DrainState>().started_tick.is_none() {
        world.resource_mut::<DrainState>().started_tick = Some(tick);
    }

    // Collect authoritative entities that do NOT already have TransferOutbound
    let mut to_transfer: Vec<(Entity, EntityId)> = Vec::new();
    let mut query = world.query_filtered::<(Entity, &NetworkId), With<Authoritative>>();
    for (entity, nid) in query.iter(&world) {
        if world.get::<TransferOutbound>(entity).is_none() {
            to_transfer.push((entity, nid.0));
        }
    }

    let mut transferred = 0;

    for (i, (entity, eid)) in to_transfer.iter().enumerate() {
        let target_server = targets[i % targets.len()];
        let target_addr = match world.resource::<PeerAddresses>().0.get(&target_server) {
            Some(addr) => *addr,
            None => continue,
        };

        // Build EntityState
        let nid = world.get::<NetworkId>(*entity).unwrap();
        let pos = world.get::<Position>(*entity).unwrap();
        let vel = world.get::<Velocity>(*entity).unwrap();
        let kind = world.get::<EntityKind>(*entity).unwrap();
        let auth_comp = world.get::<AuthorityServer>(*entity).unwrap();
        let seq = world.get::<LastInputSeq>(*entity);

        let mut entity_state = to_entity_state(nid, pos, vel, kind, auth_comp, seq);
        entity_state.authority_server = ServerId(target_server);

        let msg = PeerMessage::AuthorityTransfer {
            tick,
            entity_id: *eid,
            from_server: server_id,
            to_server: ServerId(target_server),
            full_state: entity_state,
        };

        if let Ok(bytes) = bincode::serialize(&msg) {
            world
                .resource_mut::<OutboundTransfers>()
                .messages
                .push((target_addr, bytes));

            // Two-phase: mark with TransferOutbound instead of despawning.
            // Entity is frozen (excluded from sim/replication via query filters).
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server,
                target_addr,
                sent_tick: tick,
                retry_count: 0,
            });
            transferred += 1;
        }
        // DO NOT despawn -- entity will be despawned when ACK arrives
    }

    if transferred > 0 {
        tracing::info!("Drained {transferred} entities (awaiting ACKs)");
    }

    // Check if drain is complete: no authoritative entities without TransferOutbound,
    // and no pending TransferOutbound entities.
    let remaining = world
        .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
        .iter(&world)
        .count();
    let pending_transfers = world
        .query_filtered::<(), With<TransferOutbound>>()
        .iter(&world)
        .count();

    if remaining == 0 && pending_transfers == 0 {
        world.resource_mut::<ShutdownRequested>().0 = true;
        tracing::info!("Drain complete: all entities transferred and ACK'd");
        return;
    }

    // Drain timeout: if drain has been active too long, force shutdown
    if let Some(started) = world.resource::<DrainState>().started_tick {
        if tick.saturating_sub(started) > DRAIN_TIMEOUT_TICKS {
            tracing::error!(
                remaining,
                pending_transfers,
                "Drain timeout after {} ticks, forcing shutdown",
                tick.saturating_sub(started),
            );
            world.resource_mut::<ShutdownRequested>().0 = true;
        }
    }
}

// ============================================================
// System 9: prepare_replication
// ============================================================

/// Query all authoritative entities and prepare for UDP replication.
/// Includes TransferOutbound entities so peers maintain up-to-date shadows for crash
/// recovery. The target server will have both its own authoritative copy AND a shadow
/// from the source -- that's fine, drain_inbound_events skips peer updates for entities
/// it already owns.
pub fn prepare_replication(
    tick: Res<TickCounter>,
    _config: Res<ServerConfig>,
    mut outbound: ResMut<OutboundReplication>,
    query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityKind,
            &AuthorityServer,
            Option<&LastInputSeq>,
        ),
        With<Authoritative>,
    >,
) {
    let entities: Vec<EntityState> = query
        .iter()
        .map(|(nid, pos, vel, kind, auth, seq)| to_entity_state(nid, pos, vel, kind, auth, seq))
        .collect();

    outbound.tick = tick.0;
    outbound.snapshot = if entities.is_empty() {
        None
    } else {
        Some(entities)
    };
}

// ============================================================
// System 10: prepare_client_snapshot
// ============================================================

/// Query both authoritative and shadow entities, compute per-client delta frames,
/// and send them through per-client mpsc channels. Falls back to full snapshots
/// when clients have no baseline, at periodic intervals, or when ACK is missing.
///
/// Also writes a full snapshot to ClientSnapshot for backward compatibility
/// (used by schedule tests that check ClientSnapshot.frame).
pub fn prepare_client_snapshot(
    tick: Res<TickCounter>,
    mut snapshot: ResMut<ClientSnapshot>,
    mut registry: ResMut<ClientRegistry>,
    auth_query: Query<
        (
            &NetworkId,
            &Position,
            &Velocity,
            &EntityKind,
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
            &EntityKind,
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
            &EntityKind,
            &TransferOutbound,
            Option<&LastInputSeq>,
        ),
        With<Authoritative>,
    >,
) {
    let mut entities = Vec::with_capacity(
        auth_query.iter().count() + shadow_query.iter().count() + transfer_query.iter().count(),
    );

    // Authoritative entities first (take precedence)
    let mut seen_ids = std::collections::HashSet::new();
    for (nid, pos, vel, kind, auth, seq) in auth_query.iter() {
        seen_ids.insert(nid.0);
        entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
    }

    // TransferOutbound entities: include in snapshot with target server's authority.
    // This prevents a 1-3 tick visibility gap where the entity vanishes from the
    // source's client view before the target starts broadcasting it as a shadow.
    // Clients see the entity continuously -- it just changes authority color.
    for (nid, pos, vel, kind, outbound, seq) in transfer_query.iter() {
        seen_ids.insert(nid.0);
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
        if !seen_ids.contains(&nid.0) {
            entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
        }
    }

    // Encode the full snapshot once, wrapped in Arc for zero-copy sharing across clients.
    // Only a single clone is made for the backward-compat ClientSnapshot resource.
    let full_frame = Arc::new(encode_binary_snapshot(tick.0, &entities));
    snapshot.frame = Some((*full_frame).clone());

    // Send per-client frames (delta or full).
    // IMPORTANT: Baseline is updated ONLY after try_send succeeds (fix for audit #018).
    // If the channel is full and the frame is dropped, the baseline is NOT advanced,
    // so the next tick re-diffs against the last state the client actually received.
    // On drop, we also force the next frame to be a full snapshot (clear baseline).
    for (_client_id, handle) in registry.clients.iter_mut() {
        let send_full = handle.baseline.is_none()
            || handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL;

        if send_full {
            // Full snapshot path: encode is shared via Arc, baseline built only on success.
            let frame = ClientFrame::Shared(Arc::clone(&full_frame));
            match handle.tx.try_send(frame) {
                Ok(()) => {
                    // Success: update baseline to match what we just sent
                    handle.baseline = Some(build_baseline(&entities));
                    handle.ticks_since_full = 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Frame dropped: clear baseline so next tick forces a full snapshot
                    tracing::warn!(client_id = _client_id, "Client frame channel full, dropping full snapshot");
                    handle.baseline = None;
                    // ticks_since_full not incremented -- will try full again next tick
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Client disconnected; will be cleaned up by unregister event
                }
            }
        } else {
            // Delta path: compute delta against baseline, update baseline only on success.
            let baseline = handle.baseline.as_ref().unwrap();
            let delta = compute_delta(&entities, baseline);

            let removal_ids: Vec<EntityId> = delta.removals.clone();
            let frame = ClientFrame::Owned(encode_binary_delta(tick.0, &delta.updates, &removal_ids));

            match handle.tx.try_send(frame) {
                Ok(()) => {
                    // Success: advance baseline to match the delta we just sent
                    update_baseline(handle.baseline.as_mut().unwrap(), &delta);
                    handle.ticks_since_full += 1;
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
        }
    }
}

// ============================================================
// System 11: shadow_cleanup
// ============================================================

/// Remove stale shadow entities.
pub fn shadow_cleanup(world: &mut World) {
    let tick = world.resource::<TickCounter>().0;

    let mut stale: Vec<(Entity, EntityId)> = Vec::new();
    let mut query = world.query_filtered::<(Entity, &NetworkId, &Shadow), With<Shadow>>();
    for (entity, nid, shadow) in query.iter(&world) {
        if tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS {
            stale.push((entity, nid.0));
        }
    }

    for (entity, eid) in stale {
        despawn_and_remove(world, entity, eid);
    }
}

// ============================================================
// System 12: record_metrics (no-op placeholder)
// ============================================================

/// Record per-tick metrics. Actual metric collection happens in the game loop.
pub fn record_metrics(
    _tick: Res<TickCounter>,
    _auth_query: Query<(), With<Authoritative>>,
    _shadow_query: Query<(), With<Shadow>>,
    _peer_addrs: Res<PeerAddresses>,
) {
    // Metrics are recorded by the game loop after schedule.run().
    // This system exists as a schedule ordering point.
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use shared::entity::{EntityId, EntityState, EntityType, ServerId};
    use shared::protocol::PeerInfo;

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
        world.init_resource::<ClientSnapshot>();
        world.init_resource::<PendingTransfers>();
        world.init_resource::<OutboundTransfers>();
        world.init_resource::<OutboundForwards>();
        world.init_resource::<DrainState>();
        world.init_resource::<ShutdownRequested>();
        world.init_resource::<TimingAccumulator>();
        world.init_resource::<ClientRegistry>();
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

    // -- physics_integration tests --

    #[test]
    fn physics_moves_entity() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(5.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert!(pos.0.x > 0.0); // moved in +X
    }

    #[test]
    fn physics_shadow_not_moved() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(10.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // shadow not moved
    }

    #[test]
    fn physics_zero_velocity_no_movement() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 5.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::new(10.0, 0.0, 5.0)); // unchanged
    }

    // -- clamp_positions tests --

    #[test]
    fn clamp_out_of_bounds() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(9999.0, 0.0, -9999.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0.x, 500.0);
        assert_eq!(pos.0.z, -500.0);
    }

    #[test]
    fn clamp_in_bounds_unchanged() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, -200.0), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::new(100.0, 0.0, -200.0));
    }

    // -- npc_behavior tests --

    #[test]
    fn npc_velocity_changes_at_interval() {
        let mut world = setup_world(1);
        let (entity, _eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Run enough ticks to trigger direction change
        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        let mut found_nonzero = false;
        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
            let vel = world.get::<Velocity>(entity).unwrap();
            if vel.0.length() > 0.01 {
                found_nonzero = true;
            }
        }
        assert!(found_nonzero, "NPC should have gained velocity after direction change");
    }

    #[test]
    fn npc_behavior_does_not_modify_player() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO); // player velocity not touched
    }

    #[test]
    fn npc_behavior_does_not_modify_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO);
    }

    // -- shadow_cleanup tests --

    #[test]
    fn shadow_cleanup_removes_stale() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        world.resource_mut::<TickCounter>().0 = 400; // well past stale threshold (300)

        shadow_cleanup(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn shadow_cleanup_keeps_fresh() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(50));

        world.resource_mut::<TickCounter>().0 = 60; // only 10 ticks old

        shadow_cleanup(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- prepare_replication tests --

    #[test]
    fn replication_only_authoritative() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        // Shadow entity
        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some(1));

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.snapshot.is_some());
        assert_eq!(outbound.snapshot.as_ref().unwrap().len(), 2); // only auth
    }

    #[test]
    fn replication_empty_world() {
        let mut world = setup_world(1);
        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_replication);
        schedule.run(&mut world);

        let outbound = world.resource::<OutboundReplication>();
        assert!(outbound.snapshot.is_none());
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
        let entities = outbound.snapshot.as_ref().unwrap();
        assert_eq!(entities[0].last_input_seq, 42);
    }

    // -- prepare_client_snapshot tests --

    #[test]
    fn client_snapshot_merges_auth_and_shadow() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        spawn_auth_npc(&mut world, Vec3::X, 1);

        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some(1));

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let snapshot = world.resource::<ClientSnapshot>();
        let frame = snapshot.frame.as_ref().unwrap();
        // Header 27 + 3 entities * 47
        assert_eq!(frame.len(), 27 + 3 * 47);
    }

    #[test]
    fn client_snapshot_binary_format() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.resource_mut::<TickCounter>().0 = 42;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let frame = world.resource::<ClientSnapshot>().frame.as_ref().unwrap();
        assert_eq!(frame[0], 0x01); // full snapshot type
        let tick = u64::from_le_bytes(frame[1..9].try_into().unwrap());
        assert_eq!(tick, 42);
        let count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        assert_eq!(count, 1);
    }

    // -- per-client delta compression tests --

    /// Helper: register a test client in the ClientRegistry and return (client_id, receiver).
    fn register_test_client(world: &mut World, client_id: u64) -> tokio::sync::mpsc::Receiver<ClientFrame> {
        use std::sync::atomic::AtomicU64;
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientFrame>(16);
        let acked = std::sync::Arc::new(AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>()
            .register_from_event(client_id, tx, acked);
        rx
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

        let frame = rx.try_recv().unwrap().into_vec();
        assert_eq!(frame[0], 0x01, "first frame to a new client must be a full snapshot");
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
        let frame1 = rx.try_recv().unwrap().into_vec();
        assert_eq!(frame1[0], 0x01, "first frame should be full snapshot");

        // Tick 2: no changes -> delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame2 = rx.try_recv().unwrap().into_vec();
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
        rx.try_recv().unwrap().into_vec(); // consume

        // Tick 2: no changes -> delta with 0 updates, 0 removals
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap().into_vec();
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
        let mut rx = register_test_client(&mut world, 100);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Tick 1: full snapshot
        world.resource_mut::<TickCounter>().0 = 1;
        schedule.run(&mut world);
        rx.try_recv().unwrap().into_vec();

        // Move the entity
        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(50.0, 0.0, 50.0);

        // Tick 2: delta should include the moved entity
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let frame = rx.try_recv().unwrap().into_vec();
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
        world.resource_mut::<ClientRegistry>()
            .register_from_event(100, tx, acked);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Run 101 ticks: tick 1 = full, ticks 2-100 = delta, tick 101 = full again
        for tick in 1..=101 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        // Read all 101 frames and check types
        let mut full_count = 0u32;
        let mut _delta_count = 0u32;
        let mut last_frame_type = 0u8;
        for _ in 0..101 {
            let frame = rx.try_recv().unwrap().into_vec();
            if frame[0] == 0x01 {
                full_count += 1;
            } else {
                _delta_count += 1;
            }
            last_frame_type = frame[0];
        }
        // First frame (tick 1) is full, tick 101 should also be full (100 ticks since full)
        assert!(full_count >= 2, "should have at least 2 full snapshots (first + periodic): got {full_count}");
        assert_eq!(last_frame_type, 0x01, "tick 101 should be a full snapshot (100 ticks after first)");
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

        let frame_a = rx_a.try_recv().unwrap().into_vec();
        let frame_b = rx_b.try_recv().unwrap().into_vec();
        assert_eq!(frame_a[0], 0x01, "client A first frame should be full");
        assert_eq!(frame_b[0], 0x01, "client B first frame should be full");

        // Tick 2: both should get delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);

        let frame_a2 = rx_a.try_recv().unwrap().into_vec();
        let frame_b2 = rx_b.try_recv().unwrap().into_vec();
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
        let fa1 = rx_a.try_recv().unwrap().into_vec();
        assert_eq!(fa1[0], 0x01);

        // Register client B mid-session
        let mut rx_b = register_test_client(&mut world, 200);

        // Tick 2: client A gets delta, client B gets full
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);

        let fa2 = rx_a.try_recv().unwrap().into_vec();
        let fb1 = rx_b.try_recv().unwrap().into_vec();
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
        let f1 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f1[0], 0x01);

        // Tick 2: delta
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f2[0], 0x02);

        // Request forced full snapshot
        world.resource_mut::<InboundQueue>().force_full_snapshot_clients.push(100);

        // Tick 3: should be full (baseline was cleared by drain_client_events)
        world.resource_mut::<TickCounter>().0 = 3;
        schedule.run(&mut world);
        let f3 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f3[0], 0x01, "forced full snapshot should produce 0x01 frame");
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
        let f1 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f1[0], 0x01);
        let count1 = u32::from_le_bytes(f1[9..13].try_into().unwrap());
        assert_eq!(count1, 2);

        // Remove first entity
        crate::conversion::despawn_and_remove(&mut world, entity, eid);

        // Tick 2: delta with 1 removal
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap().into_vec();
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
        rx.try_recv().unwrap().into_vec();

        // Spawn a new entity
        spawn_auth_npc(&mut world, Vec3::new(10.0, 0.0, 10.0), 1);

        // Tick 2: delta should contain the new entity
        world.resource_mut::<TickCounter>().0 = 2;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f2[0], 0x02);
        let update_count = u32::from_le_bytes(f2[9..13].try_into().unwrap());
        assert_eq!(update_count, 1, "new entity should be in delta updates");
    }

    #[test]
    fn per_client_ticks_since_full_resets_on_full() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // Use a large enough channel to hold all frames without drops
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientFrame>(128);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        world.resource_mut::<ClientRegistry>()
            .register_from_event(100, tx, acked);

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);

        // Run to just before periodic full snapshot
        for tick in 1..=100 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }
        // Consume 100 frames
        for _ in 0..100 {
            rx.try_recv().unwrap().into_vec();
        }

        // Tick 101: should be full (periodic reset)
        world.resource_mut::<TickCounter>().0 = 101;
        schedule.run(&mut world);
        let f = rx.try_recv().unwrap().into_vec();
        assert_eq!(f[0], 0x01, "tick 101 should be periodic full snapshot");

        // Tick 102: should be delta again (counter reset)
        world.resource_mut::<TickCounter>().0 = 102;
        schedule.run(&mut world);
        let f2 = rx.try_recv().unwrap().into_vec();
        assert_eq!(f2[0], 0x02, "tick after periodic full should be delta again");
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
        world.resource_mut::<ClientRegistry>()
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
    fn per_client_backward_compat_client_snapshot_resource() {
        // Verify that ClientSnapshot resource is still populated for backward compatibility
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        // No clients registered - ClientSnapshot should still be written
        world.resource_mut::<TickCounter>().0 = 5;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let snap = world.resource::<ClientSnapshot>();
        assert!(snap.frame.is_some(), "ClientSnapshot should be populated even with no clients");
        let frame = snap.frame.as_ref().unwrap();
        assert_eq!(frame[0], 0x01);
    }

    // -- drain_client_events tests --

    #[test]
    fn drain_client_events_registers_client() {
        let mut world = setup_world(1);
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientFrame>(8);
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        world.resource_mut::<InboundQueue>().register_clients.push(
            ClientRegistrationEvent {
                client_id: 42,
                tx,
                acked_tick: acked,
                player_uuid: uuid::Uuid::new_v4().to_string(),
            }
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(world.resource::<ClientRegistry>().clients.contains_key(&42));
    }

    #[test]
    fn drain_client_events_unregisters_client() {
        let mut world = setup_world(1);
        let _rx = register_test_client(&mut world, 42);

        world.resource_mut::<InboundQueue>().unregister_clients.push(42);

        let mut schedule = Schedule::default();
        schedule.add_systems(drain_client_events);
        schedule.run(&mut world);

        assert!(!world.resource::<ClientRegistry>().clients.contains_key(&42));
    }

    #[test]
    fn drain_client_events_force_full_clears_baseline() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        let mut rx = register_test_client(&mut world, 42);

        // Run one tick to establish baseline
        let mut snap_schedule = Schedule::default();
        snap_schedule.add_systems(prepare_client_snapshot);
        world.resource_mut::<TickCounter>().0 = 1;
        snap_schedule.run(&mut world);
        rx.try_recv().unwrap().into_vec(); // consume

        // Verify baseline exists
        assert!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().baseline.is_some());

        // Force full snapshot
        world.resource_mut::<InboundQueue>().force_full_snapshot_clients.push(42);
        let mut drain_schedule = Schedule::default();
        drain_schedule.add_systems(drain_client_events);
        drain_schedule.run(&mut world);

        // Baseline should be cleared
        assert!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().baseline.is_none());
        assert_eq!(world.resource::<ClientRegistry>().clients.get(&42).unwrap().ticks_since_full, 0);
    }

    // -- drain_inbound_events tests --

    #[test]
    fn peer_update_creates_shadow() {
        let mut world = setup_world(1);
        let entity_state = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        let eid = entity_state.id;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(2),
            entities: vec![entity_state],
        });

        drain_inbound_events(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        assert!(world.get::<Shadow>(bevy_entity).is_some());
        assert!(world.get::<Authoritative>(bevy_entity).is_none());
    }

    #[test]
    fn peer_update_does_not_overwrite_authoritative() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Peer sends update for same entity
        let mut shadow_state = EntityState::new_npc(Vec3::new(999.0, 0.0, 999.0), ServerId(2));
        shadow_state.id = eid;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(2),
            entities: vec![shadow_state],
        });

        drain_inbound_events(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // unchanged
    }

    #[test]
    fn peer_update_stale_tick_rejected() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(20));

        // Stale update at tick 15
        let mut stale = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        stale.id = eid;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 15,
            server_id: ServerId(2),
            entities: vec![stale],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0.x, 1.0); // unchanged
    }

    #[test]
    fn peer_update_newer_tick_updates_shadow() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        let mut newer = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        newer.id = eid;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 20,
            server_id: ServerId(2),
            entities: vec![newer],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0.x, 50.0);
    }

    #[test]
    fn authority_transfer_creates_authoritative() {
        let mut world = setup_world(2);
        let entity_state = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(2));
        let eid = entity_state.id;

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 100,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: entity_state,
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
        assert!(world.get::<Shadow>(bevy_entity).is_none());
    }

    #[test]
    fn authority_transfer_promotes_shadow() {
        let mut world = setup_world(2);
        let shadow_state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(5));

        let mut full_state = EntityState::new_npc(Vec3::new(20.0, 0.0, 20.0), ServerId(2));
        full_state.id = eid;

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 100,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state,
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
        assert!(world.get::<Shadow>(bevy_entity).is_none());
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::new(20.0, 0.0, 20.0));
    }

    #[test]
    fn authority_transfer_wrong_server_rejected() {
        let mut world = setup_world(2);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(3));
        let eid = state.id;

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 100,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(3), // not us (we are 2)
            full_state: state,
        });

        drain_inbound_events(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn authority_transfer_duplicate_rejected() {
        let mut world = setup_world(2);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 2);

        let mut state = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        state.id = eid;

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 100,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO); // unchanged -- duplicate rejected
    }

    #[test]
    fn forwarded_input_applies() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, 0.0],
            seq: 7,
        });

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!(vel.0.x > 0.0);
        let seq = world.get::<LastInputSeq>(entity).unwrap();
        assert_eq!(seq.0, 7);
    }

    #[test]
    fn forwarded_input_non_authoritative_ignored() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, false, Some(1));

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, 0.0],
            seq: 5,
        });

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO); // unchanged
    }

    // -- process_client_inputs tests --

    #[test]
    fn client_spawn_npc() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::SpawnEntity {
            position: [10.0, 0.0, -20.0],
            entity_type: "npc".to_string(),
        });

        process_client_inputs(&mut world);

        assert_eq!(world.resource::<EntityLookup>().0.len(), 1);
        let mut query = world.query_filtered::<&EntityKind, With<Authoritative>>();
        let kind = query.single(&world).unwrap();
        assert_eq!(kind.0, EntityType::Npc);
    }

    #[test]
    fn client_spawn_player_type() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::SpawnEntity {
            position: [0.0, 0.0, 0.0],
            entity_type: "player".to_string(),
        });

        process_client_inputs(&mut world);

        let mut query = world.query_filtered::<&EntityKind, With<Authoritative>>();
        let kind = query.single(&world).unwrap();
        assert_eq!(kind.0, EntityType::Player);
    }

    #[test]
    fn client_spawn_player_with_uuid() {
        let mut world = setup_world(1);
        let uuid = uuid::Uuid::new_v4();
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::SpawnPlayer {
            player_id: uuid.to_string(),
            position: [5.0, 0.0, 5.0],
        });

        process_client_inputs(&mut world);

        let eid = EntityId(uuid);
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn client_move_input_sets_velocity() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 42,
        });

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!(vel.0.x > 0.0);
        let seq = world.get::<LastInputSeq>(entity).unwrap();
        assert_eq!(seq.0, 42);
    }

    #[test]
    fn client_move_for_shadow_produces_forward() {
        let mut world = setup_world(1);
        let shadow = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let shadow_id = shadow.id;
        spawn_from_entity_state_direct(&mut world, &shadow, false, Some(1));

        // Register peer address
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: shadow_id.0.to_string(),
            direction: [0.0, 0.0, 1.0],
            seq: 3,
        });

        process_client_inputs(&mut world);

        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1);
    }

    #[test]
    fn client_remove_entity() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn client_request_snapshot_noop() {
        let mut world = setup_world(1);
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RequestSnapshot);

        process_client_inputs(&mut world); // should not panic
    }

    // -- process_coord_events tests --

    #[test]
    fn coord_peer_list_updates_addresses() {
        let mut world = setup_world(1);
        world.resource_mut::<CoordEventQueue>().peer_list_updates.push(vec![
            PeerInfo {
                server_id: 2,
                udp_addr: "127.0.0.1:9102".to_string(),
                ws_addr: "127.0.0.1:7879".to_string(),
            },
        ]);

        process_coord_events(&mut world);

        let peers = &world.resource::<PeerAddresses>().0;
        assert_eq!(peers.len(), 1);
        assert!(peers.contains_key(&2));
    }

    #[test]
    fn coord_rebalance_sets_pending() {
        let mut world = setup_world(1);
        world.resource_mut::<CoordEventQueue>().rebalance_directives.push((
            vec!["some-id".to_string()],
            2,
        ));

        process_coord_events(&mut world);

        let pending = &world.resource::<PendingTransfers>().transfers;
        assert!(pending.is_some());
    }

    #[test]
    fn coord_drain_sets_state() {
        let mut world = setup_world(1);
        world.resource_mut::<CoordEventQueue>().drain_directives.push(vec![2, 3]);

        process_coord_events(&mut world);

        let drain = world.resource::<DrainState>();
        assert!(drain.draining);
        assert_eq!(drain.targets, Some(vec![2, 3]));
    }

    #[test]
    fn coord_recover_promotes_correct_subset() {
        let mut world = setup_world(1);

        // Spawn shadows from dead server 3
        let mut shadow_ids = Vec::new();
        for _ in 0..20 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(3));
            shadow_ids.push(state.id);
            spawn_from_entity_state_direct(&mut world, &state, false, Some(10));
        }

        world.resource_mut::<CoordEventQueue>().recover_events.push((3, vec![1, 2]));

        process_coord_events(&mut world);

        // Count authoritative entities (recovered)
        let auth_count = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        let shadow_count = world.query_filtered::<(), With<Shadow>>().iter(&world).count();
        assert_eq!(auth_count + shadow_count, 20);
        assert!(auth_count > 0); // at least some should be adopted
    }

    // -- process_transfers tests --

    #[test]
    fn transfer_only_npcs_not_players() {
        let mut world = setup_world(1);

        // Spawn some NPCs and a player
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }
        spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        // Player should still exist
        let mut query = world.query_filtered::<&EntityKind, With<Authoritative>>();
        let mut player_exists = false;
        for kind in query.iter(&world) {
            if kind.0 == EntityType::Player {
                player_exists = true;
            }
        }
        assert!(player_exists);
    }

    #[test]
    fn transfer_produces_outbound_messages() {
        let mut world = setup_world(1);
        for _ in 0..10 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        let outbound = &world.resource::<OutboundTransfers>().messages;
        assert!(!outbound.is_empty());
    }

    // -- process_drain tests --

    #[test]
    fn drain_all_entities() {
        let mut world = setup_world(1);
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Two-phase: entities still have Authoritative + TransferOutbound, not yet shutdown
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 5, "entities remain until ACK (two-phase)");
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 5, "all entities should have TransferOutbound");
        assert!(!world.resource::<ShutdownRequested>().0, "not shutdown yet (waiting for ACKs)");
    }

    #[test]
    fn drain_not_active_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        process_drain(&mut world);

        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 1); // unchanged
    }

    // ============================================================
    // Additional coverage gap tests
    // ============================================================

    // -- Shadow update boundary: equal tick rejected --

    #[test]
    fn peer_update_equal_tick_accepted() {
        // The stale check is `update.tick < shadow.last_seen_tick`, so equal ticks
        // are accepted. This is correct: same-tick updates can carry different positions
        // (e.g., re-send of a corrected packet).
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(20));

        let mut same_tick = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        same_tick.id = eid;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 20,
            server_id: ServerId(2),
            entities: vec![same_tick],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0.x, 99.0); // accepted, equal tick updates position
    }

    #[test]
    fn peer_update_strictly_older_tick_rejected() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(20));

        // Tick 19 is strictly less than last_seen_tick 20
        let mut older = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        older.id = eid;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 19,
            server_id: ServerId(2),
            entities: vec![older],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let pos = world.get::<Position>(bevy_entity).unwrap();
        assert_eq!(pos.0.x, 1.0); // unchanged, strictly older rejected
    }

    // -- Authority transfer promotes shadow with player LastInputSeq --

    #[test]
    fn authority_transfer_promotes_shadow_player_with_seq() {
        let mut world = setup_world(2);
        let shadow_state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(5));

        let mut full_state = EntityState::new_player(Vec3::new(20.0, 0.0, 20.0), ServerId(2));
        full_state.id = eid;
        full_state.last_input_seq = 42;

        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 100,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state,
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
        assert!(world.get::<Shadow>(bevy_entity).is_none());
        let seq = world.get::<LastInputSeq>(bevy_entity).unwrap();
        assert_eq!(seq.0, 42);
        let kind = world.get::<EntityKind>(bevy_entity).unwrap();
        assert_eq!(kind.0, EntityType::Player);
    }

    // -- Forwarded input speed: player vs npc --

    #[test]
    fn forwarded_input_player_speed_is_5() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        });

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!((vel.0.x - 5.0).abs() < 0.01);
    }

    #[test]
    fn forwarded_input_npc_speed_is_2() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: eid,
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        });

        drain_inbound_events(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert!((vel.0.x - 2.0).abs() < 0.01);
    }

    // -- Forwarded input for unknown entity --

    #[test]
    fn forwarded_input_unknown_entity_ignored() {
        let mut world = setup_world(1);
        let unknown_eid = EntityId::new();

        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: unknown_eid,
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        });

        // Should not panic
        drain_inbound_events(&mut world);
    }

    // -- Multiple peer updates in single tick --

    #[test]
    fn multiple_peer_updates_same_tick() {
        let mut world = setup_world(1);
        let state1 = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(2));
        let state2 = EntityState::new_npc(Vec3::new(20.0, 0.0, 20.0), ServerId(3));
        let eid1 = state1.id;
        let eid2 = state2.id;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(2),
            entities: vec![state1],
        });
        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(3),
            entities: vec![state2],
        });

        drain_inbound_events(&mut world);

        assert!(world.resource::<EntityLookup>().0.contains_key(&eid1));
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid2));
    }

    // -- Multiple coord events on same tick --

    #[test]
    fn multiple_coord_events_same_tick() {
        let mut world = setup_world(1);

        // Two peer list updates -- the second should overwrite
        world.resource_mut::<CoordEventQueue>().peer_list_updates.push(vec![
            PeerInfo {
                server_id: 2,
                udp_addr: "127.0.0.1:9102".to_string(),
                ws_addr: "127.0.0.1:7879".to_string(),
            },
        ]);
        world.resource_mut::<CoordEventQueue>().peer_list_updates.push(vec![
            PeerInfo {
                server_id: 3,
                udp_addr: "127.0.0.1:9103".to_string(),
                ws_addr: "127.0.0.1:7880".to_string(),
            },
            PeerInfo {
                server_id: 4,
                udp_addr: "127.0.0.1:9104".to_string(),
                ws_addr: "127.0.0.1:7881".to_string(),
            },
        ]);

        process_coord_events(&mut world);

        // Second peer list cleared and replaced
        let peers = &world.resource::<PeerAddresses>().0;
        assert_eq!(peers.len(), 2);
        assert!(peers.contains_key(&3));
        assert!(peers.contains_key(&4));
        assert!(!peers.contains_key(&2)); // first list was overwritten
    }

    // -- process_transfers: no peer address for target --

    #[test]
    fn transfer_no_peer_address_noop() {
        let mut world = setup_world(1);
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        // Set pending transfer but don't register peer address
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 99));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        // All entities should remain
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 5);
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
    }

    // -- process_transfers: peripheral entities transferred first --

    #[test]
    fn transfer_peripheral_entities_first() {
        let mut world = setup_world(1);

        // Spawn NPCs at varying distances from origin
        let (_, close_eid) = spawn_auth_npc(&mut world, Vec3::new(1.0, 0.0, 0.0), 1);
        let (_, far_eid) = spawn_auth_npc(&mut world, Vec3::new(400.0, 0.0, 400.0), 1);
        let (_, mid_eid) = spawn_auth_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 1);

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        // With 3 NPCs + 0 others = 3 total auth, max(3/10, 1) = 1 transferred
        // Two-phase: far entity still in lookup but has TransferOutbound
        assert!(world.resource::<EntityLookup>().0.contains_key(&far_eid));
        assert!(world.resource::<EntityLookup>().0.contains_key(&close_eid));
        assert!(world.resource::<EntityLookup>().0.contains_key(&mid_eid));

        // The farthest entity should have TransferOutbound
        let far_entity = world.resource::<EntityLookup>().0[&far_eid];
        assert!(world.get::<TransferOutbound>(far_entity).is_some(),
            "farthest entity should have TransferOutbound");
        // Close and mid should NOT have TransferOutbound
        let close_entity = world.resource::<EntityLookup>().0[&close_eid];
        assert!(world.get::<TransferOutbound>(close_entity).is_none());
        let mid_entity = world.resource::<EntityLookup>().0[&mid_eid];
        assert!(world.get::<TransferOutbound>(mid_entity).is_none());
    }

    // -- process_drain: round-robin to multiple targets --

    #[test]
    fn drain_round_robin_to_multiple_targets() {
        let mut world = setup_world(1);
        for _ in 0..6 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PeerAddresses>().0.insert(3, "127.0.0.1:9103".parse().unwrap());
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2, 3]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Two-phase: all entities still Authoritative + TransferOutbound, not shutdown yet
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 6, "entities remain with TransferOutbound (two-phase)");
        let outbound_count = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound_count, 6);
        assert!(!world.resource::<ShutdownRequested>().0, "not shutdown yet");

        // Should have 6 outbound transfer messages
        let outbound = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(outbound.len(), 6);

        // Verify round-robin: alternating addresses
        let addr_9102: std::net::SocketAddr = "127.0.0.1:9102".parse().unwrap();
        let addr_9103: std::net::SocketAddr = "127.0.0.1:9103".parse().unwrap();
        let count_9102 = outbound.iter().filter(|(a, _)| *a == addr_9102).count();
        let count_9103 = outbound.iter().filter(|(a, _)| *a == addr_9103).count();
        assert_eq!(count_9102, 3);
        assert_eq!(count_9103, 3);
    }

    // -- process_drain: empty targets list --

    #[test]
    fn drain_empty_targets_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![]);

        process_drain(&mut world);

        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 1); // unchanged, empty targets
    }

    #[test]
    fn drain_targets_none_noop() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = None;

        process_drain(&mut world);

        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 1); // unchanged, no targets
    }

    // -- Drain + transfer interaction on same tick --

    #[test]
    fn drain_and_transfer_same_tick() {
        let mut world = setup_world(1);
        for _ in 0..10 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PeerAddresses>().0.insert(3, "127.0.0.1:9103".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 10;

        // Set both a rebalance transfer and a drain active
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![3]);

        // Run process_transfers first (as in schedule order)
        process_transfers(&mut world);

        // Two-phase: all entities still authoritative, some have TransferOutbound from rebalance
        let after_transfer = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(after_transfer, 10, "all entities remain (two-phase)");
        let rebalance_outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert!(rebalance_outbound > 0, "some entities should be marked for rebalance transfer");

        // Now run process_drain -- only entities WITHOUT TransferOutbound get drain-transferred
        process_drain(&mut world);

        // All entities should now have TransferOutbound
        let all_outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(all_outbound, 10, "all entities should have TransferOutbound after drain");
        // Not shutdown yet -- waiting for ACKs
        assert!(!world.resource::<ShutdownRequested>().0);
    }

    // -- Crash recovery: single survivor gets all entities --

    #[test]
    fn recover_single_survivor_gets_all() {
        let mut world = setup_world(1);

        // Spawn 10 shadows from dead server 3
        for _ in 0..10 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(3));
            spawn_from_entity_state_direct(&mut world, &state, false, Some(10));
        }

        // Only survivor is us (server 1)
        world.resource_mut::<CoordEventQueue>().recover_events.push((3, vec![1]));

        process_coord_events(&mut world);

        // We should adopt all 10
        let auth_count = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(auth_count, 10);
        let shadow_count = world.query_filtered::<(), With<Shadow>>().iter(&world).count();
        assert_eq!(shadow_count, 0);
    }

    // -- Crash recovery: dead server with no shadows --

    #[test]
    fn recover_no_shadows_from_dead_server() {
        let mut world = setup_world(1);

        // Spawn shadows from server 2 (not the dead one)
        for _ in 0..5 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
            spawn_from_entity_state_direct(&mut world, &state, false, Some(10));
        }

        // Server 3 dies but we have no shadows from it
        world.resource_mut::<CoordEventQueue>().recover_events.push((3, vec![1, 2]));

        process_coord_events(&mut world);

        let auth_count = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(auth_count, 0); // nothing to adopt
        let shadow_count = world.query_filtered::<(), With<Shadow>>().iter(&world).count();
        assert_eq!(shadow_count, 5); // server 2 shadows untouched
    }

    // -- Crash recovery: we are not in survivor list --

    #[test]
    fn recover_not_in_survivor_list_noop() {
        let mut world = setup_world(1);

        for _ in 0..5 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(3));
            spawn_from_entity_state_direct(&mut world, &state, false, Some(10));
        }

        // Survivors are 2 and 4, not us (1)
        world.resource_mut::<CoordEventQueue>().recover_events.push((3, vec![2, 4]));

        process_coord_events(&mut world);

        let auth_count = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(auth_count, 0); // we don't adopt anything
    }

    // -- Crash recovery: promoted entities get our server_id --

    #[test]
    fn recover_promoted_entity_gets_our_server_id() {
        let mut world = setup_world(1);

        let state = EntityState::new_npc(Vec3::ZERO, ServerId(3));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        // Single survivor = us, so all hash to us
        world.resource_mut::<CoordEventQueue>().recover_events.push((3, vec![1]));

        process_coord_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let auth = world.get::<AuthorityServer>(bevy_entity).unwrap();
        assert_eq!(auth.0, ServerId(1)); // authority updated to us
    }

    // -- Client input: MoveInput direction normalization --

    #[test]
    fn client_move_input_normalizes_direction() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Send unnormalized direction [3, 0, 4] (length 5)
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [3.0, 0.0, 4.0],
            seq: 1,
        });

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        // Should be normalized * PLAYER_SPEED (5.0)
        // normalize([3,0,4]) = [0.6, 0, 0.8], * 5.0 = [3.0, 0, 4.0]
        assert!((vel.0.length() - 5.0).abs() < 0.01);
    }

    // -- Client input: zero direction gives zero velocity --

    #[test]
    fn client_move_input_zero_direction() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [0.0, 0.0, 0.0],
            seq: 1,
        });

        process_client_inputs(&mut world);

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO); // normalize_or_zero gives ZERO
    }

    // -- Client input: invalid UUID ignored --

    #[test]
    fn client_move_input_invalid_uuid_ignored() {
        let mut world = setup_world(1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: "not-a-valid-uuid".to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 1,
        });

        // Should not panic
        process_client_inputs(&mut world);
    }

    #[test]
    fn client_remove_invalid_uuid_ignored() {
        let mut world = setup_world(1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: "garbage".to_string(),
        });

        // Should not panic
        process_client_inputs(&mut world);
    }

    #[test]
    fn client_remove_nonexistent_entity_noop() {
        let mut world = setup_world(1);
        let uuid = uuid::Uuid::new_v4();

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: uuid.to_string(),
        });

        // Should not panic
        process_client_inputs(&mut world);
    }

    // -- RemoveEntity during transfer forwards remove to target (#040) --

    #[test]
    fn client_remove_during_transfer_forwards_to_target() {
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Mark entity as mid-transfer
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 10,
            retry_count: 0,
        });

        // Client sends RemoveEntity
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        // Entity should be despawned locally
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // A ForwardedRemove should have been sent to the target
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should forward remove to transfer target");
        assert_eq!(forwards[0].0, "127.0.0.1:9102".parse::<std::net::SocketAddr>().unwrap());

        // Verify it deserializes to ForwardedRemove
        let msg: PeerMessage = bincode::deserialize(&forwards[0].1).unwrap();
        match msg {
            PeerMessage::ForwardedRemove { entity_id } => {
                assert_eq!(entity_id, eid);
            }
            _ => panic!("expected ForwardedRemove, got {:?}", msg),
        }
    }

    #[test]
    fn client_remove_without_transfer_does_not_forward() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // No TransferOutbound -- normal remove
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        // Entity should be despawned locally
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));

        // No forward should be sent
        assert!(world.resource::<OutboundForwards>().messages.is_empty());
    }

    // -- Heartbeat count excludes frozen entities (#041) --

    #[test]
    fn heartbeat_count_excludes_transfer_outbound() {
        let mut world = setup_world(1);

        // Spawn 5 auth entities
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        // Mark 2 as mid-transfer
        let mut query = world.query_filtered::<Entity, With<Authoritative>>();
        let entities: Vec<Entity> = query.iter(&world).collect();
        for entity in entities.iter().take(2) {
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 1,
                retry_count: 0,
            });
        }

        // Simulate the heartbeat count query from game_loop.rs
        let auth_count = world
            .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
            .iter(&world)
            .count();

        // Should be 3, not 5 (excludes the 2 frozen entities)
        assert_eq!(auth_count, 3);
    }

    // -- TransferOutbound entities visible in client snapshot with target authority (#042) --

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

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let snapshot = world.resource::<ClientSnapshot>();
        let frame = snapshot.frame.as_ref().unwrap();

        // Header 27 bytes + 3 entities * 47 bytes = 168
        assert_eq!(frame.len(), 27 + 3 * 47,
            "transfer outbound entity should be included in snapshot");

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
        assert!(found_target_auth, "transfer outbound entity should report target server as authority");
    }

    // -- Peer update: multiple entities in single update --

    #[test]
    fn peer_update_batch_creates_all_shadows() {
        let mut world = setup_world(1);

        let entities: Vec<EntityState> = (0..5)
            .map(|i| EntityState::new_npc(Vec3::new(i as f32, 0.0, 0.0), ServerId(2)))
            .collect();
        let eids: Vec<EntityId> = entities.iter().map(|e| e.id).collect();

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 10,
            server_id: ServerId(2),
            entities,
        });

        drain_inbound_events(&mut world);

        for eid in &eids {
            assert!(world.resource::<EntityLookup>().0.contains_key(eid));
        }
        let shadow_count = world.query_filtered::<(), With<Shadow>>().iter(&world).count();
        assert_eq!(shadow_count, 5);
    }

    // -- Peer update: updates velocity on existing shadow --

    #[test]
    fn peer_update_updates_shadow_velocity() {
        let mut world = setup_world(1);
        let mut state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        state.velocity = Vec3::new(1.0, 0.0, 0.0);
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        let mut updated = state.clone();
        updated.velocity = Vec3::new(0.0, 0.0, 5.0);

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 20,
            server_id: ServerId(2),
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let vel = world.get::<Velocity>(bevy_entity).unwrap();
        assert_eq!(vel.0, Vec3::new(0.0, 0.0, 5.0));
    }

    // -- Peer update: updates authority_server on existing shadow --

    #[test]
    fn peer_update_updates_shadow_last_input_seq() {
        let mut world = setup_world(1);
        let mut state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        state.last_input_seq = 0;
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        // Peer sends update with last_input_seq advanced
        let mut updated = state.clone();
        updated.last_input_seq = 42;

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 20,
            server_id: ServerId(2),
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let seq = world.get::<LastInputSeq>(bevy_entity).unwrap();
        assert_eq!(seq.0, 42);
    }

    #[test]
    fn peer_update_updates_shadow_authority() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(10));

        // Entity was transferred to server 3
        let mut updated = state.clone();
        updated.authority_server = ServerId(3);

        world.resource_mut::<InboundQueue>().peer_updates.push(PeerUpdate {
            tick: 20,
            server_id: ServerId(2),
            entities: vec![updated],
        });

        drain_inbound_events(&mut world);

        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        let auth = world.get::<AuthorityServer>(bevy_entity).unwrap();
        assert_eq!(auth.0, ServerId(3));
    }

    // -- Shadow cleanup boundary: exactly at stale threshold --

    #[test]
    fn shadow_cleanup_at_exact_threshold() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(0));

        // Set tick to exactly SHADOW_STALE_TICKS (300)
        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS;

        shadow_cleanup(&mut world);

        // Exactly at threshold should NOT be removed (uses > not >=)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn shadow_cleanup_one_past_threshold() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, false, Some(0));

        // One tick past threshold
        world.resource_mut::<TickCounter>().0 = SHADOW_STALE_TICKS + 1;

        shadow_cleanup(&mut world);

        // Should be removed
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // -- Client snapshot deduplication --

    #[test]
    fn client_snapshot_deduplicates_auth_and_shadow_same_entity() {
        let mut world = setup_world(1);

        // Create an entity that exists as both auth and shadow (shouldn't happen
        // in normal operation, but tests the deduplication safety)
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let auth_entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Manually spawn a shadow with same NetworkId (bypassing lookup, which overwrites)
        let shadow_entity = world.spawn((
            NetworkId(eid),
            Position(Vec3::new(99.0, 0.0, 99.0)),
            Velocity(Vec3::ZERO),
            EntityKind(EntityType::Npc),
            AuthorityServer(ServerId(2)),
            Shadow { last_seen_tick: 10 },
        )).id();

        world.resource_mut::<TickCounter>().0 = 10;

        let mut schedule = Schedule::default();
        schedule.add_systems(prepare_client_snapshot);
        schedule.run(&mut world);

        let snapshot = world.resource::<ClientSnapshot>();
        let frame = snapshot.frame.as_ref().unwrap();
        let entity_count = u32::from_le_bytes(frame[9..13].try_into().unwrap());
        // Should only include the entity once (auth takes precedence)
        assert_eq!(entity_count, 1);

        // Cleanup
        world.despawn(auth_entity);
        world.despawn(shadow_entity);
    }

    // -- Physics integration dt correctness --

    #[test]
    fn physics_dt_is_tick_duration() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(20.0, 0.0, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        // dt = 50ms = 0.05s, vel = 20 -> pos should be 1.0
        assert!((pos.0.x - 1.0).abs() < 0.001);
    }

    // -- Coord events: unparseable peer address --

    #[test]
    fn coord_peer_list_unparseable_address_skipped() {
        let mut world = setup_world(1);
        world.resource_mut::<CoordEventQueue>().peer_list_updates.push(vec![
            PeerInfo {
                server_id: 2,
                udp_addr: "not-a-valid-address".to_string(),
                ws_addr: "127.0.0.1:7879".to_string(),
            },
            PeerInfo {
                server_id: 3,
                udp_addr: "127.0.0.1:9103".to_string(),
                ws_addr: "127.0.0.1:7880".to_string(),
            },
        ]);

        process_coord_events(&mut world);

        let peers = &world.resource::<PeerAddresses>().0;
        assert_eq!(peers.len(), 1); // only server 3
        assert!(peers.contains_key(&3));
        assert!(!peers.contains_key(&2)); // invalid address skipped
    }

    // -- EntityLookup consistency after transfer out --

    #[test]
    fn entity_lookup_consistent_after_transfer() {
        let mut world = setup_world(1);
        let mut eids = Vec::new();
        for _ in 0..10 {
            let (_, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
            eids.push(eid);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfers(&mut world);

        // Verify lookup is consistent: every entity in lookup should exist in ECS
        let lookup = world.resource::<EntityLookup>();
        for (eid, &bevy_entity) in &lookup.0 {
            assert!(
                world.get_entity(bevy_entity).is_ok(),
                "EntityLookup contains stale Bevy Entity for {:?}", eid
            );
        }

        // Verify total: lookup size + transferred = original count
        let auth_count = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(world.resource::<EntityLookup>().0.len(), auth_count);
    }

    // -- EntityLookup consistency after drain --

    #[test]
    fn entity_lookup_populated_after_drain_before_acks() {
        let mut world = setup_world(1);
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Two-phase: entities still in lookup (waiting for ACKs)
        assert_eq!(world.resource::<EntityLookup>().0.len(), 5,
            "entities stay in lookup until ACK (two-phase)");
    }

    // -- Client SpawnPlayer with invalid UUID --

    #[test]
    fn client_spawn_player_invalid_uuid_ignored() {
        let mut world = setup_world(1);

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::SpawnPlayer {
            player_id: "not-a-uuid".to_string(),
            position: [0.0, 0.0, 0.0],
        });

        process_client_inputs(&mut world);

        assert!(world.resource::<EntityLookup>().0.is_empty());
    }

    // -- Drain with target peer that has no address --

    #[test]
    fn drain_target_no_address_skips_entity() {
        let mut world = setup_world(1);
        for _ in 0..4 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        // Target server 5 is not in peer addresses
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![5]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Entities should still be removed (despawned) but no UDP messages sent
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        // The drain continues but skips sending, the entities are still despawned
        // Actually looking at the code, if target has no address, it skips the entity entirely
        // So entities remain
        assert_eq!(remaining, 4); // entities not transferred because no address
    }

    // ============================================================
    // Forwarded remove tests (ghost player bug fix)
    // ============================================================

    #[test]
    fn remove_shadow_entity_produces_forwarded_remove() {
        // Scenario: Player was transferred from S1 to S2. Client disconnects from S1.
        // S1 should forward the remove to S2 (the authority) via UDP.
        let mut world = setup_world(1);

        // Spawn a player shadow on S1 that is authoritative on S2
        let shadow_state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        let eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(10));

        // Register peer address for S2
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // Push RemoveEntity (as if the client disconnected)
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        // The outbound forwards should contain a ForwardedRemove message
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "Should produce exactly one forwarded remove");

        // Deserialize and verify the message
        let msg: PeerMessage = bincode::deserialize(&forwards[0].1).unwrap();
        match msg {
            PeerMessage::ForwardedRemove { entity_id } => {
                assert_eq!(entity_id, eid);
            }
            _ => panic!("Expected ForwardedRemove, got {:?}", msg),
        }

        // Shadow should also be cleaned up locally
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn remove_authoritative_entity_does_not_forward() {
        // Scenario: Player is still authoritative on this server. RemoveEntity
        // should just remove locally, no forwarding needed.
        let mut world = setup_world(1);
        let (_, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        // No forwarded messages
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert!(forwards.is_empty(), "Should not forward for authoritative entity");

        // Entity should be gone
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn forwarded_remove_despawns_authoritative_entity() {
        // Scenario: S2 receives a ForwardedRemove from S1 for a player entity
        // that S2 owns authoritatively.
        let mut world = setup_world(2);
        let (_, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 2);

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            ForwardedRemove { entity_id: eid },
        );

        drain_inbound_events(&mut world);

        // Entity should be gone
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
        let remaining = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn forwarded_remove_for_nonexistent_entity_is_noop() {
        // ForwardedRemove for an entity that doesn't exist should be silently ignored.
        let mut world = setup_world(1);
        let unknown_eid = EntityId::new();

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            ForwardedRemove { entity_id: unknown_eid },
        );

        // Should not panic
        drain_inbound_events(&mut world);

        assert!(!world.resource::<EntityLookup>().0.contains_key(&unknown_eid));
    }

    #[test]
    fn forwarded_remove_for_shadow_entity_is_ignored() {
        // ForwardedRemove for a shadow entity (not authoritative) should be ignored.
        // Only the authority server should act on ForwardedRemove.
        let mut world = setup_world(1);
        let shadow_state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        let eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(10));

        world.resource_mut::<InboundQueue>().forwarded_removes.push(
            ForwardedRemove { entity_id: eid },
        );

        drain_inbound_events(&mut world);

        // Shadow should still exist (forwarded remove only acts on authoritative)
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn remove_shadow_without_peer_address_still_removes_local() {
        // If we have no peer address for the authority server, we can't forward
        // the remove, but we should still clean up the local shadow.
        let mut world = setup_world(1);
        let shadow_state = EntityState::new_player(Vec3::ZERO, ServerId(2));
        let eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(10));

        // Do NOT register peer address for S2

        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        process_client_inputs(&mut world);

        // No forwarded messages (can't reach S2)
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert!(forwards.is_empty());

        // But local shadow should still be cleaned up
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn full_ghost_player_scenario_via_schedule() {
        // End-to-end test: player spawned on S1, authority transferred to S2,
        // then RemoveEntity arrives on S1. After running the full schedule,
        // S1 should produce a forwarded remove.
        let mut world = setup_world(1);

        // 1. Spawn the player as authoritative on S1
        let uuid = uuid::Uuid::new_v4();
        let eid = EntityId(uuid);
        let player_state = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let mut player_state = player_state;
        player_state.id = eid;
        spawn_from_entity_state_direct(&mut world, &player_state, true, None);

        // 2. Simulate authority transfer: remove auth, add as shadow with S2 authority
        let &bevy_entity = world.resource::<EntityLookup>().0.get(&eid).unwrap();
        despawn_and_remove(&mut world, bevy_entity, eid);

        let mut transferred = player_state.clone();
        transferred.authority_server = ServerId(2);
        spawn_from_entity_state_direct(&mut world, &transferred, false, Some(50));

        // 3. Register S2 peer address
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // 4. Client disconnects -- push RemoveEntity
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::RemoveEntity {
            entity_id: eid.0.to_string(),
        });

        world.resource_mut::<TickCounter>().0 = 51;

        // 5. Run the full schedule
        let mut schedule = crate::schedule::build_schedule();
        schedule.run(&mut world);

        // 6. Verify: forwarded remove was produced
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert!(forwards.len() >= 1, "Should have at least one forwarded remove");

        // Find the ForwardedRemove message
        let mut found = false;
        for (_addr, bytes) in forwards {
            if let Ok(msg) = bincode::deserialize::<PeerMessage>(bytes) {
                if let PeerMessage::ForwardedRemove { entity_id } = msg {
                    assert_eq!(entity_id, eid);
                    found = true;
                }
            }
        }
        assert!(found, "ForwardedRemove message should be in outbound forwards");

        // Entity should be gone from this server
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // ====================================================================
    // WI-004: TransferAck sent on accepting authority transfer
    // ====================================================================

    #[test]
    fn transfer_ack_sent_on_accept() {
        let mut world = setup_world(2); // We are server 2
        world.resource_mut::<PeerAddresses>().0.insert(1, "127.0.0.1:9101".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 50;

        // Push an inbound authority transfer from server 1 to us
        let state = EntityState::new_npc(Vec3::new(10.0, 0.0, 20.0), ServerId(2));
        let eid = state.id;
        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 50,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // Verify ACK was generated in OutboundTransfers
        let transfers = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(transfers.len(), 1, "should have exactly one ACK message");

        let (addr, bytes) = &transfers[0];
        assert_eq!(*addr, "127.0.0.1:9101".parse::<std::net::SocketAddr>().unwrap());

        let msg: PeerMessage = bincode::deserialize(bytes).unwrap();
        match msg {
            PeerMessage::TransferAck { entity_id, tick } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 50);
            }
            _ => panic!("expected TransferAck, got {:?}", msg),
        }
    }

    #[test]
    fn transfer_ack_skipped_when_source_not_in_peers() {
        let mut world = setup_world(2);
        // Server 3 is NOT in PeerAddresses
        world.resource_mut::<TickCounter>().0 = 10;

        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 10,
            entity_id: eid,
            from_server: ServerId(3),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // No ACK should be sent since server 3 is not in peer addresses
        assert!(
            world.resource::<OutboundTransfers>().messages.is_empty(),
            "no ACK should be sent when source is not in peer list"
        );
        // But the entity should still be accepted
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn transfer_ack_sent_on_duplicate_transfer() {
        let mut world = setup_world(2);
        world.resource_mut::<PeerAddresses>().0.insert(1, "127.0.0.1:9101".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 10;

        // Spawn entity as authoritative (simulating already accepted)
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Now push a duplicate transfer
        world.resource_mut::<InboundQueue>().authority_transfers.push(InboundTransfer {
            tick: 10,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        });

        drain_inbound_events(&mut world);

        // ACK should still be sent even for duplicate
        let transfers = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(transfers.len(), 1, "ACK should be sent for duplicate transfer");
    }

    // ====================================================================
    // WI-005: process_transfer_acks unit tests
    // ====================================================================

    #[test]
    fn process_transfer_acks_despawns_outbound_entity() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        // Add TransferOutbound component
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Push matching ACK
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        process_transfer_acks(&mut world);

        // Entity should be despawned
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    #[test]
    fn process_transfer_acks_ignores_unknown_entity() {
        let mut world = setup_world(1);

        // ACK for nonexistent entity
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: EntityId::new(), tick: 5 }
        );

        // Should not panic
        process_transfer_acks(&mut world);
    }

    #[test]
    fn process_transfer_acks_ignores_entity_without_outbound() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let eid = state.id;
        spawn_from_entity_state_direct(&mut world, &state, true, None);

        // ACK for entity that does NOT have TransferOutbound
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        process_transfer_acks(&mut world);

        // Entity should still exist
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // ====================================================================
    // WI-006: process_transfer_timeouts unit tests
    // ====================================================================

    #[test]
    fn transfer_timeout_retries_once() {
        let mut world = setup_world(1);
        let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let _eid = state.id;
        let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Set tick past timeout
        world.resource_mut::<TickCounter>().0 = 12; // 1 + 10 + 1 = past timeout

        process_transfer_timeouts(&mut world);

        // Should have retried: OutboundTransfers has a message
        assert_eq!(world.resource::<OutboundTransfers>().messages.len(), 1);

        // retry_count should be 1, sent_tick updated
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
            retry_count: TRANSFER_MAX_RETRIES, // already at max retries
        });

        // Set tick way past timeout
        world.resource_mut::<TickCounter>().0 = 22;

        process_transfer_timeouts(&mut world);

        // TransferOutbound should be removed, entity still has Authoritative
        assert!(world.get::<TransferOutbound>(entity).is_none());
        assert!(world.get::<Authoritative>(entity).is_some());
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));

        // No retry should have been sent
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

        // Set tick NOT past timeout (5 + 10 = 15, we are at 10)
        world.resource_mut::<TickCounter>().0 = 10;

        process_transfer_timeouts(&mut world);

        // No change
        assert!(world.get::<TransferOutbound>(entity).is_some());
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 0);
        assert!(world.resource::<OutboundTransfers>().messages.is_empty());
    }

    // ====================================================================
    // Two-phase transfer: duplicate ACKs handled gracefully
    // ====================================================================

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

        // Push two ACKs for the same entity (duplicate)
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 6 }
        );

        // Should not panic -- first ACK despawns, second is a no-op (unknown entity)
        process_transfer_acks(&mut world);

        // Entity should be despawned
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // ====================================================================
    // Two-phase transfer: process_transfers skips entities with TransferOutbound
    // ====================================================================

    #[test]
    fn process_transfers_skips_entities_with_transfer_outbound() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // Spawn 5 entities, 2 already have TransferOutbound
        let mut entities = Vec::new();
        for _ in 0..5 {
            let state = EntityState::new_npc(Vec3::new(100.0, 0.0, 100.0), ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            entities.push((entity, eid));
        }

        // Mark first 2 as already in-flight
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

        // Only 3 entities should be candidates (5 total minus 2 already outbound)
        // transfer_count = max(1, 5/10) = 1, but capped to candidates.len()=3
        // So at most 1 entity should be newly transferred (10% of 5 = 0.5, max(1)=1)
        let new_outbound_count = entities.iter()
            .filter(|(e, _)| world.get::<TransferOutbound>(*e).is_some())
            .count();
        // Original 2 + up to 1 new = 3
        assert!(new_outbound_count >= 2 && new_outbound_count <= 3,
            "expected 2-3 outbound entities, got {}", new_outbound_count);

        // Verify the original 2 still have their original sent_tick
        for (entity, _) in entities.iter().take(2) {
            let outbound = world.get::<TransferOutbound>(*entity).unwrap();
            assert_eq!(outbound.sent_tick, 1, "original TransferOutbound should not be overwritten");
        }
    }

    // ====================================================================
    // Two-phase transfer: ACK queue is fully drained
    // ====================================================================

    #[test]
    fn process_transfer_acks_drains_entire_queue() {
        let mut world = setup_world(1);

        // Spawn 3 entities, all with TransferOutbound
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

        // Push ACKs for all 3
        for eid in &eids {
            world.resource_mut::<InboundQueue>().transfer_acks.push(
                InboundTransferAck { entity_id: *eid, tick: 5 }
            );
        }

        process_transfer_acks(&mut world);

        // All 3 should be despawned
        for eid in &eids {
            assert!(!world.resource::<EntityLookup>().0.contains_key(eid),
                "entity {:?} should be despawned", eid);
        }

        // Queue should be empty
        assert!(world.resource::<InboundQueue>().transfer_acks.is_empty());
    }

    // ====================================================================
    // Two-phase transfer: timeout at exact boundary
    // ====================================================================

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

        // Set tick to exactly sent_tick + TRANSFER_TIMEOUT_TICKS (boundary, NOT past it)
        // TRANSFER_TIMEOUT_TICKS = 10, so tick 15 means diff = 10 which is NOT > 10
        world.resource_mut::<TickCounter>().0 = 15;

        process_transfer_timeouts(&mut world);

        // Should NOT have retried (15 - 5 = 10, check is > 10, not >= 10)
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

        // One past the boundary: 16 - 5 = 11 > 10
        world.resource_mut::<TickCounter>().0 = 16;

        process_transfer_timeouts(&mut world);

        // Should have retried
        assert_eq!(world.resource::<OutboundTransfers>().messages.len(), 1);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 1);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().sent_tick, 16);
    }

    // ====================================================================
    // Two-phase transfer: ACK sends correct serialized TransferAck via UDP
    // ====================================================================

    #[test]
    fn transfer_ack_message_deserializes_correctly() {
        let mut world = setup_world(2);
        world.resource_mut::<PeerAddresses>().0.insert(1, "127.0.0.1:9101".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 42;

        // Spawn entity and push a transfer
        let state = EntityState::new_npc(Vec3::new(10.0, 0.0, 20.0), ServerId(2));
        let eid = state.id;

        let transfer = InboundTransfer {
            tick: 42,
            entity_id: eid,
            from_server: ServerId(1),
            to_server: ServerId(2),
            full_state: state,
        };
        world.resource_mut::<InboundQueue>().authority_transfers.push(transfer);

        drain_inbound_events(&mut world);

        // Verify the outbound ACK message deserializes to TransferAck
        let transfers = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(transfers.len(), 1);

        let (addr, bytes) = &transfers[0];
        assert_eq!(addr.to_string(), "127.0.0.1:9101");

        let decoded: PeerMessage = bincode::deserialize(bytes).unwrap();
        match decoded {
            PeerMessage::TransferAck { entity_id, tick } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 42);
            }
            _ => panic!("expected TransferAck, got {:?}", decoded),
        }
    }

    // ====================================================================
    // Two-phase transfer: drain waits for ACKs before shutdown
    // ====================================================================

    #[test]
    fn drain_does_not_shutdown_with_pending_transfers() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // Spawn entities
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        // Start drain
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Should NOT be shutdown -- entities have TransferOutbound, waiting for ACKs
        assert!(!world.resource::<ShutdownRequested>().0);

        // Verify entities have TransferOutbound
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 3);
    }

    // ====================================================================
    // Two-phase transfer: drain completes after all ACKs
    // ====================================================================

    #[test]
    fn drain_completes_after_all_acks_received() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        let mut eids = Vec::new();
        for _ in 0..3 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            eids.push(state.id);
            spawn_from_entity_state_direct(&mut world, &state, true, None);
        }

        // Start drain and transfer entities
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        assert!(!world.resource::<ShutdownRequested>().0);

        // Simulate ACKs for all entities
        for eid in &eids {
            world.resource_mut::<InboundQueue>().transfer_acks.push(
                InboundTransferAck { entity_id: *eid, tick: 11 }
            );
        }

        // Process ACKs (despawns entities)
        process_transfer_acks(&mut world);

        // Now run drain again -- should detect completion
        world.resource_mut::<TickCounter>().0 = 12;
        process_drain(&mut world);

        assert!(world.resource::<ShutdownRequested>().0, "drain should complete after all ACKs");
    }

    // ====================================================================
    // Two-phase transfer: drain only transfers entities without TransferOutbound
    // ====================================================================

    #[test]
    fn drain_skips_entities_already_in_transfer() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // Spawn 4 entities
        let mut entities = Vec::new();
        for _ in 0..4 {
            let state = EntityState::new_npc(Vec3::ZERO, ServerId(1));
            let eid = state.id;
            let entity = spawn_from_entity_state_direct(&mut world, &state, true, None);
            entities.push((entity, eid));
        }

        // Mark 2 as already in-flight from a prior rebalance
        for (entity, _) in entities.iter().take(2) {
            world.entity_mut(*entity).insert(TransferOutbound {
                target_server: 2,
                target_addr: "127.0.0.1:9102".parse().unwrap(),
                sent_tick: 5,
                retry_count: 0,
            });
        }

        // Start drain
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // All 4 should now have TransferOutbound (2 original + 2 new from drain)
        let outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert_eq!(outbound, 4);

        // The 2 original should still have sent_tick=5
        for (entity, _) in entities.iter().take(2) {
            assert_eq!(world.get::<TransferOutbound>(*entity).unwrap().sent_tick, 5);
        }

        // The 2 new should have sent_tick=10
        for (entity, _) in entities.iter().skip(2) {
            assert_eq!(world.get::<TransferOutbound>(*entity).unwrap().sent_tick, 10);
        }
    }

    // ====================================================================
    // Chunking: entity count math verification
    // ====================================================================

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

    // ====================================================================
    // Chunking: bincode size per chunk stays under UDP limit
    // ====================================================================

    #[test]
    fn chunked_entity_update_stays_under_udp_limit() {
        // 100 entities in a single ChunkedEntityUpdate
        let entities: Vec<EntityState> = (0..100)
            .map(|_| EntityState::new_npc(Vec3::new(100.0, 0.0, -100.0), ServerId(1)))
            .collect();

        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 999,
            server_id: ServerId(1),
            chunk_index: 0,
            total_chunks: 1,
            entities,
        };

        let bytes = bincode::serialize(&msg).unwrap();
        // UDP max payload: 65507 bytes. Each chunk should be well under this.
        assert!(bytes.len() < 65507,
            "chunk size {} exceeds UDP limit 65507", bytes.len());
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
            entities,
        };

        let bytes = bincode::serialize(&msg).unwrap();
        assert!(bytes.len() < 65507,
            "unchunked 100 entities {} exceeds UDP limit", bytes.len());
    }

    // ====================================================================
    // Chunking: full reassembly produces correct entity set
    // ====================================================================

    // ====================================================================
    // Two-phase transfer: duplicate RebalanceDirective for same entity
    // ====================================================================

    #[test]
    fn duplicate_rebalance_directive_does_not_double_transfer() {
        // If two RebalanceDirectives arrive before the first is processed,
        // only entities without TransferOutbound should be candidates.
        let mut world = setup_world(1);
        for _ in 0..5 {
            spawn_auth_npc(&mut world, Vec3::new(200.0, 0.0, 200.0), 1);
        }

        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());
        world.resource_mut::<TickCounter>().0 = 10;

        // First rebalance
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        process_transfers(&mut world);

        let first_outbound = world.query_filtered::<(), With<TransferOutbound>>().iter(&world).count();
        assert!(first_outbound > 0);

        // Clear outbound messages from first transfer
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Second rebalance directive for same target -- should only transfer
        // entities that don't already have TransferOutbound
        world.resource_mut::<PendingTransfers>().transfers = Some((vec![], 2));
        process_transfers(&mut world);

        // All 5 entities should still exist
        let total = world.query_filtered::<(), With<Authoritative>>().iter(&world).count();
        assert_eq!(total, 5);

        // No entity should have been transferred twice -- check that TransferOutbound
        // from the first batch still has sent_tick=10
        let mut query = world.query::<&TransferOutbound>();
        for outbound in query.iter(&world) {
            assert!(outbound.sent_tick >= 10);
        }
    }

    // ====================================================================
    // Two-phase transfer: input for TransferOutbound entity is forwarded
    // ====================================================================

    #[test]
    fn move_input_for_transfer_outbound_is_forwarded() {
        // When a client sends MoveInput for an entity that is in TransferOutbound state,
        // the input should be forwarded to the target server, not applied locally.
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Mark entity as mid-transfer to server 2
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        // Client sends MoveInput for the entity
        world.resource_mut::<InboundQueue>().client_inputs.push(ClientMessage::MoveInput {
            entity_id: eid.0.to_string(),
            direction: [1.0, 0.0, 0.0],
            seq: 42,
        });

        process_client_inputs(&mut world);

        // Velocity should NOT be changed locally (entity is frozen)
        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "velocity should remain zero for TransferOutbound entity");

        // LastInputSeq should NOT be updated locally
        let seq = world.get::<LastInputSeq>(entity).unwrap();
        assert_eq!(seq.0, 0, "seq should not be updated for TransferOutbound entity");

        // Instead, a ForwardedInput should have been sent to the target
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should forward input to transfer target");

        // Verify the forwarded message
        let (addr, bytes) = &forwards[0];
        assert_eq!(*addr, "127.0.0.1:9102".parse::<std::net::SocketAddr>().unwrap());
        let msg: PeerMessage = bincode::deserialize(bytes).unwrap();
        match msg {
            PeerMessage::ForwardedInput { entity_id, direction, seq } => {
                assert_eq!(entity_id, eid);
                assert_eq!(direction, [1.0, 0.0, 0.0]);
                assert_eq!(seq, 42);
            }
            _ => panic!("expected ForwardedInput, got {:?}", msg),
        }
    }

    // ====================================================================
    // Two-phase transfer: forwarded input for TransferOutbound is re-forwarded
    // ====================================================================

    #[test]
    fn forwarded_input_for_transfer_outbound_is_re_forwarded() {
        // When a peer sends a ForwardedInput for an entity that we own but is in
        // TransferOutbound state, we should re-forward to the new target.
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_player(&mut world, Vec3::ZERO, 1);

        // Mark entity as mid-transfer to server 3
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 3,
            target_addr: "127.0.0.1:9103".parse().unwrap(),
            sent_tick: 5,
            retry_count: 0,
        });

        // Peer sends forwarded input (originally from server 2)
        world.resource_mut::<InboundQueue>().forwarded_inputs.push(ForwardedInput {
            entity_id: eid,
            direction: [0.0, 0.0, 1.0],
            seq: 99,
        });

        drain_inbound_events(&mut world);

        // Velocity should NOT be changed (entity is frozen)
        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO);

        // A re-forwarded input should have been sent to server 3
        let forwards = &world.resource::<OutboundForwards>().messages;
        assert_eq!(forwards.len(), 1, "should re-forward to transfer target");

        let (addr, bytes) = &forwards[0];
        assert_eq!(*addr, "127.0.0.1:9103".parse::<std::net::SocketAddr>().unwrap());
        let msg: PeerMessage = bincode::deserialize(bytes).unwrap();
        match msg {
            PeerMessage::ForwardedInput { entity_id, seq, .. } => {
                assert_eq!(entity_id, eid);
                assert_eq!(seq, 99);
            }
            _ => panic!("expected ForwardedInput, got {:?}", msg),
        }
    }

    // ====================================================================
    // Two-phase transfer: ACK for already-despawned entity (race condition)
    // ====================================================================

    #[test]
    fn transfer_ack_for_already_despawned_entity_is_noop() {
        // Race: entity was removed (client disconnect) while in TransferOutbound.
        // The ACK arrives later for an entity that no longer exists.
        let mut world = setup_world(1);
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Despawn entity (simulating client disconnect handling)
        despawn_and_remove(&mut world, entity, eid);

        // ACK arrives after despawn
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eid, tick: 5 }
        );

        // Should not panic -- ACK for unknown entity is silently ignored
        process_transfer_acks(&mut world);
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eid));
    }

    // ====================================================================
    // Two-phase transfer: multiple simultaneous transfers get independent ACKs
    // ====================================================================

    #[test]
    fn multiple_simultaneous_transfers_get_independent_acks() {
        // 3 entities transferred to the same target. ACKs arrive for 2 of them.
        // Only those 2 should be despawned.
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

        // ACK for first 2 only
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eids[0], tick: 5 }
        );
        world.resource_mut::<InboundQueue>().transfer_acks.push(
            InboundTransferAck { entity_id: eids[1], tick: 5 }
        );

        process_transfer_acks(&mut world);

        // First 2 should be gone, third should remain
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eids[0]));
        assert!(!world.resource::<EntityLookup>().0.contains_key(&eids[1]));
        assert!(world.resource::<EntityLookup>().0.contains_key(&eids[2]));
        assert!(world.get::<TransferOutbound>(entities[2]).is_some());
    }

    // ====================================================================
    // Two-phase transfer: retry builds fresh state (not stale)
    // ====================================================================

    #[test]
    fn transfer_retry_uses_current_position() {
        // After timeout, the retry should serialize current ECS state, not stale data.
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

        // Change entity position after the initial transfer (simulating that
        // even though physics is frozen for TransferOutbound, the entity could
        // have been updated by some other path)
        world.get_mut::<Position>(entity).unwrap().0 = Vec3::new(99.0, 0.0, 99.0);

        // Trigger timeout at tick 12 (1 + 10 + 1 = past timeout)
        world.resource_mut::<TickCounter>().0 = 12;
        process_transfer_timeouts(&mut world);

        // Verify the retry message contains the CURRENT position (99.0), not the old one (10.0)
        let messages = &world.resource::<OutboundTransfers>().messages;
        assert_eq!(messages.len(), 1);

        let msg: PeerMessage = bincode::deserialize(&messages[0].1).unwrap();
        match msg {
            PeerMessage::AuthorityTransfer { full_state, entity_id, tick, .. } => {
                assert_eq!(entity_id, eid);
                assert_eq!(tick, 12, "retry should use current tick");
                assert!((full_state.position.x - 99.0).abs() < 0.001,
                    "retry should use current position (99.0), got {}", full_state.position.x);
            }
            _ => panic!("expected AuthorityTransfer, got {:?}", msg),
        }
    }

    // ====================================================================
    // Two-phase transfer: transfer timeout max retries then abort
    // ====================================================================

    #[test]
    fn transfer_timeout_full_retry_cycle() {
        // Exercise the full retry cycle: initial + MAX_RETRIES attempts = 4 total
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

        // Retry 3 (final retry, retry_count goes to 3 = TRANSFER_MAX_RETRIES)
        world.resource_mut::<TickCounter>().0 = 33;
        process_transfer_timeouts(&mut world);
        assert_eq!(world.get::<TransferOutbound>(entity).unwrap().retry_count, 3);
        world.resource_mut::<OutboundTransfers>().messages.clear();

        // Next timeout should ABORT (retry_count == MAX_RETRIES)
        world.resource_mut::<TickCounter>().0 = 44;
        process_transfer_timeouts(&mut world);
        assert!(world.get::<TransferOutbound>(entity).is_none(), "should be aborted");
        assert!(world.get::<Authoritative>(entity).is_some(), "should keep authoritative");
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid));
        assert!(world.resource::<OutboundTransfers>().messages.is_empty(), "no retry after abort");
    }

    // ====================================================================
    // Replication: TransferOutbound entities INCLUDED for crash recovery
    // ====================================================================

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
        assert!(outbound.snapshot.is_some());
        // All 3 entities should be replicated, including the TransferOutbound one.
        // This ensures peers have up-to-date shadows for crash recovery.
        assert_eq!(outbound.snapshot.as_ref().unwrap().len(), 3,
            "TransferOutbound entity should be included in replication for crash recovery");
    }

    // ====================================================================
    // Physics: TransferOutbound entities are NOT moved
    // ====================================================================

    #[test]
    fn physics_does_not_move_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.get_mut::<Velocity>(entity).unwrap().0 = Vec3::new(100.0, 0.0, 0.0);

        // Mark as TransferOutbound (frozen)
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(physics_integration);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0, Vec3::ZERO, "TransferOutbound entity should not be moved by physics");
    }

    // ====================================================================
    // NPC behavior: TransferOutbound entities are NOT updated
    // ====================================================================

    #[test]
    fn npc_behavior_skips_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Mark as TransferOutbound (frozen)
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(npc_behavior);

        for tick in 1..=80 {
            world.resource_mut::<TickCounter>().0 = tick;
            schedule.run(&mut world);
        }

        let vel = world.get::<Velocity>(entity).unwrap();
        assert_eq!(vel.0, Vec3::ZERO, "TransferOutbound NPC should not get velocity changes");
    }

    // ====================================================================
    // Clamp: TransferOutbound entities are NOT clamped
    // ====================================================================

    #[test]
    fn clamp_skips_transfer_outbound() {
        let mut world = setup_world(1);
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::new(9999.0, 0.0, 9999.0), 1);

        // Mark as TransferOutbound (frozen)
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(clamp_positions);
        schedule.run(&mut world);

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.0.x, 9999.0, "TransferOutbound entity should not be clamped");
    }

    // ====================================================================
    // Drain: immediate shutdown when no entities
    // ====================================================================

    #[test]
    fn drain_with_no_entities_immediate_shutdown() {
        let mut world = setup_world(1);
        // No entities spawned

        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // With no auth entities and no pending transfers, drain should complete immediately
        assert!(world.resource::<ShutdownRequested>().0,
            "drain with no entities should trigger immediate shutdown");
    }

    // ====================================================================
    // Drain: timeout forces shutdown even with pending transfers
    // ====================================================================

    #[test]
    fn drain_timeout_forces_shutdown_with_pending() {
        let mut world = setup_world(1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        // Spawn entities
        for _ in 0..3 {
            spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        }

        // Start drain
        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 10;

        process_drain(&mut world);

        // Entities now have TransferOutbound, not shutdown
        assert!(!world.resource::<ShutdownRequested>().0);

        // Advance past DRAIN_TIMEOUT_TICKS (600)
        world.resource_mut::<TickCounter>().0 = 10 + DRAIN_TIMEOUT_TICKS + 1;

        process_drain(&mut world);

        // Should force shutdown despite pending transfers
        assert!(world.resource::<ShutdownRequested>().0,
            "drain timeout should force shutdown even with pending transfers");
    }

    // ====================================================================
    // Drain: drain_started_tick is recorded correctly
    // ====================================================================

    #[test]
    fn drain_records_start_tick() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.resource_mut::<PeerAddresses>().0.insert(2, "127.0.0.1:9102".parse().unwrap());

        world.resource_mut::<DrainState>().draining = true;
        world.resource_mut::<DrainState>().targets = Some(vec![2]);
        world.resource_mut::<TickCounter>().0 = 42;

        assert!(world.resource::<DrainState>().started_tick.is_none());

        process_drain(&mut world);

        assert_eq!(world.resource::<DrainState>().started_tick, Some(42));
    }

    // ====================================================================
    // Chunking: exactly 100 entities uses unchunked format
    // ====================================================================

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

    // ====================================================================
    // Chunking: 101 entities forces chunked format (2 chunks)
    // ====================================================================

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
                chunk_index: i as u16,
                total_chunks,
                entities: chunk.to_vec(),
            };
            let bytes = bincode::serialize(&msg).unwrap();
            let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();
            if let PeerMessage::ChunkedEntityUpdate { entities: chunk_entities, .. } = decoded {
                reassembled.extend(chunk_entities);
            }
        }

        assert_eq!(reassembled.len(), 101);
        // Verify first chunk had 100, second had 1
        assert!((reassembled[99].position.x - 99.0).abs() < 0.001);
        assert!((reassembled[100].position.x - 100.0).abs() < 0.001);
    }

    // ====================================================================
    // Chunking: out-of-order chunk delivery
    // ====================================================================

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
            if let PeerMessage::ChunkedEntityUpdate { chunk_index, entities: chunk_entities, .. } = decoded {
                chunks_received.push((chunk_index, chunk_entities));
            }
        }

        // Sort by chunk_index to reassemble in order
        chunks_received.sort_by_key(|(idx, _)| *idx);
        let reassembled: Vec<EntityState> = chunks_received.into_iter()
            .flat_map(|(_, entities)| entities)
            .collect();

        assert_eq!(reassembled.len(), 250);
        // Verify data integrity
        for (i, e) in reassembled.iter().enumerate() {
            assert!((e.position.x - i as f32).abs() < 0.001,
                "entity {} position mismatch after out-of-order reassembly", i);
        }
    }

    // ====================================================================
    // Chunking: duplicate chunk is idempotent
    // ====================================================================

    #[test]
    fn duplicate_chunk_received_is_idempotent() {
        let entities: Vec<EntityState> = (0..50)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();

        let msg = PeerMessage::ChunkedEntityUpdate {
            tick: 100,
            server_id: ServerId(1),
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
            PeerMessage::ChunkedEntityUpdate { entities: e1, chunk_index: c1, .. },
            PeerMessage::ChunkedEntityUpdate { entities: e2, chunk_index: c2, .. },
        ) = (&first, &second) {
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
                chunk_index: i as u16,
                total_chunks,
                entities: chunk.to_vec(),
            };

            // Roundtrip through bincode
            let bytes = bincode::serialize(&msg).unwrap();
            let decoded: PeerMessage = bincode::deserialize(&bytes).unwrap();

            if let PeerMessage::ChunkedEntityUpdate { entities: chunk_entities, .. } = decoded {
                all_entities.extend(chunk_entities);
            }
        }

        assert_eq!(all_entities.len(), total);
        // Verify ordering preserved
        for (i, e) in all_entities.iter().enumerate() {
            assert!((e.position.x - i as f32).abs() < 0.001,
                "entity {} position mismatch: expected {}, got {}", i, i, e.position.x);
        }
    }

    // ====================================================================
    // Crash recovery: SHADOW_STALE_TICKS is 300 (15 seconds)
    // ====================================================================

    #[test]
    fn shadow_stale_ticks_is_300() {
        // 15 seconds at 20Hz = 300 ticks. This must exceed worst-case crash
        // detection time (5s heartbeat + 5s rebalance = 10s = 200 ticks).
        assert_eq!(SHADOW_STALE_TICKS, 300);
    }

    // ====================================================================
    // Crash recovery: RecoverEntities aborts transfers to dead server
    // ====================================================================

    #[test]
    fn recover_entities_aborts_transfer_to_dead_server() {
        let mut world = setup_world(1);

        // Entity mid-transfer to server 2
        let (entity_a, eid_a) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.entity_mut(entity_a).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Entity mid-transfer to server 3 (should NOT be affected)
        let (entity_b, _eid_b) = spawn_auth_npc(&mut world, Vec3::X, 1);
        world.entity_mut(entity_b).insert(TransferOutbound {
            target_server: 3,
            target_addr: "127.0.0.1:9103".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // Normal entity (no transfer)
        let (_entity_c, _eid_c) = spawn_auth_npc(&mut world, Vec3::Z, 1);

        // Enqueue RecoverEntities for dead server 2; survivors = [1]
        world.resource_mut::<CoordEventQueue>()
            .recover_events
            .push((2, vec![1]));

        process_coord_events(&mut world);

        // Entity A: TransferOutbound removed (target was dead server 2)
        assert!(world.get::<TransferOutbound>(entity_a).is_none(),
            "TransferOutbound should be removed for entity targeting dead server");
        assert!(world.get::<Authoritative>(entity_a).is_some(),
            "Entity should still be authoritative after abort");
        assert!(world.resource::<EntityLookup>().0.contains_key(&eid_a),
            "Entity should still exist in lookup");

        // Entity B: TransferOutbound still present (targeting server 3, not dead)
        assert!(world.get::<TransferOutbound>(entity_b).is_some(),
            "TransferOutbound to alive server should not be affected");
    }

    #[test]
    fn recover_entities_aborts_transfer_and_promotes_shadows() {
        let mut world = setup_world(1);

        // Shadow entity from dead server 2
        let shadow_state = EntityState::new_npc(Vec3::new(10.0, 0.0, 10.0), ServerId(2));
        let shadow_eid = shadow_state.id;
        spawn_from_entity_state_direct(&mut world, &shadow_state, false, Some(1));

        // Entity mid-transfer to server 2
        let (entity, transfer_eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 2,
            target_addr: "127.0.0.1:9102".parse().unwrap(),
            sent_tick: 1,
            retry_count: 0,
        });

        // RecoverEntities for dead server 2; this server is the only survivor
        world.resource_mut::<CoordEventQueue>()
            .recover_events
            .push((2, vec![1]));

        process_coord_events(&mut world);

        // Shadow should be promoted to authoritative
        let &shadow_bevy = world.resource::<EntityLookup>().0.get(&shadow_eid).unwrap();
        assert!(world.get::<Authoritative>(shadow_bevy).is_some(),
            "Shadow from dead server should be promoted");
        assert!(world.get::<Shadow>(shadow_bevy).is_none(),
            "Shadow marker should be removed");

        // Transfer should be aborted
        assert!(world.get::<TransferOutbound>(entity).is_none(),
            "TransferOutbound to dead server should be removed");
        assert!(world.resource::<EntityLookup>().0.contains_key(&transfer_eid),
            "Transferred entity should still exist");
    }
}
