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
