use crate::components::*;
use crate::resources::*;
use bevy_ecs::prelude::*;

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
        {
            let mut addrs = world.resource_mut::<PeerAddresses>();
            addrs.0.clear();
            for peer in &peers {
                // Try direct parse first (IP:port). DNS resolution is not
                // available in sync context so we only use direct parse here.
                let addr: Option<std::net::SocketAddr> = peer.udp_addr.parse().ok();
                if let Some(addr) = addr {
                    addrs.0.insert(peer.server_id.0, addr);
                } else {
                    tracing::warn!("Could not parse peer address: {}", peer.udp_addr);
                }
            }
        }
        // WI-202: populate PeerWsAddresses with full URLs (ws:// or wss://)
        // so process_transfers can reach them when sending Reconnect during
        // player migration. Cleared + repopulated each PeerList update so
        // departed peers are pruned automatically.
        {
            let mut ws_addrs = world.resource_mut::<PeerWsAddresses>();
            ws_addrs.0.clear();
            for peer in &peers {
                let scheme = if peer.tls { "wss" } else { "ws" };
                let url = format!("{scheme}://{}", peer.ws_addr);
                ws_addrs.0.insert(peer.server_id.0, url);
            }
        }
        // WI-213: publish the new active-peer set into the shared `ActivePeers`
        // resource. The UDP receiver reads this in its periodic sweep to prune
        // per-peer state (`session_of_peer`, `session_history`, `latest_tick`)
        // for departed peers. Without this write, those maps grow unbounded
        // over the server's lifetime.
        //
        // Clear + repopulate (rather than diff) so the semantics are "this is
        // the complete current set". Empty-peers case is legal — a sole
        // remaining server briefly has no peers; the receiver's "only prune
        // when non-empty" gate handles that correctly.
        {
            let active = world.resource::<ActivePeers>().0.clone();
            let mut guard = active.write().unwrap_or_else(|e| e.into_inner());
            guard.clear();
            guard.extend(peers.iter().map(|p| p.server_id.0));
        }
        tracing::info!(
            "Peer list updated: {} peers {:?}",
            peers.len(),
            peers
                .iter()
                .map(|p| format!("{}", p.server_id))
                .collect::<Vec<_>>()
        );
    }

    // RebalanceDirective
    for (entity_ids, target_server) in rebalance_directives {
        world.resource_mut::<PendingTransfers>().transfers = Some((entity_ids, target_server));
        tracing::info!("Received rebalance directive -> {target_server}");
    }

    // DrainDirective
    for target_servers in drain_directives {
        let mut drain = world.resource_mut::<DrainState>();
        drain.0 = Some(DrainInfo {
            targets: target_servers.clone(),
            started_tick: None,
        });
        tracing::info!(
            "Received drain directive -> {:?}",
            target_servers
                .iter()
                .map(|s| format!("{s}"))
                .collect::<Vec<_>>()
        );
    }

    // RecoverEntities
    for (dead_server_id, survivors) in recover_events {
        let my_index = survivors.iter().position(|&id| id == server_id);
        let my_index = match my_index {
            Some(i) => i,
            None => continue,
        };
        let survivor_count = survivors.len();

        // Find shadow entities from the dead server that hash to us
        let mut to_promote: Vec<Entity> = Vec::new();
        let mut query =
            world.query_filtered::<(Entity, &NetworkId, &AuthorityServer), With<Shadow>>();
        for (entity, nid, auth) in query.iter(&world) {
            if auth.0 == dead_server_id {
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
            tracing::info!(
                "Recovered {} entities from dead {}",
                adopted,
                dead_server_id
            );
        }

        // Abort any in-progress transfers targeting the dead server.
        // The transfer target is gone -- waiting for an ACK that will never come is pointless.
        // Remove TransferOutbound so the entity unfreezes and resumes normal operation.
        let mut to_unfreeze: Vec<Entity> = Vec::new();
        let mut transfer_query =
            world.query_filtered::<(Entity, &NetworkId, &TransferOutbound), With<Authoritative>>();
        for (entity, nid, outbound) in transfer_query.iter(&world) {
            if outbound.target_server == dead_server_id.0 {
                tracing::info!(
                    "Aborting transfer for entity {} to dead server {}",
                    nid.0,
                    dead_server_id
                );
                to_unfreeze.push(entity);
            }
        }
        for entity in to_unfreeze {
            world.entity_mut(entity).remove::<TransferOutbound>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::spawn_from_entity_state_direct;
    use bevy_ecs::world::World;
    use glam::Vec3;
    use shared::entity::ServerId as SId;
    use shared::entity::{EntityId, EntityState, ServerId};
    use shared::protocol::PeerInfo;

    fn setup_world(server_id: u16) -> World {
        let mut world = World::default();
        world.insert_resource(ServerConfig {
            server_id: ServerId(server_id),
        });
        world.insert_resource(SessionId(0));
        world.init_resource::<TickCounter>();
        world.init_resource::<PeerAddresses>();
        world.init_resource::<PeerWsAddresses>();
        // WI-213: ActivePeers is cloned to share with the receiver task in
        // production; tests only exercise the write side, so Default is fine.
        world.init_resource::<ActivePeers>();
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

    fn spawn_shadow_npc(world: &mut World, pos: Vec3, owner_server: u16) -> (Entity, EntityId) {
        let state = EntityState::new_npc(pos, ServerId(owner_server));
        let eid = state.id;
        let entity = spawn_from_entity_state_direct(world, &state, false, Some((0, 0)));
        (entity, eid)
    }

    // -- process_coord_events tests --

    #[test]
    fn coord_peer_list_updates_addresses() {
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![PeerInfo {
                server_id: SId(2),
                udp_addr: "127.0.0.1:9102".into(),
                ws_addr: "127.0.0.1:7879".into(),
                tls: false,
            }]);

        process_coord_events(&mut world);

        assert!(world.resource::<PeerAddresses>().0.contains_key(&2));
    }

    #[test]
    fn coord_rebalance_sets_pending() {
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .rebalance_directives
            .push((vec![EntityId::new()], ServerId(2)));

        process_coord_events(&mut world);

        assert!(world.resource::<PendingTransfers>().transfers.is_some());
    }

    #[test]
    fn coord_drain_sets_state() {
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .drain_directives
            .push(vec![ServerId(2), ServerId(3)]);

        process_coord_events(&mut world);

        assert!(world.resource::<DrainState>().0.is_some());
    }

    #[test]
    fn coord_recover_promotes_correct_subset() {
        let mut world = setup_world(1);
        // Spawn enough shadows so it's statistically near-impossible for all to
        // hash away from server 1 (probability = (1/2)^10 < 0.1%).
        let mut eids = Vec::new();
        for i in 0..10 {
            let pos = Vec3::new(i as f32, 0.0, 0.0);
            let (_, eid) = spawn_shadow_npc(&mut world, pos, 3);
            eids.push(eid);
        }

        // Server 3 is dead, survivors are [1, 2]
        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1), ServerId(2)]));

        process_coord_events(&mut world);

        // At least one entity should be promoted (hash determines which)
        let lookup = world.resource::<EntityLookup>();
        let promoted = eids
            .iter()
            .filter(|eid| {
                let bevy_entity = lookup.0[eid];
                world.get::<Authoritative>(bevy_entity).is_some()
            })
            .count();
        assert!(
            promoted > 0,
            "at least one shadow should be promoted on recovery"
        );
    }

    // -- Coord events: unparseable peer address --

    #[test]
    fn coord_peer_list_unparseable_address_skipped() {
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(2),
                    udp_addr: "not-a-valid-address".into(),
                    ws_addr: "127.0.0.1:7879".into(),
                    tls: false,
                },
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
            ]);

        process_coord_events(&mut world);

        let addrs = world.resource::<PeerAddresses>();
        assert!(
            !addrs.0.contains_key(&2),
            "unparseable address should be skipped"
        );
        assert!(addrs.0.contains_key(&3), "valid address should be added");
    }

    // -- Multiple coord events on same tick --

    #[test]
    fn multiple_coord_events_same_tick() {
        let mut world = setup_world(1);

        // Two peer list updates (second should overwrite first)
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![PeerInfo {
                server_id: SId(2),
                udp_addr: "127.0.0.1:9102".into(),
                ws_addr: "127.0.0.1:7879".into(),
                tls: false,
            }]);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
                PeerInfo {
                    server_id: SId(4),
                    udp_addr: "127.0.0.1:9104".into(),
                    ws_addr: "127.0.0.1:7881".into(),
                    tls: false,
                },
            ]);

        process_coord_events(&mut world);

        let addrs = world.resource::<PeerAddresses>();
        // Second update overwrites first
        assert!(
            !addrs.0.contains_key(&2),
            "first update overwritten by second"
        );
        assert!(addrs.0.contains_key(&3));
        assert!(addrs.0.contains_key(&4));
    }

    // -- Crash recovery: single survivor gets all entities --

    #[test]
    fn recover_single_survivor_gets_all() {
        let mut world = setup_world(1);
        let (_, eid1) = spawn_shadow_npc(&mut world, Vec3::ZERO, 3);
        let (_, eid2) = spawn_shadow_npc(&mut world, Vec3::X, 3);
        let (_, eid3) = spawn_shadow_npc(&mut world, Vec3::Z, 3);

        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1)]));

        process_coord_events(&mut world);

        let lookup = world.resource::<EntityLookup>();
        for eid in [eid1, eid2, eid3] {
            let bevy_entity = lookup.0[&eid];
            assert!(
                world.get::<Authoritative>(bevy_entity).is_some(),
                "single survivor should adopt ALL shadows from dead server"
            );
        }
    }

    // -- Crash recovery: dead server with no shadows --

    #[test]
    fn recover_no_shadows_from_dead_server() {
        let mut world = setup_world(1);
        // Shadows from server 2, but recovery is for server 3
        spawn_shadow_npc(&mut world, Vec3::ZERO, 2);

        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1)]));

        process_coord_events(&mut world);

        // The shadow from server 2 should NOT be promoted
        let mut query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(
            query.iter(&world).count(),
            0,
            "no shadows from dead server, none promoted"
        );
    }

    // -- Crash recovery: we are not in survivor list --

    #[test]
    fn recover_not_in_survivor_list_noop() {
        let mut world = setup_world(1);
        spawn_shadow_npc(&mut world, Vec3::ZERO, 3);

        // Survivors are [2, 4] -- we are server 1, not in list
        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(2), ServerId(4)]));

        process_coord_events(&mut world);

        let mut query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(
            query.iter(&world).count(),
            0,
            "not in survivor list, should not promote anything"
        );
    }

    // -- Crash recovery: promoted entities get our server_id --

    #[test]
    fn recover_promoted_entity_gets_our_server_id() {
        let mut world = setup_world(1);
        let (_, eid) = spawn_shadow_npc(&mut world, Vec3::ZERO, 3);

        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1)]));

        process_coord_events(&mut world);

        let lookup = world.resource::<EntityLookup>();
        let bevy_entity = lookup.0[&eid];
        assert_eq!(
            world.get::<AuthorityServer>(bevy_entity).unwrap().0,
            ServerId(1),
            "promoted entity should have our server_id"
        );
    }

    // -- Duplicate rebalance directive --

    #[test]
    fn coord_duplicate_rebalance_last_directive_wins() {
        let mut world = setup_world(1);
        spawn_auth_npc(&mut world, Vec3::ZERO, 1);

        // Push two rebalance directives
        world
            .resource_mut::<CoordEventQueue>()
            .rebalance_directives
            .push((vec![EntityId::new()], ServerId(2)));
        world
            .resource_mut::<CoordEventQueue>()
            .rebalance_directives
            .push((vec![EntityId::new(), EntityId::new()], ServerId(3)));

        process_coord_events(&mut world);

        // Last directive wins (each overwrites PendingTransfers)
        let pending = world.resource::<PendingTransfers>();
        let (ids, target) = pending.transfers.as_ref().unwrap();
        assert_eq!(*target, ServerId(3));
        assert_eq!(ids.len(), 2);
    }

    // -- WI-202: PeerWsAddresses populated from PeerInfo.tls --

    #[test]
    fn coord_peer_list_populates_ws_urls() {
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(2),
                    udp_addr: "192.168.0.173:9102".into(),
                    ws_addr: "192.168.0.173:7879".into(),
                    tls: true,
                },
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
            ]);

        process_coord_events(&mut world);

        let ws_addrs = world.resource::<PeerWsAddresses>();
        assert_eq!(
            ws_addrs.0.get(&2).map(|s| s.as_str()),
            Some("wss://192.168.0.173:7879"),
            "tls=true peer should produce wss:// url"
        );
        assert_eq!(
            ws_addrs.0.get(&3).map(|s| s.as_str()),
            Some("ws://127.0.0.1:7880"),
            "tls=false peer should produce ws:// url"
        );
    }

    #[test]
    fn coord_peer_list_prunes_ws_urls_for_departed_peers() {
        // First PeerList introduces S2 and S3.
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(2),
                    udp_addr: "127.0.0.1:9102".into(),
                    ws_addr: "127.0.0.1:7879".into(),
                    tls: false,
                },
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
            ]);
        process_coord_events(&mut world);
        assert_eq!(world.resource::<PeerWsAddresses>().0.len(), 2);

        // Second PeerList drops S3. The resource should be cleared +
        // repopulated so the stale S3 entry is pruned.
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![PeerInfo {
                server_id: SId(2),
                udp_addr: "127.0.0.1:9102".into(),
                ws_addr: "127.0.0.1:7879".into(),
                tls: false,
            }]);
        process_coord_events(&mut world);
        let ws_addrs = world.resource::<PeerWsAddresses>();
        assert_eq!(ws_addrs.0.len(), 1);
        assert!(ws_addrs.0.contains_key(&2));
        assert!(!ws_addrs.0.contains_key(&3));
    }

    // -- WI-213: ActivePeers populated from PeerList --

    #[test]
    fn coord_peer_list_populates_active_peers() {
        // On the first PeerList, the shared `ActivePeers` resource must be
        // populated with the set of peer server_ids so the UDP receiver
        // can subsequently prune state for departed peers.
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(2),
                    udp_addr: "127.0.0.1:9102".into(),
                    ws_addr: "127.0.0.1:7879".into(),
                    tls: false,
                },
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
            ]);

        process_coord_events(&mut world);

        let active = world.resource::<ActivePeers>().0.clone();
        let guard = active.read().unwrap();
        assert!(guard.contains(&2));
        assert!(guard.contains(&3));
        assert_eq!(guard.len(), 2);
    }

    #[test]
    fn coord_peer_list_prunes_active_peers_for_departed() {
        // Second PeerList drops S3: ActivePeers must no longer contain 3.
        let mut world = setup_world(1);
        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![
                PeerInfo {
                    server_id: SId(2),
                    udp_addr: "127.0.0.1:9102".into(),
                    ws_addr: "127.0.0.1:7879".into(),
                    tls: false,
                },
                PeerInfo {
                    server_id: SId(3),
                    udp_addr: "127.0.0.1:9103".into(),
                    ws_addr: "127.0.0.1:7880".into(),
                    tls: false,
                },
            ]);
        process_coord_events(&mut world);
        {
            let guard = world.resource::<ActivePeers>().0.read().unwrap();
            assert_eq!(guard.len(), 2);
        }

        world
            .resource_mut::<CoordEventQueue>()
            .peer_list_updates
            .push(vec![PeerInfo {
                server_id: SId(2),
                udp_addr: "127.0.0.1:9102".into(),
                ws_addr: "127.0.0.1:7879".into(),
                tls: false,
            }]);
        process_coord_events(&mut world);
        let active = world.resource::<ActivePeers>().0.clone();
        let guard = active.read().unwrap();
        assert!(guard.contains(&2));
        assert!(!guard.contains(&3));
        assert_eq!(guard.len(), 1);
    }

    // -- Crash recovery: aborts transfer to dead server --

    #[test]
    fn recover_entities_aborts_transfer_to_dead_server() {
        let mut world = setup_world(1);
        // Auth entity being transferred to server 3 (which will crash)
        let (entity, eid) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 3,
            target_addr: "127.0.0.1:9103".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        // Server 3 crashes, we are in survivor list
        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1), ServerId(2)]));

        process_coord_events(&mut world);

        // TransferOutbound should be removed
        let lookup = world.resource::<EntityLookup>();
        let bevy_entity = lookup.0[&eid];
        assert!(
            world.get::<TransferOutbound>(bevy_entity).is_none(),
            "transfer to dead server should be aborted"
        );
        // Entity should still be authoritative
        assert!(world.get::<Authoritative>(bevy_entity).is_some());
    }

    #[test]
    fn recover_entities_aborts_transfer_and_promotes_shadows() {
        let mut world = setup_world(1);

        // Auth entity being transferred to server 3
        let (entity, _) = spawn_auth_npc(&mut world, Vec3::ZERO, 1);
        world.entity_mut(entity).insert(TransferOutbound {
            target_server: 3,
            target_addr: "127.0.0.1:9103".parse().unwrap(),
            sent_tick: 0,
            retry_count: 0,
        });

        // Shadow from server 3
        spawn_shadow_npc(&mut world, Vec3::new(100.0, 0.0, 0.0), 3);

        // Server 3 crashes
        world
            .resource_mut::<CoordEventQueue>()
            .recover_events
            .push((ServerId(3), vec![ServerId(1)]));

        process_coord_events(&mut world);

        // Both effects should happen: transfer aborted AND shadow promoted
        let mut auth_query = world.query_filtered::<(), With<Authoritative>>();
        assert_eq!(
            auth_query.iter(&world).count(),
            2,
            "should have original auth entity + promoted shadow"
        );
    }
}
