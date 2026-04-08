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
