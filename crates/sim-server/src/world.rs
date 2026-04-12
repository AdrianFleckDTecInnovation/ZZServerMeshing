use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use shared::spatial::{clamp_to_world, WORLD_EXTENT};
use glam::Vec3;
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;

pub struct World {
    pub server_id: ServerId,
    pub tick: u64,
    /// Entities this server is authoritative for.
    pub entities: HashMap<EntityId, EntityState>,
    /// Read-only copies of entities from peer servers.
    pub shadow_entities: HashMap<EntityId, EntityState>,
    /// Tick at which each shadow was last updated (for stale cleanup).
    pub shadow_last_seen: HashMap<EntityId, u64>,
    /// UDP addresses of peer servers.
    pub peer_addrs: HashMap<u16, SocketAddr>,
    /// Pending rebalance: (entity_id strings, target_server_id). Set by coordinator client.
    pub pending_transfers: Option<(Vec<String>, u16)>,
    /// Graceful drain mode: transfer all entities and exit.
    pub draining: bool,
    pub drain_targets: Option<Vec<u16>>,
}

/// Max ticks before a shadow entity is considered stale and removed.
const SHADOW_STALE_TICKS: u64 = shared::tick::TICK_RATE as u64 * 5; // 5 seconds

impl World {
    pub fn new(server_id: ServerId) -> Self {
        Self {
            server_id,
            tick: 0,
            entities: HashMap::new(),
            shadow_entities: HashMap::new(),
            shadow_last_seen: HashMap::new(),
            peer_addrs: HashMap::new(),
            pending_transfers: None,
            draining: false,
            drain_targets: None,
        }
    }

    pub fn spawn_initial_npcs(&mut self, count: u32) {
        let mut rng = rand::thread_rng();
        for _ in 0..count {
            let pos = Vec3::new(
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
                0.0,
                rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
            );
            let entity = EntityState::new_npc(pos, self.server_id);
            self.entities.insert(entity.id, entity);
        }
    }

    pub fn spawn_entity(&mut self, position: Vec3, entity_type: EntityType) -> EntityId {
        let entity = match entity_type {
            EntityType::Player => EntityState::new_player(position, self.server_id),
            EntityType::Npc => EntityState::new_npc(position, self.server_id),
        };
        let id = entity.id;
        self.entities.insert(id, entity);
        id
    }

    /// Spawn a player with a specific UUID (assigned by the gateway).
    pub fn spawn_player_with_id(&mut self, uuid: uuid::Uuid, position: Vec3) -> EntityId {
        let id = EntityId(uuid);
        let entity = EntityState {
            id,
            position,
            velocity: Vec3::ZERO,
            entity_type: EntityType::Player,
            authority_server: self.server_id,
        };
        self.entities.insert(id, entity);
        id
    }

    pub fn remove_entity(&mut self, id: &EntityId) {
        self.entities.remove(id);
    }

    pub fn apply_move_input(&mut self, entity_id: &EntityId, direction: Vec3) {
        if let Some(entity) = self.entities.get_mut(entity_id) {
            let speed = match entity.entity_type {
                EntityType::Player => 5.0,
                EntityType::Npc => 2.0,
            };
            entity.velocity = direction.normalize_or_zero() * speed;
        }
    }

    /// Returns only authoritative entities.
    #[cfg(test)]
    pub fn get_snapshot(&self) -> Vec<EntityState> {
        self.entities.values().cloned().collect()
    }

    /// Returns authoritative + shadow entities (for client-facing WebSocket broadcast).
    /// Authoritative entities take precedence over shadows with the same ID.
    pub fn get_client_snapshot(&self) -> Vec<EntityState> {
        let mut snapshot = Vec::with_capacity(self.entities.len() + self.shadow_entities.len());
        for entity in self.entities.values() {
            snapshot.push(entity.clone());
        }
        for (id, entity) in &self.shadow_entities {
            if !self.entities.contains_key(id) {
                snapshot.push(entity.clone());
            }
        }
        snapshot
    }

    /// Apply incoming shadow entity data from a peer server.
    /// Discards stale updates (lower tick than what we already have).
    pub fn apply_peer_update(&mut self, tick: u64, entities: Vec<EntityState>) {
        for entity in entities {
            // Don't overwrite our own authoritative entities
            if self.entities.contains_key(&entity.id) {
                continue;
            }
            // Discard stale packets (out-of-order UDP)
            if let Some(&last_tick) = self.shadow_last_seen.get(&entity.id) {
                if tick < last_tick {
                    continue;
                }
            }
            self.shadow_last_seen.insert(entity.id, tick);
            self.shadow_entities.insert(entity.id, entity);
        }
    }

    /// Remove stale shadow entities.
    pub fn cleanup_shadows(&mut self) {
        let tick = self.tick;
        let stale: Vec<EntityId> = self
            .shadow_last_seen
            .iter()
            .filter(|(_, &last)| tick.saturating_sub(last) > SHADOW_STALE_TICKS)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.shadow_entities.remove(&id);
            self.shadow_last_seen.remove(&id);
        }
    }

    pub fn clamp_all_positions(&mut self) {
        for entity in self.entities.values_mut() {
            entity.position = clamp_to_world(entity.position);
        }
    }

    /// Returns the count of authoritative entities.
    #[cfg(test)]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns the count of shadow entities.
    #[cfg(test)]
    pub fn shadow_count(&self) -> usize {
        self.shadow_entities.len()
    }

    /// Recover orphaned entities from a dead server by promoting shadows to authoritative.
    /// Recover entities from a dead server.
    /// `survivor_ids` is the sorted list of all surviving server IDs.
    /// Each survivor only adopts entities whose UUID hashes to its position
    /// in the survivor list, preventing duplicate adoption.
    pub fn recover_from_dead_server(&mut self, dead_server_id: u16, survivor_ids: &[u16]) {
        let my_index = survivor_ids.iter().position(|&id| id == self.server_id.0);
        let my_index = match my_index {
            Some(i) => i,
            None => return, // we're not in the survivor list
        };
        let survivor_count = survivor_ids.len();

        let to_adopt: Vec<EntityId> = self
            .shadow_entities
            .iter()
            .filter(|(_, e)| e.authority_server.0 == dead_server_id)
            .filter(|(id, _)| {
                // Distribute entities across survivors by UUID hash
                (id.0.as_u128() as usize) % survivor_count == my_index
            })
            .map(|(id, _)| *id)
            .collect();

        let mut adopted = 0;
        for id in to_adopt {
            if let Some(mut entity) = self.shadow_entities.remove(&id) {
                entity.authority_server = self.server_id;
                self.entities.insert(id, entity);
                self.shadow_last_seen.remove(&id);
                adopted += 1;
            }
        }

        if adopted > 0 {
            tracing::info!("Recovered {} entities from dead S{}", adopted, dead_server_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::{EntityState, EntityType, ServerId};

    fn make_world(server_id: u16) -> World {
        World::new(ServerId(server_id))
    }

    #[test]
    fn spawn_entity_adds_to_entities() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::new(10.0, 0.0, 20.0), EntityType::Npc);
        assert_eq!(w.entity_count(), 1);
        assert!(w.entities.contains_key(&id));
        assert_eq!(w.entities[&id].authority_server, ServerId(1));
    }

    #[test]
    fn spawn_player_with_id_uses_given_uuid() {
        let mut w = make_world(2);
        let uuid = uuid::Uuid::new_v4();
        let id = w.spawn_player_with_id(uuid, Vec3::ZERO);
        assert_eq!(id.0, uuid);
        assert_eq!(w.entities[&id].entity_type, EntityType::Player);
        assert_eq!(w.entities[&id].authority_server, ServerId(2));
    }

    #[test]
    fn remove_entity_removes() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        assert_eq!(w.entity_count(), 1);
        w.remove_entity(&id);
        assert_eq!(w.entity_count(), 0);
    }

    #[test]
    fn remove_nonexistent_entity_is_noop() {
        let mut w = make_world(1);
        let fake_id = EntityId::new();
        w.remove_entity(&fake_id); // should not panic
        assert_eq!(w.entity_count(), 0);
    }

    #[test]
    fn apply_move_input_sets_velocity() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Player);
        w.apply_move_input(&id, Vec3::new(1.0, 0.0, 0.0));
        let vel = w.entities[&id].velocity;
        assert!((vel.x - 5.0).abs() < 0.01); // player speed = 5.0
        assert_eq!(vel.z, 0.0);
    }

    #[test]
    fn apply_move_input_npc_speed() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        w.apply_move_input(&id, Vec3::new(0.0, 0.0, 1.0));
        let vel = w.entities[&id].velocity;
        assert!((vel.z - 2.0).abs() < 0.01); // npc speed = 2.0
    }

    #[test]
    fn apply_move_input_zero_direction() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Player);
        w.apply_move_input(&id, Vec3::ZERO);
        assert_eq!(w.entities[&id].velocity, Vec3::ZERO);
    }

    #[test]
    fn apply_move_input_nonexistent_entity_is_noop() {
        let mut w = make_world(1);
        let fake_id = EntityId::new();
        w.apply_move_input(&fake_id, Vec3::X); // should not panic
    }

    #[test]
    fn get_client_snapshot_merges_auth_and_shadow() {
        let mut w = make_world(1);
        w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        w.spawn_entity(Vec3::X, EntityType::Npc);
        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        w.shadow_entities.insert(shadow.id, shadow);
        let snap = w.get_client_snapshot();
        assert_eq!(snap.len(), 3); // 2 auth + 1 shadow
    }

    #[test]
    fn get_client_snapshot_auth_takes_precedence() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        // Insert a shadow with the same ID (shouldn't happen in practice but guard against it)
        let mut shadow = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        shadow.id = id;
        w.shadow_entities.insert(id, shadow);
        let snap = w.get_client_snapshot();
        assert_eq!(snap.len(), 1); // no duplicate
        assert_eq!(snap[0].position, Vec3::ZERO); // auth version, not shadow
    }

    #[test]
    fn get_snapshot_returns_only_authoritative() {
        let mut w = make_world(1);
        w.spawn_entity(Vec3::ZERO, EntityType::Npc);
        w.spawn_entity(Vec3::X, EntityType::Npc);

        // Add a shadow entity
        let shadow = EntityState::new_npc(Vec3::Z, ServerId(2));
        w.shadow_entities.insert(shadow.id, shadow);

        let snap = w.get_snapshot();
        assert_eq!(snap.len(), 2); // only auth entities, not the shadow
    }

    #[test]
    fn apply_peer_update_adds_shadows() {
        let mut w = make_world(1);
        let e = EntityState::new_npc(Vec3::new(100.0, 0.0, 100.0), ServerId(2));
        let eid = e.id;
        w.apply_peer_update(10, vec![e]);
        assert_eq!(w.shadow_count(), 1);
        assert!(w.shadow_entities.contains_key(&eid));
    }

    #[test]
    fn apply_peer_update_does_not_overwrite_authoritative() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::ZERO, EntityType::Npc);

        // Peer sends an update for the same entity ID
        let mut shadow = EntityState::new_npc(Vec3::new(999.0, 0.0, 999.0), ServerId(2));
        shadow.id = id; // same ID as our auth entity
        w.apply_peer_update(10, vec![shadow]);

        // Auth entity should be unchanged
        assert_eq!(w.entities[&id].position, Vec3::ZERO);
        assert_eq!(w.shadow_count(), 0); // not added as shadow either
    }

    #[test]
    fn apply_peer_update_discards_stale_tick() {
        let mut w = make_world(1);
        let e1 = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = e1.id;

        // Apply tick 20
        w.apply_peer_update(20, vec![e1]);
        assert_eq!(w.shadow_entities[&eid].position.x, 1.0);

        // Apply tick 15 (stale) — should be discarded
        let mut e2 = EntityState::new_npc(Vec3::new(99.0, 0.0, 99.0), ServerId(2));
        e2.id = eid;
        w.apply_peer_update(15, vec![e2]);
        assert_eq!(w.shadow_entities[&eid].position.x, 1.0); // unchanged
    }

    #[test]
    fn apply_peer_update_accepts_newer_tick() {
        let mut w = make_world(1);
        let e1 = EntityState::new_npc(Vec3::new(1.0, 0.0, 1.0), ServerId(2));
        let eid = e1.id;
        w.apply_peer_update(10, vec![e1]);

        let mut e2 = EntityState::new_npc(Vec3::new(50.0, 0.0, 50.0), ServerId(2));
        e2.id = eid;
        w.apply_peer_update(20, vec![e2]);
        assert_eq!(w.shadow_entities[&eid].position.x, 50.0);
    }

    #[test]
    fn cleanup_shadows_removes_stale() {
        let mut w = make_world(1);
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        let eid = e.id;
        w.apply_peer_update(1, vec![e]);
        assert_eq!(w.shadow_count(), 1);

        // Advance tick well past stale threshold (5s = 100 ticks at 20Hz)
        w.tick = 200;
        w.cleanup_shadows();
        assert_eq!(w.shadow_count(), 0);
        assert!(!w.shadow_last_seen.contains_key(&eid));
    }

    #[test]
    fn cleanup_shadows_keeps_fresh() {
        let mut w = make_world(1);
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        w.apply_peer_update(50, vec![e]);
        w.tick = 60; // only 10 ticks old — well within threshold
        w.cleanup_shadows();
        assert_eq!(w.shadow_count(), 1);
    }

    #[test]
    fn clamp_all_positions_enforces_bounds() {
        let mut w = make_world(1);
        let id = w.spawn_entity(Vec3::new(9999.0, 0.0, -9999.0), EntityType::Npc);
        w.clamp_all_positions();
        assert_eq!(w.entities[&id].position.x, WORLD_EXTENT);
        assert_eq!(w.entities[&id].position.z, -WORLD_EXTENT);
    }

    #[test]
    fn recover_from_dead_server_adopts_correct_subset() {
        let mut w = make_world(1);
        // Add shadows from dead server 3
        let mut adopted_count = 0;
        for _ in 0..20 {
            let e = EntityState::new_npc(Vec3::ZERO, ServerId(3));
            let eid = e.id;
            w.shadow_entities.insert(eid, e);
            w.shadow_last_seen.insert(eid, 10);
            // Count how many this server would adopt with survivors [1, 2]
            if (eid.0.as_u128() as usize) % 2 == 0 {
                adopted_count += 1;
            }
        }

        let initial_shadows = w.shadow_count();
        w.recover_from_dead_server(3, &[1, 2]);

        // Server 1 (index 0) adopts entities where uuid_hash % 2 == 0
        assert_eq!(w.entity_count(), adopted_count);
        assert_eq!(w.shadow_count(), initial_shadows - adopted_count);

        // All adopted entities should now have authority_server = 1
        for e in w.entities.values() {
            assert_eq!(e.authority_server, ServerId(1));
        }
    }

    #[test]
    fn recover_from_dead_server_no_duplicates_across_survivors() {
        // Create two worlds simulating two survivors
        let mut w1 = make_world(1);
        let mut w2 = make_world(2);

        // Both have the same shadow entities from dead server 3
        let mut shadow_ids = Vec::new();
        for _ in 0..50 {
            let e = EntityState::new_npc(Vec3::ZERO, ServerId(3));
            let eid = e.id;
            shadow_ids.push(eid);
            w1.shadow_entities.insert(eid, e.clone());
            w1.shadow_last_seen.insert(eid, 10);
            w2.shadow_entities.insert(eid, e);
            w2.shadow_last_seen.insert(eid, 10);
        }

        let survivors = vec![1, 2];
        w1.recover_from_dead_server(3, &survivors);
        w2.recover_from_dead_server(3, &survivors);

        // No entity should be adopted by both
        for id in &shadow_ids {
            let in_w1 = w1.entities.contains_key(id);
            let in_w2 = w2.entities.contains_key(id);
            assert!(!(in_w1 && in_w2), "Entity {} adopted by both survivors", id);
        }

        // All entities should be adopted by exactly one survivor
        let total_adopted = w1.entity_count() + w2.entity_count();
        assert_eq!(total_adopted, 50);
    }

    #[test]
    fn recover_ignores_shadows_from_other_servers() {
        let mut w = make_world(1);
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(2));
        w.shadow_entities.insert(e.id, e);
        w.shadow_last_seen.insert(EntityId::new(), 10);

        w.recover_from_dead_server(3, &[1]); // dead server is 3, shadow is from 2
        assert_eq!(w.entity_count(), 0); // nothing adopted
    }

    #[test]
    fn recover_when_not_in_survivor_list() {
        let mut w = make_world(1);
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(3));
        w.shadow_entities.insert(e.id, e);
        w.recover_from_dead_server(3, &[2, 4]); // server 1 not in list
        assert_eq!(w.entity_count(), 0);
    }

    #[test]
    fn spawn_initial_npcs() {
        let mut w = make_world(1);
        w.spawn_initial_npcs(10);
        assert_eq!(w.entity_count(), 10);
        for e in w.entities.values() {
            assert_eq!(e.entity_type, EntityType::Npc);
            assert_eq!(e.authority_server, ServerId(1));
            // Positions should be within 80% of world extent
            assert!(e.position.x.abs() <= WORLD_EXTENT * 0.8 + 0.01);
            assert!(e.position.z.abs() <= WORLD_EXTENT * 0.8 + 0.01);
        }
    }
}
