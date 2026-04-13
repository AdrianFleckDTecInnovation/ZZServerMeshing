use serde::Serialize;
use std::collections::VecDeque;
use std::time::Duration;

const RING_SIZE: usize = 200; // 10 seconds at 20Hz

/// Per-tick timing breakdown.
#[derive(Debug, Clone, Default)]
pub struct TickTimings {
    pub input_drain: Duration,
    pub simulation: Duration,
    pub transfers: Duration,
    pub replication: Duration,
    pub shadow_cleanup: Duration,
    pub total: Duration,
}

/// Collects per-tick timings in a ring buffer and computes percentiles.
pub struct MetricsCollector {
    server_id: u16,
    ring: VecDeque<TickTimings>,
    pub current_tick: u64,
    pub entity_count: usize,
    pub shadow_count: usize,
    pub peer_count: usize,
    pub connected_clients: usize,
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
    pub tick: u64,
    pub entity_count: usize,
    pub shadow_count: usize,
    pub peer_count: usize,
    pub connected_clients: usize,
    pub tick_budget_pct: f64,
    pub phases: PhaseMetrics,
}

#[derive(Debug, Serialize)]
pub struct PhaseMetrics {
    pub input_drain_ms: Percentiles,
    pub simulation_ms: Percentiles,
    pub transfers_ms: Percentiles,
    pub replication_ms: Percentiles,
    pub shadow_cleanup_ms: Percentiles,
    pub total_ms: Percentiles,
}

impl MetricsCollector {
    pub fn new(server_id: u16) -> Self {
        Self {
            server_id,
            ring: VecDeque::with_capacity(RING_SIZE),
            current_tick: 0,
            entity_count: 0,
            shadow_count: 0,
            peer_count: 0,
            connected_clients: 0,
        }
    }

    pub fn record(&mut self, timings: TickTimings) {
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
            tick: self.current_tick,
            entity_count: self.entity_count,
            shadow_count: self.shadow_count,
            peer_count: self.peer_count,
            connected_clients: self.connected_clients,
            tick_budget_pct,
            phases: PhaseMetrics {
                input_drain_ms: self.percentiles(|t| t.input_drain),
                simulation_ms: self.percentiles(|t| t.simulation),
                transfers_ms: self.percentiles(|t| t.transfers),
                replication_ms: self.percentiles(|t| t.replication),
                shadow_cleanup_ms: self.percentiles(|t| t.shadow_cleanup),
                total_ms: self.percentiles(|t| t.total),
            },
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
            transfers: Duration::from_millis(2),
            replication: Duration::from_millis(3),
            shadow_cleanup: Duration::from_millis(1),
            total: Duration::from_millis(12),
        };
        c.record(t);
        let snap = c.snapshot();
        assert!((snap.phases.input_drain_ms.p50 - 1.0).abs() < 0.1);
        assert!((snap.phases.simulation_ms.p50 - 5.0).abs() < 0.1);
        assert!((snap.phases.transfers_ms.p50 - 2.0).abs() < 0.1);
        assert!((snap.phases.replication_ms.p50 - 3.0).abs() < 0.1);
        assert!((snap.phases.shadow_cleanup_ms.p50 - 1.0).abs() < 0.1);
    }
}
