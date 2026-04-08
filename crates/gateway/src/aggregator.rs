use shared::entity::EntityState;
use shared::protocol::ServerMessage;
use std::collections::HashMap;
use std::time::Instant;

struct ServerSnapshot {
    entities: HashMap<String, EntityState>,
}

pub struct GatewayMetrics {
    pub snapshots_per_sec: u32,
    pub bandwidth_kbps: u64,
    pub rebalance_events: u64,
    pub server_count: usize,
    pub aggregate_us: u64,
    pub broadcast_us: u64,
    pub total_entities: usize,
}

/// Binary frame types (first byte of every frame).
pub const FRAME_FULL_SNAPSHOT: u8 = 0x01;
pub const FRAME_DELTA: u8 = 0x02;

/// Full snapshot every N broadcast ticks.
/// Set to 1 to disable delta compression (send full every tick).
const FULL_SNAPSHOT_INTERVAL: u64 = 1;

pub struct Aggregator {
    server_snapshots: HashMap<u16, ServerSnapshot>,
    last_tick: u64,

    // Metrics
    snapshot_count: u32,
    total_bandwidth_bytes: u64,
    rebalance_events: u64,
    last_metrics_reset: Instant,
    snapshots_per_second: u32,
    previous_authorities: HashMap<String, u16>,

    // Per-tick timing
    pub last_aggregate_us: u64,
    pub last_broadcast_us: u64,

    // Delta compression state
    last_broadcast: HashMap<String, BroadcastState>,
    pub broadcast_tick: u64,
    last_broadcast_tick: u64,
}

#[derive(Clone)]
struct BroadcastState {
    position: [f32; 3],
    velocity: [f32; 3],
    authority_server: u16,
    entity_type: u8,
}

impl Aggregator {
    pub fn new() -> Self {
        Self {
            server_snapshots: HashMap::new(),
            last_tick: 0,
            snapshot_count: 0,
            total_bandwidth_bytes: 0,
            rebalance_events: 0,
            last_metrics_reset: Instant::now(),
            snapshots_per_second: 0,
            previous_authorities: HashMap::new(),
            last_aggregate_us: 0,
            last_broadcast_us: 0,
            last_broadcast: HashMap::new(),
            broadcast_tick: 0,
            last_broadcast_tick: 0,
        }
    }

    pub fn on_server_snapshot(&mut self, server_id: u16, msg: ServerMessage, raw_size: usize) {
        self.snapshot_count += 1;
        self.total_bandwidth_bytes += raw_size as u64;

        if let ServerMessage::WorldSnapshot { tick, entities } = msg {
            let mut entity_map = HashMap::with_capacity(entities.len());
            for e in entities {
                let id_str = e.id.to_string();
                entity_map.insert(id_str, e);
            }

            self.server_snapshots.insert(server_id, ServerSnapshot { entities: entity_map });

            if tick > self.last_tick {
                self.last_tick = tick;
            }
        }
    }

    /// Detect rebalance events by comparing merged snapshot against previous broadcast.
    /// Called once per broadcast tick (not per server snapshot) to avoid false positives.
    fn detect_rebalances(&mut self) {
        // Build merged view first — only one entry per entity
        let mut merged: HashMap<&str, u16> = HashMap::new();
        for snap in self.server_snapshots.values() {
            for (id, e) in &snap.entities {
                merged.insert(id.as_str(), e.authority_server.0);
            }
        }
        // Compare merged view against previous
        for (id, &authority) in &merged {
            if let Some(&prev) = self.previous_authorities.get(*id) {
                if prev != authority {
                    self.rebalance_events += 1;
                }
            }
        }
        // Update previous to current merged state
        self.previous_authorities.clear();
        for (id, authority) in merged {
            self.previous_authorities.insert(id.to_string(), authority);
        }
    }

    pub fn remove_server(&mut self, server_id: u16) {
        self.server_snapshots.remove(&server_id);
    }

    /// Build the next broadcast frame — full snapshot or delta.
    /// Returns (binary_bytes, entity_count_or_update_count).
    pub fn get_broadcast_frame(&mut self, metrics: &GatewayMetrics) -> Option<(Vec<u8>, usize)> {
        if self.server_snapshots.is_empty() {
            return None;
        }


        let t0 = Instant::now();

        let is_full = self.broadcast_tick % FULL_SNAPSHOT_INTERVAL == 0
            || self.last_broadcast.is_empty();

        // Detect rebalances once per broadcast, not per server snapshot
        self.detect_rebalances();

        let result = if is_full {
            self.build_full_snapshot(metrics)
        } else {
            self.build_delta(metrics)
        };

        self.last_aggregate_us = t0.elapsed().as_micros() as u64;

        // Update broadcast state
        self.last_broadcast_tick = self.last_tick;
        self.broadcast_tick += 1;
        self.last_broadcast.clear();
        for snap in self.server_snapshots.values() {
            for (id, e) in &snap.entities {
                self.last_broadcast.insert(id.clone(), BroadcastState {
                    position: [e.position.x, e.position.y, e.position.z],
                    velocity: [e.velocity.x, e.velocity.y, e.velocity.z],
                    authority_server: e.authority_server.0,
                    entity_type: match e.entity_type {
                        shared::entity::EntityType::Npc => 0,
                        shared::entity::EntityType::Player => 1,
                    },
                });
            }
        }

        result
    }

    /// Full snapshot frame.
    /// ```
    /// [type:u8=0x01][tick:u64][entity_count:u32][metrics:14bytes]
    /// [entity × N: 43 bytes each]
    /// ```
    fn build_full_snapshot(&self, metrics: &GatewayMetrics) -> Option<(Vec<u8>, usize)> {
        // Merge entities across all servers, deduplicating by entity ID.
        // If two servers claim the same entity (brief overlap during transfer),
        // prefer the one whose authority_server matches the sending server.
        let mut merged: HashMap<&str, &EntityState> = HashMap::new();
        for (server_id, snap) in &self.server_snapshots {
            for (id, e) in &snap.entities {
                let dominated = merged.get(id.as_str()).map_or(false, |existing| {
                    // Keep the entry from the server that claims authority
                    existing.authority_server.0 == *server_id
                });
                if !merged.contains_key(id.as_str()) || e.authority_server.0 == *server_id || dominated {
                    merged.insert(id.as_str(), e);
                }
            }
        }

        let entity_count = merged.len();
        let mut buf = Vec::with_capacity(1 + 26 + entity_count * 43);

        buf.push(FRAME_FULL_SNAPSHOT);
        self.write_header(&mut buf, entity_count as u32, metrics);

        for e in merged.values() {
            write_entity(&mut buf, e);
        }

        Some((buf, entity_count))
    }

    /// Delta frame: only changed/new entities + removed entity IDs.
    /// ```
    /// [type:u8=0x02][tick:u64][update_count:u32][removal_count:u32][metrics:14bytes]
    /// [updated entities × N: 43 bytes each]
    /// [removed UUIDs × M: 16 bytes each]
    /// ```
    fn build_delta(&self, metrics: &GatewayMetrics) -> Option<(Vec<u8>, usize)> {
        // Collect current entity IDs
        let mut current_ids: HashMap<&str, &EntityState> = HashMap::new();
        for snap in self.server_snapshots.values() {
            for (id, e) in &snap.entities {
                current_ids.insert(id.as_str(), e);
            }
        }

        // Find changed/new entities
        let mut updates: Vec<&EntityState> = Vec::new();
        for (&id, &e) in &current_ids {
            if self.entity_changed(id, e) {
                updates.push(e);
            }
        }

        // Find removed entities (were in last_broadcast but not in current)
        let mut removals: Vec<uuid::Uuid> = Vec::new();
        for id in self.last_broadcast.keys() {
            if !current_ids.contains_key(id.as_str()) {
                if let Ok(uuid) = uuid::Uuid::parse_str(id) {
                    removals.push(uuid);
                }
            }
        }

        let update_count = updates.len();
        let removal_count = removals.len();

        let mut buf = Vec::with_capacity(1 + 30 + update_count * 43 + removal_count * 16);

        buf.push(FRAME_DELTA);
        buf.extend_from_slice(&self.last_tick.to_le_bytes());
        buf.extend_from_slice(&(update_count as u32).to_le_bytes());
        buf.extend_from_slice(&(removal_count as u32).to_le_bytes());
        buf.extend_from_slice(&metrics.snapshots_per_sec.to_le_bytes());
        buf.extend_from_slice(&(metrics.bandwidth_kbps as u32).to_le_bytes());
        buf.extend_from_slice(&(metrics.rebalance_events as u32).to_le_bytes());
        buf.extend_from_slice(&(metrics.server_count as u16).to_le_bytes());

        for e in &updates {
            write_entity(&mut buf, e);
        }

        for uuid in &removals {
            buf.extend_from_slice(uuid.as_bytes());
        }

        Some((buf, update_count))
    }

    fn write_header(&self, buf: &mut Vec<u8>, entity_count: u32, metrics: &GatewayMetrics) {
        buf.extend_from_slice(&self.last_tick.to_le_bytes());
        buf.extend_from_slice(&entity_count.to_le_bytes());
        buf.extend_from_slice(&metrics.snapshots_per_sec.to_le_bytes());
        buf.extend_from_slice(&(metrics.bandwidth_kbps as u32).to_le_bytes());
        buf.extend_from_slice(&(metrics.rebalance_events as u32).to_le_bytes());
        buf.extend_from_slice(&(metrics.server_count as u16).to_le_bytes());
    }

    fn entity_changed(&self, id: &str, e: &EntityState) -> bool {
        match self.last_broadcast.get(id) {
            None => true,
            Some(prev) => {
                prev.authority_server != e.authority_server.0
                    || (prev.position[0] - e.position.x).abs() > 0.01
                    || (prev.position[1] - e.position.y).abs() > 0.01
                    || (prev.position[2] - e.position.z).abs() > 0.01
                    || (prev.velocity[0] - e.velocity.x).abs() > 0.01
                    || (prev.velocity[1] - e.velocity.y).abs() > 0.01
                    || (prev.velocity[2] - e.velocity.z).abs() > 0.01
            }
        }
    }

    pub fn get_authority_server(&self, entity_id: &str) -> Option<u16> {
        for snap in self.server_snapshots.values() {
            if let Some(e) = snap.entities.get(entity_id) {
                return Some(e.authority_server.0);
            }
        }
        None
    }

    pub fn get_least_loaded_server(&self, server_ids: &[u16]) -> u16 {
        let mut best_id = server_ids[0];
        let mut best_count = usize::MAX;
        for &sid in server_ids {
            let count = self.server_snapshots
                .get(&sid)
                .map(|s| s.entities.len())
                .unwrap_or(0);
            if count < best_count {
                best_count = count;
                best_id = sid;
            }
        }
        best_id
    }

    pub fn update_metrics(&mut self) -> GatewayMetrics {
        let elapsed = self.last_metrics_reset.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            self.snapshots_per_second = (self.snapshot_count as f64 / elapsed) as u32;
            self.snapshot_count = 0;
            self.rebalance_events = 0;
            self.last_metrics_reset = Instant::now();
        }

        let total_entities: usize = self.server_snapshots.values()
            .map(|s| s.entities.len())
            .sum();

        GatewayMetrics {
            snapshots_per_sec: self.snapshots_per_second,
            bandwidth_kbps: self.total_bandwidth_bytes / 1024,
            rebalance_events: self.rebalance_events,
            server_count: self.server_snapshots.len(),
            aggregate_us: self.last_aggregate_us,
            broadcast_us: self.last_broadcast_us,
            total_entities,
        }
    }
}

/// Parse a full snapshot frame back into (tick, entities).
/// Used for testing binary protocol correctness.
#[cfg(test)]
pub fn parse_full_snapshot(buf: &[u8]) -> (u64, Vec<(uuid::Uuid, [f32; 3], [f32; 3], u8, u16)>) {
    assert_eq!(buf[0], FRAME_FULL_SNAPSHOT);
    let tick = u64::from_le_bytes(buf[1..9].try_into().unwrap());
    let entity_count = u32::from_le_bytes(buf[9..13].try_into().unwrap()) as usize;
    // Skip 14 bytes of metrics header (4+4+4+2 = snapshots_per_sec + bandwidth + rebalance + server_count)
    let header_end = 1 + 8 + 4 + 14;
    let mut entities = Vec::with_capacity(entity_count);
    for i in 0..entity_count {
        let offset = header_end + i * 43;
        let uuid = uuid::Uuid::from_bytes(buf[offset..offset + 16].try_into().unwrap());
        let px = f32::from_le_bytes(buf[offset + 16..offset + 20].try_into().unwrap());
        let py = f32::from_le_bytes(buf[offset + 20..offset + 24].try_into().unwrap());
        let pz = f32::from_le_bytes(buf[offset + 24..offset + 28].try_into().unwrap());
        let vx = f32::from_le_bytes(buf[offset + 28..offset + 32].try_into().unwrap());
        let vy = f32::from_le_bytes(buf[offset + 32..offset + 36].try_into().unwrap());
        let vz = f32::from_le_bytes(buf[offset + 36..offset + 40].try_into().unwrap());
        let etype = buf[offset + 40];
        let auth = u16::from_le_bytes(buf[offset + 41..offset + 43].try_into().unwrap());
        entities.push((uuid, [px, py, pz], [vx, vy, vz], etype, auth));
    }
    (tick, entities)
}

fn write_entity(buf: &mut Vec<u8>, e: &EntityState) {
    buf.extend_from_slice(e.id.0.as_bytes());
    buf.extend_from_slice(&e.position.x.to_le_bytes());
    buf.extend_from_slice(&e.position.y.to_le_bytes());
    buf.extend_from_slice(&e.position.z.to_le_bytes());
    buf.extend_from_slice(&e.velocity.x.to_le_bytes());
    buf.extend_from_slice(&e.velocity.y.to_le_bytes());
    buf.extend_from_slice(&e.velocity.z.to_le_bytes());
    buf.push(match e.entity_type {
        shared::entity::EntityType::Npc => 0,
        shared::entity::EntityType::Player => 1,
    });
    buf.extend_from_slice(&e.authority_server.0.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::{EntityId, EntityState, EntityType, ServerId};
    use shared::protocol::ServerMessage;
    use glam::Vec3;

    fn default_metrics() -> GatewayMetrics {
        GatewayMetrics {
            snapshots_per_sec: 20,
            bandwidth_kbps: 100,
            rebalance_events: 0,
            server_count: 2,
            aggregate_us: 0,
            broadcast_us: 0,
            total_entities: 0,
        }
    }

    fn make_entity(pos: Vec3, server_id: u16) -> EntityState {
        EntityState::new_npc(pos, ServerId(server_id))
    }

    fn make_snapshot(tick: u64, entities: Vec<EntityState>) -> ServerMessage {
        ServerMessage::WorldSnapshot { tick, entities }
    }

    #[test]
    fn new_aggregator_is_empty() {
        let agg = Aggregator::new();
        assert_eq!(agg.last_tick, 0);
        assert!(agg.server_snapshots.is_empty());
    }

    #[test]
    fn on_server_snapshot_stores_entities() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(1.0, 0.0, 2.0), 1);
        let msg = make_snapshot(10, vec![e]);
        agg.on_server_snapshot(1, msg, 100);

        assert_eq!(agg.last_tick, 10);
        assert_eq!(agg.server_snapshots.len(), 1);
    }

    #[test]
    fn get_authority_server_finds_entity() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 3);
        let id_str = e.id.to_string();
        agg.on_server_snapshot(3, make_snapshot(1, vec![e]), 50);

        assert_eq!(agg.get_authority_server(&id_str), Some(3));
    }

    #[test]
    fn get_authority_server_missing_entity() {
        let agg = Aggregator::new();
        assert_eq!(agg.get_authority_server("nonexistent"), None);
    }

    #[test]
    fn get_least_loaded_server_empty() {
        let agg = Aggregator::new();
        // No snapshots — all have 0 entities, should return first
        assert_eq!(agg.get_least_loaded_server(&[1, 2, 3]), 1);
    }

    #[test]
    fn get_least_loaded_server_picks_lowest() {
        let mut agg = Aggregator::new();

        // Server 1 has 5 entities
        let entities1: Vec<EntityState> = (0..5).map(|_| make_entity(Vec3::ZERO, 1)).collect();
        agg.on_server_snapshot(1, make_snapshot(1, entities1), 500);

        // Server 2 has 2 entities
        let entities2: Vec<EntityState> = (0..2).map(|_| make_entity(Vec3::ZERO, 2)).collect();
        agg.on_server_snapshot(2, make_snapshot(1, entities2), 200);

        // Server 3 has 0 entities (not in snapshots)
        assert_eq!(agg.get_least_loaded_server(&[1, 2, 3]), 3);
    }

    #[test]
    fn remove_server_clears_snapshots() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        agg.on_server_snapshot(1, make_snapshot(1, vec![e]), 50);
        assert_eq!(agg.server_snapshots.len(), 1);

        agg.remove_server(1);
        assert_eq!(agg.server_snapshots.len(), 0);
    }

    #[test]
    fn full_snapshot_binary_frame_roundtrip() {
        let mut agg = Aggregator::new();
        let e1 = make_entity(Vec3::new(10.0, 0.0, -20.0), 1);
        let e2 = EntityState::new_player(Vec3::new(5.0, 0.0, 5.0), ServerId(1));
        let e1_id = e1.id.0;
        let e2_id = e2.id.0;

        agg.on_server_snapshot(1, make_snapshot(42, vec![e1, e2]), 200);

        let metrics = default_metrics();
        let (frame, count) = agg.get_broadcast_frame(&metrics).unwrap();
        assert_eq!(count, 2);
        assert_eq!(frame[0], FRAME_FULL_SNAPSHOT);

        // Parse it back
        let (tick, entities) = parse_full_snapshot(&frame);
        assert_eq!(tick, 42);
        assert_eq!(entities.len(), 2);

        // Verify entity data is correct
        let ids: Vec<uuid::Uuid> = entities.iter().map(|(id, _, _, _, _)| *id).collect();
        assert!(ids.contains(&e1_id));
        assert!(ids.contains(&e2_id));

        // Check one entity's position
        let ent1 = entities.iter().find(|(id, _, _, _, _)| *id == e1_id).unwrap();
        assert!((ent1.1[0] - 10.0).abs() < 0.01);
        assert!((ent1.1[2] - (-20.0)).abs() < 0.01);
        assert_eq!(ent1.3, 0); // NPC type
        assert_eq!(ent1.4, 1); // authority server 1

        let ent2 = entities.iter().find(|(id, _, _, _, _)| *id == e2_id).unwrap();
        assert_eq!(ent2.3, 1); // Player type
    }

    #[test]
    fn full_snapshot_entity_size_is_43_bytes() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        agg.on_server_snapshot(1, make_snapshot(1, vec![e]), 50);

        let metrics = default_metrics();
        let (frame, _) = agg.get_broadcast_frame(&metrics).unwrap();
        // Header: 1 (type) + 8 (tick) + 4 (count) + 14 (metrics) = 27
        // 1 entity × 43 = 43
        assert_eq!(frame.len(), 27 + 43);
    }

    #[test]
    fn empty_snapshot_returns_none() {
        let mut agg = Aggregator::new();
        let metrics = default_metrics();
        assert!(agg.get_broadcast_frame(&metrics).is_none());
    }

    #[test]
    fn dedup_prefers_authority_server_match() {
        let mut agg = Aggregator::new();

        // Same entity reported by two servers during authority transfer
        let e1 = EntityState {
            id: EntityId(uuid::Uuid::new_v4()),
            position: Vec3::new(1.0, 0.0, 1.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2), // claims authority is server 2
        };
        let eid = e1.id;

        let e2 = EntityState {
            id: eid,
            position: Vec3::new(2.0, 0.0, 2.0),
            velocity: Vec3::ZERO,
            entity_type: EntityType::Npc,
            authority_server: ServerId(2), // also claims authority is server 2
        };

        // Server 1 reports it (not the authority)
        agg.on_server_snapshot(1, make_snapshot(10, vec![e1]), 50);
        // Server 2 reports it (IS the authority)
        agg.on_server_snapshot(2, make_snapshot(10, vec![e2]), 50);

        let metrics = default_metrics();
        let (frame, count) = agg.get_broadcast_frame(&metrics).unwrap();
        assert_eq!(count, 1); // deduplicated

        let (_, entities) = parse_full_snapshot(&frame);
        assert_eq!(entities.len(), 1);
        // Should prefer server 2's version since authority matches
        assert!((entities[0].1[0] - 2.0).abs() < 0.01);
    }

    #[test]
    fn detect_rebalances_counts_authority_changes() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        let _eid = e.id;

        // First broadcast — establishes baseline
        agg.on_server_snapshot(1, make_snapshot(1, vec![e.clone()]), 50);
        let metrics = default_metrics();
        agg.get_broadcast_frame(&metrics);

        // Entity authority changes to server 2
        let mut e2 = e.clone();
        e2.authority_server = ServerId(2);
        agg.on_server_snapshot(2, make_snapshot(2, vec![e2]), 50);
        agg.server_snapshots.remove(&1); // server 1 no longer reports it

        agg.get_broadcast_frame(&metrics);
        assert!(agg.rebalance_events > 0);
    }

    #[test]
    fn multiple_servers_merged_correctly() {
        let mut agg = Aggregator::new();

        let e1 = make_entity(Vec3::new(10.0, 0.0, 10.0), 1);
        let e2 = make_entity(Vec3::new(20.0, 0.0, 20.0), 2);
        let e3 = make_entity(Vec3::new(30.0, 0.0, 30.0), 2);

        agg.on_server_snapshot(1, make_snapshot(5, vec![e1]), 50);
        agg.on_server_snapshot(2, make_snapshot(5, vec![e2, e3]), 100);

        let metrics = default_metrics();
        let (_, count) = agg.get_broadcast_frame(&metrics).unwrap();
        assert_eq!(count, 3); // all 3 entities from 2 servers
    }

    #[test]
    fn entity_43_bytes_encoding() {
        // Verify the binary encoding of a single entity
        let e = EntityState {
            id: EntityId(uuid::Uuid::nil()),
            position: Vec3::new(1.0, 2.0, 3.0),
            velocity: Vec3::new(4.0, 5.0, 6.0),
            entity_type: EntityType::Player,
            authority_server: ServerId(7),
        };
        let mut buf = Vec::new();
        write_entity(&mut buf, &e);
        assert_eq!(buf.len(), 43);
        // UUID (16) + pos (12) + vel (12) + type (1) + auth (2) = 43

        // Verify UUID bytes
        assert_eq!(&buf[0..16], uuid::Uuid::nil().as_bytes());
        // Verify position
        assert_eq!(f32::from_le_bytes(buf[16..20].try_into().unwrap()), 1.0);
        assert_eq!(f32::from_le_bytes(buf[20..24].try_into().unwrap()), 2.0);
        assert_eq!(f32::from_le_bytes(buf[24..28].try_into().unwrap()), 3.0);
        // Verify velocity
        assert_eq!(f32::from_le_bytes(buf[28..32].try_into().unwrap()), 4.0);
        // Verify entity type (Player = 1)
        assert_eq!(buf[40], 1);
        // Verify authority server
        assert_eq!(u16::from_le_bytes(buf[41..43].try_into().unwrap()), 7);
    }

    #[test]
    fn entity_changed_new_entity() {
        let agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        // Not in last_broadcast → changed
        assert!(agg.entity_changed(&e.id.to_string(), &e));
    }

    #[test]
    fn entity_changed_same_position_not_changed() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(10.0, 0.0, 20.0), 1);
        let id_str = e.id.to_string();
        // Populate last_broadcast
        agg.last_broadcast.insert(id_str.clone(), BroadcastState {
            position: [10.0, 0.0, 20.0],
            velocity: [0.0, 0.0, 0.0],
            authority_server: 1,
            entity_type: 0,
        });
        assert!(!agg.entity_changed(&id_str, &e));
    }

    #[test]
    fn entity_changed_position_moved() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(10.5, 0.0, 20.0), 1);
        let id_str = e.id.to_string();
        agg.last_broadcast.insert(id_str.clone(), BroadcastState {
            position: [10.0, 0.0, 20.0],
            velocity: [0.0, 0.0, 0.0],
            authority_server: 1,
            entity_type: 0,
        });
        // Position changed by 0.5 (> 0.01 threshold)
        assert!(agg.entity_changed(&id_str, &e));
    }

    #[test]
    fn entity_changed_authority_changed() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(10.0, 0.0, 20.0), 2); // authority = 2
        let id_str = e.id.to_string();
        agg.last_broadcast.insert(id_str.clone(), BroadcastState {
            position: [10.0, 0.0, 20.0],
            velocity: [0.0, 0.0, 0.0],
            authority_server: 1, // was 1
            entity_type: 0,
        });
        assert!(agg.entity_changed(&id_str, &e));
    }

    #[test]
    fn entity_changed_tiny_position_change_below_threshold() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(10.005, 0.0, 20.0), 1);
        let id_str = e.id.to_string();
        agg.last_broadcast.insert(id_str.clone(), BroadcastState {
            position: [10.0, 0.0, 20.0],
            velocity: [0.0, 0.0, 0.0],
            authority_server: 1,
            entity_type: 0,
        });
        // 0.005 < 0.01 threshold
        assert!(!agg.entity_changed(&id_str, &e));
    }

    #[test]
    fn broadcast_tick_increments() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        agg.on_server_snapshot(1, make_snapshot(1, vec![e]), 50);
        let metrics = default_metrics();

        assert_eq!(agg.broadcast_tick, 0);
        agg.get_broadcast_frame(&metrics);
        assert_eq!(agg.broadcast_tick, 1);
        agg.get_broadcast_frame(&metrics);
        assert_eq!(agg.broadcast_tick, 2);
    }

    #[test]
    fn last_broadcast_populated_after_frame() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::new(5.0, 0.0, 10.0), 1);
        let id_str = e.id.to_string();
        agg.on_server_snapshot(1, make_snapshot(1, vec![e]), 50);
        let metrics = default_metrics();

        assert!(agg.last_broadcast.is_empty());
        agg.get_broadcast_frame(&metrics);
        assert!(agg.last_broadcast.contains_key(&id_str));
        let state = &agg.last_broadcast[&id_str];
        assert!((state.position[0] - 5.0).abs() < 0.01);
    }

    #[test]
    fn snapshot_count_tracked() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        agg.on_server_snapshot(1, make_snapshot(1, vec![e.clone()]), 100);
        agg.on_server_snapshot(1, make_snapshot(2, vec![e.clone()]), 100);
        agg.on_server_snapshot(1, make_snapshot(3, vec![e]), 100);
        assert_eq!(agg.snapshot_count, 3);
    }

    #[test]
    fn bandwidth_tracked() {
        let mut agg = Aggregator::new();
        let e = make_entity(Vec3::ZERO, 1);
        agg.on_server_snapshot(1, make_snapshot(1, vec![e]), 500);
        assert_eq!(agg.total_bandwidth_bytes, 500);
    }

    #[test]
    fn metrics_update_returns_current_state() {
        let mut agg = Aggregator::new();
        let entities: Vec<EntityState> = (0..10).map(|_| make_entity(Vec3::ZERO, 1)).collect();
        agg.on_server_snapshot(1, make_snapshot(1, entities), 1000);

        let m = agg.update_metrics();
        assert_eq!(m.total_entities, 10);
        assert_eq!(m.server_count, 1);
    }
}
