# 011: Network Saturation Telemetry

## Problem Statement

The UNO Q cluster's primary bottleneck is WiFi/UDP saturation. Load testing demonstrated entity breakdown at ~8k entities and peer instability at ~10k. However, there is no way to detect this from telemetry. The `/metrics` endpoint exposes tick timing, entity counts, and replication mode/counts, but zero network health signals.

UDP drops are already tracked in the game loop (`udp_repl_drops`, `udp_ctrl_drops` at game_loop.rs:196-197), but they are only logged to tracing every 5 seconds (lines 520-545) and never sent to `MetricsCollector`. The UDP receiver (`receiver.rs`) processes incoming packets and does chunk reassembly with stale eviction, but tracks nothing about per-peer receive health. The `drain_inbound_events` system processes shadow updates but has no concept of per-peer freshness beyond the shadow component's `last_seen_tick`.

This means operators cannot detect WiFi saturation until it causes functional symptoms: shadow loss, peer disconnects, or client desync. By then the mesh is already degraded. We need proactive signals that show saturation building before it causes problems.

## Design

### Telemetry Signals

Five signal categories, ordered by how early they detect saturation:

**1. UDP Outbound Drops (already tracked, just not exposed)**

`udp_repl_drops` and `udp_ctrl_drops` in game_loop.rs count `try_send` returning `Full` on the mpsc channels to the UDP sender task. This means the game loop is producing UDP work faster than the sender can write to the socket. These counters are already accumulated but only logged -- they need to be written to MetricsCollector each tick and exposed in MetricsSnapshot.

Cost: 2 atomic writes per tick. Negligible.

**2. UDP Outbound Bytes**

Track total bytes enqueued for UDP send per tick. Combined with the tick rate (20Hz), this gives bytes/second outbound. WiFi saturation has a known bandwidth ceiling; comparing actual throughput against that ceiling is diagnostic.

Cost: one `usize` accumulator per tick, incremented per enqueue call. Negligible.

**3. Per-Peer Shadow Freshness**

For each peer, track the latest received tick number from their replication packets. `current_tick - latest_peer_tick` = staleness in ticks. Rising staleness means incoming packets from that peer are being lost or delayed. This is the earliest inbound signal of packet loss.

The receiver already has `latest_tick: HashMap<u16, u64>` (receiver.rs:25) for chunk reassembly eviction. The values need to be forwarded to MetricsCollector. The game loop can compute staleness when recording metrics.

Cost: one HashMap read per peer per metrics snapshot. Negligible.

**4. Per-Peer Receive Rate**

Count packets received from each peer per second. A healthy peer sends at 20Hz. If we see 15Hz, we're losing 25% of inbound packets. Combined with staleness, this distinguishes between "peer is slow" (low rate + low staleness) and "packets are being dropped" (low rate + rising staleness).

The receiver processes all inbound UDP. A per-peer counter incremented on each decoded message, reset every second, gives the rate.

Cost: one HashMap increment per received packet. Negligible.

**5. Chunk Reassembly Failures**

When a chunked entity update starts arriving but some chunks never arrive (packet loss mid-burst), the reassembly entry eventually gets evicted by the periodic sweep or the 2-tick-behind cleanup. Each eviction represents a complete tick of shadow data lost from that peer.

The receiver already logs evictions (receiver.rs:54-58) but doesn't count them. A per-peer eviction counter gives a direct measure of inbound packet loss for large updates.

Cost: one counter increment per eviction. Negligible.

### Data Flow

```
                   game_loop.rs                       MetricsCollector
                   ============                       ================

  udp_repl_drops -----> record in Phase 6 ----------> udp_repl_drops: u64
  udp_ctrl_drops -----> record in Phase 6 ----------> udp_ctrl_drops: u64
  udp_bytes_out  -----> record in Phase 6 ----------> udp_bytes_out: u64


                   receiver.rs                        SharedInbound (new fields)
                   ===========                        =========================

  per-peer latest_tick -----> push to shared -------> peer_health: HashMap<u16, PeerHealthStats>
  per-peer pkt_count   -----> push to shared -------> (aggregated per reporting window)
  chunk evictions      -----> push to shared ------->


                   MetricsCollector                    MetricsSnapshot (JSON)
                   ================                    ====================

  udp_repl_drops ------> snapshot() ----------------> "udp_repl_drops": 0
  udp_ctrl_drops ------> snapshot() ----------------> "udp_ctrl_drops": 0
  udp_bytes_out  ------> snapshot() ----------------> "udp_bytes_out": 12345
  peer_health    ------> snapshot() ----------------> "peer_health": { "2": { ... } }
```

### Where Data Originates

| Signal | Source | Thread | Path to MetricsCollector |
|--------|--------|--------|--------------------------|
| Outbound repl drops | game_loop.rs:196 (`udp_repl_drops`) | Game loop (main) | Direct write in Phase 6 |
| Outbound ctrl drops | game_loop.rs:197 (`udp_ctrl_drops`) | Game loop (main) | Direct write in Phase 6 |
| Outbound bytes | game_loop.rs Phase 4 (enqueue calls) | Game loop (main) | Direct write in Phase 6 |
| Per-peer latest tick | receiver.rs:181 (`latest_tick`) | UDP receiver (tokio task) | Via SharedInbound |
| Per-peer packet count | receiver.rs (new counter) | UDP receiver (tokio task) | Via SharedInbound |
| Chunk evictions | receiver.rs:45-58 (sweep), :185 (inline) | UDP receiver (tokio task) | Via SharedInbound |

### Message Format Changes

#### MetricsSnapshot additions

```rust
pub struct MetricsSnapshot {
    // ... existing fields ...

    /// Cumulative UDP replication channel drops since last snapshot window.
    pub udp_repl_drops: u64,
    /// Cumulative UDP control channel drops since last snapshot window.
    pub udp_ctrl_drops: u64,
    /// UDP bytes enqueued for outbound send this tick.
    pub udp_bytes_out: u64,
    /// Per-peer network health. Key is peer server_id.
    pub peer_health: HashMap<u16, PeerHealthSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct PeerHealthSnapshot {
    /// Latest tick received from this peer.
    pub latest_tick: u64,
    /// Staleness: our current tick minus their latest tick.
    pub staleness_ticks: u64,
    /// Packets received from this peer in the last reporting window.
    pub packets_received: u32,
    /// Chunk reassembly evictions (incomplete chunked updates) in the last reporting window.
    pub chunk_evictions: u32,
}
```

#### SharedInbound additions

The receiver task communicates peer health stats to the game loop through a separate mutex-protected structure (not through the InboundQueue, which is per-tick drained). Peer health is accumulated over time and snapshotted, not queued as events.

```rust
/// Per-peer health statistics accumulated by the UDP receiver.
/// Snapshotted by the game loop for metrics reporting.
pub struct PeerHealthAccumulator {
    /// Latest tick received from each peer.
    pub latest_tick: HashMap<u16, u64>,
    /// Packets received from each peer since last reset.
    pub packets_received: HashMap<u16, u32>,
    /// Chunk evictions per peer since last reset.
    pub chunk_evictions: HashMap<u16, u32>,
}
```

This is a new shared structure, analogous to `SharedInbound` but for health data. Protected by `std::sync::Mutex` (not tokio, matching the project convention from design 005). The game loop reads and resets the counters every metrics recording (Phase 6).

### Per-Tick Cost Analysis

All operations are O(1) per packet or O(P) per metrics snapshot where P is the number of peers (typically 2-10).

| Operation | Per-tick cost | Notes |
|-----------|--------------|-------|
| Increment udp_repl_drops | 1 u64 add | Already happens, just not stored |
| Increment udp_bytes_out | ~(2*P) usize adds | One per enqueue call per peer |
| Record to MetricsCollector | 5 field copies | Phase 6, already takes mutex |
| Read peer health accumulator | 1 mutex lock + HashMap clone | Once per tick in Phase 6 |
| Receiver: increment pkt count | 1 HashMap entry increment | Per received packet |
| Receiver: update latest_tick | 1 HashMap entry write | Already happens for reassembly |
| Receiver: increment evictions | 1 HashMap entry increment | Per eviction (rare) |

Total additional cost per tick: sub-microsecond. Well within the 50ms budget.

Memory: ~64 bytes per peer in PeerHealthAccumulator + ~64 bytes per peer in MetricsSnapshot. At 10 peers: ~1.3 KB. Negligible.

### JSON Output Example

```json
{
  "server_id": 1,
  "tick": 50000,
  "entity_count": 150,
  "shadow_count": 300,
  "peer_count": 2,
  "connected_clients": 1,
  "tick_budget_pct": 42.5,
  "phases": { ... },
  "replication_mode": "tiered",
  "replication_full_count": 150,
  "replication_sent_count": 280,
  "replication_hot_count": 80,
  "replication_cold_count": 200,
  "udp_repl_drops": 0,
  "udp_ctrl_drops": 0,
  "udp_bytes_out": 14200,
  "peer_health": {
    "2": {
      "latest_tick": 49998,
      "staleness_ticks": 2,
      "packets_received": 19,
      "chunk_evictions": 0
    },
    "3": {
      "latest_tick": 49995,
      "staleness_ticks": 5,
      "packets_received": 14,
      "chunk_evictions": 2
    }
  }
}
```

In this example, peer S3 is showing early saturation signs: 5-tick staleness (250ms behind) and only 14 packets received (should be ~20 per second). Two chunk evictions mean two ticks of shadow data were lost from S3 due to incomplete chunked updates.

### Failure Modes

**Receiver crashes**: PeerHealthAccumulator stops updating. latest_tick stays frozen, staleness grows. This is correct behavior -- a dead receiver looks like total packet loss.

**MetricsCollector mutex poisoned**: Already handled by `unwrap_or_else(|e| e.into_inner())` pattern used throughout the codebase.

**PeerHealthAccumulator mutex contention**: The receiver writes on every packet (~60 packets/second with 3 peers at 20Hz). The game loop reads once per tick (20Hz). Contention is minimal. The mutex is held for sub-microsecond HashMap operations.

**Peer removed from mesh**: Stale entries in PeerHealthAccumulator for removed peers. The game loop should filter peer_health output to only include peers currently in PeerAddresses. Stale accumulator entries are harmless (small memory, cleaned up on next metrics snapshot).

**Counter overflow**: u64 for drops and bytes. At 1 billion drops per second (impossible), overflow would take 292 years.

### Dashboard Integration

The board dashboard (metrics_server.rs) currently shows a simple "N peers connected" indicator. With per-peer health data, it could show per-peer status dots (green/yellow/red based on staleness thresholds). This is a follow-up enhancement, not part of this design. The immediate goal is getting the data into the JSON endpoint.

### Testing Plan

1. **Unit tests for MetricsCollector**: Verify new fields default to zero, appear in JSON serialization, accumulate correctly, reset correctly.

2. **Unit tests for PeerHealthAccumulator**: Verify increment/reset cycle, multiple peers tracked independently, eviction counts accumulate.

3. **Integration test**: Start a 3-server cluster, spawn entities, read `/metrics` from each server, verify `peer_health` map contains entries for the other two servers, verify `staleness_ticks` is small (< 10), verify `packets_received` is approximately 20 per second.

4. **Scene test (`network-telemetry`)**: Connect to a running cluster, read metrics, verify all new fields are present and non-null, verify peer_health keys match expected peer server IDs.

### Migration

All new fields are additive to the MetricsSnapshot JSON. Existing consumers (test.sh, board dashboards) that read the JSON will ignore unknown fields. No breaking changes.

The board dashboard HTML (embedded in metrics_server.rs) does not need to change for the telemetry to work -- the data is accessible via `/metrics` JSON. Dashboard visualization is a separate enhancement.
