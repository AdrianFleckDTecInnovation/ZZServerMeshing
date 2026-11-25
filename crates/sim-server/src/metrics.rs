use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const RING_SIZE: usize = 200; // 10 seconds at 20Hz

/// Per-tick timing breakdown.
#[derive(Debug, Clone, Default)]
pub struct TickTimings {
    pub input_drain: Duration,
    pub simulation: Duration,
    /// Part of simulation, measured before asynchronous WebSocket transmission.
    pub client_snapshot: Duration,
    pub transfers: Duration,
    pub replication: Duration,
    pub shadow_cleanup: Duration,
    pub total: Duration,
}

/// Collects per-tick timings in a ring buffer and computes percentiles.
pub struct MetricsCollector {
    server_id: u16,
    /// Local session id (WI-136). Set once at startup from `SessionId` resource
    /// via `set_session_id`; exposed on `/metrics` so operators can see when
    /// this server has restarted.
    session_id: u64,
    ring: VecDeque<TickTimings>,
    work: TickWork,
    pub current_tick: u64,
    pub entity_count: usize,
    pub shadow_count: usize,
    pub peer_count: usize,
    pub connected_clients: usize,
    /// Replication mode: "full", "filtered", or "tiered".
    pub replication_mode: String,
    /// Total authoritative entities (full replication size).
    pub replication_full_count: usize,
    /// Entities actually sent across all peers after filtering.
    pub replication_sent_count: usize,
    /// Hot-tier entities sent this tick (near peer players, 20Hz).
    pub replication_hot_count: usize,
    /// Cold-tier entities sent this tick (far from players, 5Hz on cold ticks only).
    pub replication_cold_count: usize,
    /// UDP replication channel drops in the current 5-second window.
    pub udp_repl_drops: u64,
    /// UDP control channel drops in the current 5-second window.
    pub udp_ctrl_drops: u64,
    /// UDP bytes enqueued for outbound send this tick.
    pub udp_bytes_out: u64,
    /// Per-peer network health. Key is peer server_id.
    pub peer_health: HashMap<u16, PeerHealthSnapshot>,
}

/// Per-peer network health snapshot for the metrics endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct PeerHealthSnapshot {
    /// Latest tick received from this peer.
    ///
    /// WI-212: this value is continuous across peer restarts — it is a
    /// high-water mark that is not reset when the peer's session changes.
    pub latest_tick: u64,
    /// How many ticks behind our current tick this peer's latest data is.
    pub staleness_ticks: u64,
    /// Packets received from this peer in the current reporting window (~5s).
    ///
    /// WI-212: preserved across session changes. The 5s reset (in the game
    /// loop) is a periodic rollup, not a restart-triggered reset.
    pub packets_received: u32,
    /// Incomplete chunk reassemblies evicted for this peer in the current reporting window.
    pub chunk_evictions: u32,
    /// Most recently observed session id for this peer (WI-136). Emitted as
    /// a decimal u64 in JSON (consumers can render as hex; the sim-server
    /// startup log line is already hex-formatted for human eyes).
    pub session_id: u64,
    /// WI-211: count of straggler UDP packets dropped for this peer because
    /// their `session_id` was in the LRU of fully-superseded sessions.
    /// Non-zero values indicate WiFi reorder / rolling-deploy churn rather
    /// than correctness bugs.
    pub stragglers_dropped: u64,
    /// WI-212: local tick at which the most recent session change was
    /// observed for this peer. `0` means "no session change has been
    /// observed yet since this sim-server started" (the peer is on its
    /// first session since our boot). Subsequent values advance on each
    /// detected peer restart.
    pub session_changed_at_tick: u64,
}

#[derive(Debug, Serialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub server_id: u16,
    /// Local sim-server session id (WI-136). Unique per sim-server process;
    /// changes across restarts so peers and operators can detect a restart.
    pub session_id: u64,
    pub tick: u64,
    pub entity_count: usize,
    pub shadow_count: usize,
    pub peer_count: usize,
    pub connected_clients: usize,
    pub tick_budget_pct: f64,
    pub phases: PhaseMetrics,
    /// Cumulative elapsed work. Differences between two snapshots cover that exact tick window.
    pub work: TickWork,
    /// Replication mode on the most recent tick: "full", "filtered", or "tiered".
    pub replication_mode: String,
    /// Total authoritative entity count (would be sent unfiltered).
    pub replication_full_count: usize,
    /// Total entities actually sent across all peers (after filtering).
    pub replication_sent_count: usize,
    /// Hot-tier entities sent (near peer players, 20Hz).
    pub replication_hot_count: usize,
    /// Cold-tier entities sent (far from players, sent on cold ticks at 5Hz).
    pub replication_cold_count: usize,
    /// UDP replication channel drops in the current 5-second window.
    pub udp_repl_drops: u64,
    /// UDP control channel drops in the current 5-second window.
    pub udp_ctrl_drops: u64,
    /// UDP bytes enqueued for outbound send this tick.
    pub udp_bytes_out: u64,
    /// Per-peer network health. Key is peer server_id.
    pub peer_health: HashMap<u16, PeerHealthSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct PhaseMetrics {
    pub input_drain_ms: Percentiles,
    pub simulation_ms: Percentiles,
    pub client_snapshot_ms: Percentiles,
    pub transfers_ms: Percentiles,
    pub replication_ms: Percentiles,
    pub shadow_cleanup_ms: Percentiles,
    pub total_ms: Percentiles,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TickWork {
    pub total_us: u64,
    pub client_snapshot_us: u64,
}

impl MetricsCollector {
    pub fn new(server_id: u16) -> Self {
        Self {
            server_id,
            session_id: 0,
            ring: VecDeque::with_capacity(RING_SIZE),
            work: TickWork::default(),
            current_tick: 0,
            entity_count: 0,
            shadow_count: 0,
            peer_count: 0,
            connected_clients: 0,
            replication_mode: "full".to_string(),
            replication_full_count: 0,
            replication_sent_count: 0,
            replication_hot_count: 0,
            replication_cold_count: 0,
            udp_repl_drops: 0,
            udp_ctrl_drops: 0,
            udp_bytes_out: 0,
            peer_health: HashMap::new(),
        }
    }

    /// Record the local sim-server session id (WI-136). Called once at
    /// startup after the `SessionId` resource is generated.
    pub fn set_session_id(&mut self, session_id: u64) {
        self.session_id = session_id;
    }

    /// Read the local session id. Primarily for tests and dashboard rendering.
    #[allow(dead_code)] // used by tests; kept public for dashboard/debug consumers
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn record(&mut self, timings: TickTimings) {
        self.work.total_us = self
            .work
            .total_us
            .saturating_add(timings.total.as_micros() as u64);
        self.work.client_snapshot_us = self
            .work
            .client_snapshot_us
            .saturating_add(timings.client_snapshot.as_micros() as u64);
        if self.ring.len() >= RING_SIZE {
            self.ring.pop_front();
        }
        self.ring.push_back(timings);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let tick_budget_ms = 50.0; // TICK_DURATION_MS

        let total_p95 = self.percentile(|t| t.total, 0.95);
        let tick_budget_pct = (total_p95 / tick_budget_ms) * 100.0;

        MetricsSnapshot {
            server_id: self.server_id,
            session_id: self.session_id,
            tick: self.current_tick,
            entity_count: self.entity_count,
            shadow_count: self.shadow_count,
            peer_count: self.peer_count,
            connected_clients: self.connected_clients,
            tick_budget_pct,
            work: self.work,
            phases: PhaseMetrics {
                input_drain_ms: self.percentiles(|t| t.input_drain),
                simulation_ms: self.percentiles(|t| t.simulation),
                client_snapshot_ms: self.percentiles(|t| t.client_snapshot),
                transfers_ms: self.percentiles(|t| t.transfers),
                replication_ms: self.percentiles(|t| t.replication),
                shadow_cleanup_ms: self.percentiles(|t| t.shadow_cleanup),
                total_ms: self.percentiles(|t| t.total),
            },
            replication_mode: self.replication_mode.clone(),
            replication_full_count: self.replication_full_count,
            replication_sent_count: self.replication_sent_count,
            replication_hot_count: self.replication_hot_count,
            replication_cold_count: self.replication_cold_count,
            udp_repl_drops: self.udp_repl_drops,
            udp_ctrl_drops: self.udp_ctrl_drops,
            udp_bytes_out: self.udp_bytes_out,
            peer_health: self.peer_health.clone(),
        }
    }

    fn percentiles(&self, extract: impl Fn(&TickTimings) -> Duration) -> Percentiles {
        Percentiles {
            p50: self.percentile(&extract, 0.50),
            p95: self.percentile(&extract, 0.95),
            p99: self.percentile(&extract, 0.99),
        }
    }

    fn percentile(&self, extract: impl Fn(&TickTimings) -> Duration, pct: f64) -> f64 {
        if self.ring.is_empty() {
            return 0.0;
        }
        let mut values: Vec<f64> = self
            .ring
            .iter()
            .map(|t| extract(t).as_secs_f64() * 1000.0)
            .collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((values.len() as f64 * pct) as usize).min(values.len() - 1);
        values[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(total_ms: u64) -> TickTimings {
        TickTimings {
            input_drain: Duration::from_micros(100),
            simulation: Duration::from_micros(500),
            client_snapshot: Duration::from_micros(250),
            transfers: Duration::from_micros(50),
            replication: Duration::from_micros(200),
            shadow_cleanup: Duration::from_micros(30),
            total: Duration::from_millis(total_ms),
        }
    }

    #[test]
    fn empty_collector_returns_zero_percentiles() {
        let c = MetricsCollector::new(1);
        let snap = c.snapshot();
        assert_eq!(snap.phases.total_ms.p50, 0.0);
        assert_eq!(snap.phases.total_ms.p95, 0.0);
        assert_eq!(snap.phases.total_ms.p99, 0.0);
        assert_eq!(snap.tick_budget_pct, 0.0);
    }

    #[test]
    fn record_single_timing() {
        let mut c = MetricsCollector::new(1);
        c.record(timing(10));
        let snap = c.snapshot();
        assert!((snap.phases.total_ms.p50 - 10.0).abs() < 0.1);
        assert!((snap.phases.total_ms.p95 - 10.0).abs() < 0.1);
    }

    #[test]
    fn percentile_ordering() {
        let mut c = MetricsCollector::new(1);
        // Add timings 1ms through 100ms
        for i in 1..=100 {
            c.record(timing(i));
        }
        let snap = c.snapshot();
        assert!(snap.phases.total_ms.p50 <= snap.phases.total_ms.p95);
        assert!(snap.phases.total_ms.p95 <= snap.phases.total_ms.p99);
        // p50 should be around 50ms
        assert!((snap.phases.total_ms.p50 - 50.0).abs() < 2.0);
        // p95 should be around 95ms
        assert!((snap.phases.total_ms.p95 - 95.0).abs() < 2.0);
        // p99 should be around 99ms
        assert!((snap.phases.total_ms.p99 - 99.0).abs() < 2.0);
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut c = MetricsCollector::new(1);
        // Fill ring buffer (200 entries) with 1ms timings
        for _ in 0..200 {
            c.record(timing(1));
        }
        // Add 200 more with 10ms — old 1ms entries should be evicted
        for _ in 0..200 {
            c.record(timing(10));
        }
        let snap = c.snapshot();
        // All entries should now be 10ms
        assert!((snap.phases.total_ms.p50 - 10.0).abs() < 0.1);
        // Interval counters also include ticks which have left the percentile window.
        assert_eq!(snap.work.total_us, 2_200_000);
        assert_eq!(snap.work.client_snapshot_us, 100_000);
    }

    #[test]
    fn tick_budget_percentage() {
        let mut c = MetricsCollector::new(1);
        // 25ms total = 50% of 50ms budget
        for _ in 0..100 {
            c.record(timing(25));
        }
        let snap = c.snapshot();
        assert!((snap.tick_budget_pct - 50.0).abs() < 1.0);
    }

    #[test]
    fn snapshot_reflects_state() {
        let mut c = MetricsCollector::new(42);
        c.current_tick = 1000;
        c.entity_count = 50;
        c.shadow_count = 30;
        c.peer_count = 2;
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.server_id, 42);
        assert_eq!(snap.tick, 1000);
        assert_eq!(snap.entity_count, 50);
        assert_eq!(snap.shadow_count, 30);
        assert_eq!(snap.peer_count, 2);
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let mut c = MetricsCollector::new(1);
        c.record(timing(5));
        let snap = c.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"server_id\":1"));
        assert!(json.contains("\"p50\""));
        assert!(json.contains("\"tick_budget_pct\""));
    }

    #[test]
    fn phase_timings_are_separate() {
        let mut c = MetricsCollector::new(1);
        let t = TickTimings {
            input_drain: Duration::from_millis(1),
            simulation: Duration::from_millis(5),
            client_snapshot: Duration::from_millis(4),
            transfers: Duration::from_millis(2),
            replication: Duration::from_millis(3),
            shadow_cleanup: Duration::from_millis(1),
            total: Duration::from_millis(12),
        };
        c.record(t);
        let snap = c.snapshot();
        assert!((snap.phases.input_drain_ms.p50 - 1.0).abs() < 0.1);
        assert!((snap.phases.simulation_ms.p50 - 5.0).abs() < 0.1);
        assert_eq!(snap.phases.client_snapshot_ms.p50, 4.0);
        assert_eq!(snap.work.total_us, 12_000);
        assert_eq!(snap.work.client_snapshot_us, 4_000);
        assert!((snap.phases.transfers_ms.p50 - 2.0).abs() < 0.1);
        assert!((snap.phases.replication_ms.p50 - 3.0).abs() < 0.1);
        assert!((snap.phases.shadow_cleanup_ms.p50 - 1.0).abs() < 0.1);
    }

    #[test]
    fn snapshot_hot_cold_counts_in_json() {
        // Verify that replication_hot_count and replication_cold_count
        // are serialized into the JSON metrics snapshot.
        let mut c = MetricsCollector::new(1);
        c.replication_hot_count = 42;
        c.replication_cold_count = 17;
        c.replication_mode = "tiered".to_string();
        c.record(timing(5));
        let snap = c.snapshot();
        assert_eq!(snap.replication_hot_count, 42);
        assert_eq!(snap.replication_cold_count, 17);
        assert_eq!(snap.replication_mode, "tiered");

        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"replication_hot_count\":42"),
            "hot_count missing from JSON: {json}"
        );
        assert!(
            json.contains("\"replication_cold_count\":17"),
            "cold_count missing from JSON: {json}"
        );
        assert!(
            json.contains("\"replication_mode\":\"tiered\""),
            "replication_mode missing from JSON: {json}"
        );
    }

    #[test]
    fn snapshot_hot_cold_counts_default_zero() {
        // Fresh MetricsCollector should have hot/cold counts at 0.
        let c = MetricsCollector::new(1);
        let snap = c.snapshot();
        assert_eq!(snap.replication_hot_count, 0);
        assert_eq!(snap.replication_cold_count, 0);
        assert_eq!(snap.replication_mode, "full");
    }

    // =========================================================================
    // WI-100: UDP drop counters and bytes out
    // =========================================================================

    #[test]
    fn snapshot_udp_drops_default_zero() {
        let c = MetricsCollector::new(1);
        let snap = c.snapshot();
        assert_eq!(snap.udp_repl_drops, 0);
        assert_eq!(snap.udp_ctrl_drops, 0);
        assert_eq!(snap.udp_bytes_out, 0);
    }

    #[test]
    fn snapshot_udp_drops_in_json() {
        let mut c = MetricsCollector::new(1);
        c.udp_repl_drops = 5;
        c.udp_ctrl_drops = 2;
        c.udp_bytes_out = 12345;
        c.record(timing(5));
        let snap = c.snapshot();
        assert_eq!(snap.udp_repl_drops, 5);
        assert_eq!(snap.udp_ctrl_drops, 2);
        assert_eq!(snap.udp_bytes_out, 12345);

        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"udp_repl_drops\":5"),
            "udp_repl_drops missing: {json}"
        );
        assert!(
            json.contains("\"udp_ctrl_drops\":2"),
            "udp_ctrl_drops missing: {json}"
        );
        assert!(
            json.contains("\"udp_bytes_out\":12345"),
            "udp_bytes_out missing: {json}"
        );
    }

    #[test]
    fn snapshot_udp_drops_accumulate() {
        let mut c = MetricsCollector::new(1);
        c.udp_repl_drops = 3;
        c.record(timing(5));
        let snap1 = c.snapshot();
        assert_eq!(snap1.udp_repl_drops, 3);

        c.udp_repl_drops = 10;
        let snap2 = c.snapshot();
        assert_eq!(snap2.udp_repl_drops, 10);
    }

    // =========================================================================
    // WI-104: Per-peer health snapshots
    // =========================================================================

    #[test]
    fn snapshot_peer_health_default_empty() {
        let c = MetricsCollector::new(1);
        let snap = c.snapshot();
        assert!(snap.peer_health.is_empty());
    }

    #[test]
    fn snapshot_peer_health_in_json() {
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 49998,
                staleness_ticks: 2,
                packets_received: 19,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));
        let snap = c.snapshot();
        assert_eq!(snap.peer_health.len(), 1);
        let p2 = &snap.peer_health[&2];
        assert_eq!(p2.latest_tick, 49998);
        assert_eq!(p2.staleness_ticks, 2);
        assert_eq!(p2.packets_received, 19);
        assert_eq!(p2.chunk_evictions, 0);

        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"peer_health\""),
            "peer_health missing: {json}"
        );
        assert!(
            json.contains("\"latest_tick\":49998"),
            "latest_tick missing: {json}"
        );
        assert!(
            json.contains("\"staleness_ticks\":2"),
            "staleness_ticks missing: {json}"
        );
    }

    #[test]
    fn snapshot_peer_health_multiple_peers() {
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 100,
                staleness_ticks: 5,
                packets_received: 20,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.peer_health.insert(
            3,
            PeerHealthSnapshot {
                latest_tick: 95,
                staleness_ticks: 10,
                packets_received: 14,
                chunk_evictions: 2,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));
        let snap = c.snapshot();
        assert_eq!(snap.peer_health.len(), 2);
        assert!(snap.peer_health.contains_key(&2));
        assert!(snap.peer_health.contains_key(&3));
        assert_eq!(snap.peer_health[&3].chunk_evictions, 2);
    }

    #[test]
    fn snapshot_full_mode_resets_hot_cold() {
        // When replication_mode is "full", hot/cold counts should reflect
        // the values set on the collector (which the game_loop sets to 0).
        let mut c = MetricsCollector::new(1);
        c.replication_mode = "full".to_string();
        c.replication_hot_count = 0;
        c.replication_cold_count = 0;
        c.replication_full_count = 100;
        c.replication_sent_count = 200;
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.replication_hot_count, 0);
        assert_eq!(snap.replication_cold_count, 0);
        assert_eq!(snap.replication_full_count, 100);
        assert_eq!(snap.replication_sent_count, 200);
    }

    // =========================================================================
    // WI-105: Peer health edge cases
    // =========================================================================

    #[test]
    fn peer_health_snapshot_staleness_max() {
        // When a peer has never sent data (latest_tick=0), the game loop sets
        // staleness to u64::MAX. Verify that serializes without overflow.
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 0,
                staleness_ticks: u64::MAX,
                packets_received: 0,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.peer_health[&2].staleness_ticks, u64::MAX);

        // Must serialize without panic
        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"staleness_ticks\":18446744073709551615"),
            "u64::MAX staleness should serialize: {json}"
        );
    }

    #[test]
    fn peer_health_snapshot_zero_staleness() {
        // Peer is exactly caught up (latest_tick == current_tick).
        let mut c = MetricsCollector::new(1);
        c.current_tick = 500;
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 500,
                staleness_ticks: 0,
                packets_received: 100,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.peer_health[&2].staleness_ticks, 0);
        assert_eq!(snap.peer_health[&2].latest_tick, 500);
    }

    #[test]
    fn peer_health_json_keys_are_string() {
        // HashMap<u16, _> serializes with string keys in JSON.
        // Verify the peer_health map uses "2" not 2 as keys.
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 10,
                staleness_ticks: 5,
                packets_received: 3,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let json = serde_json::to_string(&c.snapshot()).unwrap();
        // serde_json serializes HashMap<u16, _> with string keys
        assert!(
            json.contains("\"2\":{") || json.contains("\"2\": {"),
            "peer_health key should be a string: {json}"
        );
    }

    #[test]
    fn peer_health_overwrite_replaces_previous() {
        // Setting peer_health on the collector replaces the previous value.
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 10,
                staleness_ticks: 90,
                packets_received: 5,
                chunk_evictions: 1,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        // Simulate next reporting window
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 110,
                staleness_ticks: 0,
                packets_received: 20,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.peer_health.len(), 1);
        assert_eq!(snap.peer_health[&2].latest_tick, 110);
        assert_eq!(snap.peer_health[&2].packets_received, 20);
        assert_eq!(snap.peer_health[&2].chunk_evictions, 0);
    }

    #[test]
    fn peer_health_removed_peer_disappears() {
        // If a peer is removed from peer_health (e.g., disconnected),
        // it should no longer appear in the snapshot.
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 50,
                staleness_ticks: 10,
                packets_received: 8,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.peer_health.insert(
            3,
            PeerHealthSnapshot {
                latest_tick: 48,
                staleness_ticks: 12,
                packets_received: 6,
                chunk_evictions: 1,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        // Remove peer 2 (simulates peer disconnect)
        c.peer_health.remove(&2);
        let snap = c.snapshot();
        assert_eq!(snap.peer_health.len(), 1);
        assert!(!snap.peer_health.contains_key(&2));
        assert!(snap.peer_health.contains_key(&3));
    }

    #[test]
    fn udp_bytes_out_large_value() {
        // Verify u64 bytes_out handles large values without truncation.
        let mut c = MetricsCollector::new(1);
        c.udp_bytes_out = 1_000_000_000; // 1 GB
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.udp_bytes_out, 1_000_000_000);

        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"udp_bytes_out\":1000000000"),
            "large bytes_out should serialize: {json}"
        );
    }

    #[test]
    fn snapshot_all_new_fields_present_in_json() {
        // Comprehensive check: every new telemetry field appears in the JSON output.
        let mut c = MetricsCollector::new(1);
        c.udp_repl_drops = 7;
        c.udp_ctrl_drops = 3;
        c.udp_bytes_out = 9999;
        c.peer_health.insert(
            5,
            PeerHealthSnapshot {
                latest_tick: 200,
                staleness_ticks: 10,
                packets_received: 50,
                chunk_evictions: 2,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let required_fields = [
            "udp_repl_drops",
            "udp_ctrl_drops",
            "udp_bytes_out",
            "peer_health",
            "latest_tick",
            "staleness_ticks",
            "packets_received",
            "chunk_evictions",
            "session_id",
        ];
        for field in &required_fields {
            assert!(
                json.contains(field),
                "field '{}' missing from JSON: {json}",
                field
            );
        }
    }

    #[test]
    fn snapshot_deserializes_back() {
        // Verify MetricsSnapshot can round-trip through JSON.
        let mut c = MetricsCollector::new(1);
        c.udp_repl_drops = 3;
        c.udp_ctrl_drops = 1;
        c.udp_bytes_out = 4096;
        c.current_tick = 500;
        c.entity_count = 10;
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 498,
                staleness_ticks: 2,
                packets_received: 40,
                chunk_evictions: 0,
                session_id: 0,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let snap = c.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["udp_repl_drops"], 3);
        assert_eq!(parsed["udp_ctrl_drops"], 1);
        assert_eq!(parsed["udp_bytes_out"], 4096);
        assert_eq!(parsed["peer_health"]["2"]["latest_tick"], 498);
        assert_eq!(parsed["peer_health"]["2"]["staleness_ticks"], 2);
        assert_eq!(parsed["peer_health"]["2"]["packets_received"], 40);
        assert_eq!(parsed["peer_health"]["2"]["chunk_evictions"], 0);
    }

    #[test]
    fn peer_health_empty_serializes_as_empty_object() {
        // No peers -> peer_health should be {} not null.
        let c = MetricsCollector::new(1);
        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["peer_health"].is_object());
        assert_eq!(parsed["peer_health"].as_object().unwrap().len(), 0);
    }

    // =========================================================================
    // WI-136: Session id exposure on /metrics
    // =========================================================================

    #[test]
    fn session_id_defaults_to_zero() {
        // A freshly-constructed collector has session_id == 0 (placeholder
        // until the sim-server boot path calls set_session_id).
        let c = MetricsCollector::new(1);
        assert_eq!(c.session_id(), 0);
        let snap = c.snapshot();
        assert_eq!(snap.session_id, 0);
    }

    #[test]
    fn set_session_id_updates_snapshot_value() {
        let mut c = MetricsCollector::new(1);
        c.set_session_id(0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(c.session_id(), 0xDEAD_BEEF_CAFE_BABE);
        let snap = c.snapshot();
        assert_eq!(snap.session_id, 0xDEAD_BEEF_CAFE_BABE);
    }

    #[test]
    fn snapshot_includes_local_and_per_peer_session_id_in_json() {
        // Exercises the full WI-136 shape: top-level session_id plus a
        // session_id under every peer_health entry. A fresh snapshot
        // (no set_session_id, no peer_sessions populated) still emits
        // session_id: 0 on both paths, which is the acceptable placeholder
        // state for a server that has not yet generated its session id.
        let mut c = MetricsCollector::new(7);
        c.set_session_id(0x0123_4567_89AB_CDEF);
        c.peer_health.insert(
            2,
            PeerHealthSnapshot {
                latest_tick: 100,
                staleness_ticks: 0,
                packets_received: 10,
                chunk_evictions: 0,
                session_id: 0xAAAA_AAAA_AAAA_AAAA,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.peer_health.insert(
            3,
            PeerHealthSnapshot {
                latest_tick: 95,
                staleness_ticks: 5,
                packets_received: 8,
                chunk_evictions: 0,
                session_id: 0xBBBB_BBBB_BBBB_BBBB,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        c.record(timing(5));

        let snap = c.snapshot();
        assert_eq!(snap.session_id, 0x0123_4567_89AB_CDEF);
        assert_eq!(snap.peer_health[&2].session_id, 0xAAAA_AAAA_AAAA_AAAA);
        assert_eq!(snap.peer_health[&3].session_id, 0xBBBB_BBBB_BBBB_BBBB);

        // JSON shape: top-level session_id is a number (u64 decimal).
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["session_id"], 0x0123_4567_89AB_CDEFu64);
        assert_eq!(
            parsed["peer_health"]["2"]["session_id"],
            0xAAAA_AAAA_AAAA_AAAAu64
        );
        assert_eq!(
            parsed["peer_health"]["3"]["session_id"],
            0xBBBB_BBBB_BBBB_BBBBu64
        );
    }

    #[test]
    fn fresh_metrics_snapshot_emits_session_id_fields() {
        // Per batch spec: a fresh metrics snapshot must emit both the local
        // session id and per-peer session ids (even when the per-peer map is
        // empty, the top-level field must still be present).
        let c = MetricsCollector::new(1);
        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Top-level session_id must be present (serde doesn't skip the field).
        assert!(
            parsed.get("session_id").is_some(),
            "top-level session_id field missing from /metrics JSON: {json}"
        );
        assert_eq!(parsed["session_id"], 0);
        // peer_health is an object (possibly empty); when populated, per-entry
        // session_id must be present. Add a peer and re-check.
        let mut c = MetricsCollector::new(1);
        c.peer_health.insert(
            4,
            PeerHealthSnapshot {
                latest_tick: 42,
                staleness_ticks: 0,
                packets_received: 1,
                chunk_evictions: 0,
                session_id: 0xFFFF_0000_FFFF_0000,
                stragglers_dropped: 0,
                session_changed_at_tick: 0,
            },
        );
        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed["peer_health"]["4"].get("session_id").is_some(),
            "per-peer session_id field missing from /metrics JSON: {json}"
        );
        assert_eq!(
            parsed["peer_health"]["4"]["session_id"],
            0xFFFF_0000_FFFF_0000u64
        );
    }
}
