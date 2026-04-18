# 011: Network Saturation Telemetry -- Work Items

Design doc: `docs/mesh-architect/designs/011-network-saturation-telemetry.md`

## Overview

8 work items in 3 phases. Phase 1 exposes data already tracked in game_loop.rs (outbound drops + bytes). Phase 2 adds per-peer inbound health from the receiver. Phase 3 adds the scene test and wires it into the test suite.

Estimated total: 4-6 hours.

---

## Phase 1: Outbound UDP Telemetry (game_loop.rs + metrics.rs)

These fields already exist as local variables in the game loop. The work is plumbing them into MetricsCollector and MetricsSnapshot.

### WI-100: Add outbound drop counters to MetricsCollector and MetricsSnapshot

**Size:** small
**Depends on:** none
**Files:**
- `crates/sim-server/src/metrics.rs`

**Description:**

Add three new fields to `MetricsCollector`:

```rust
pub udp_repl_drops: u64,
pub udp_ctrl_drops: u64,
pub udp_bytes_out: u64,
```

Initialize them to 0 in `MetricsCollector::new()`.

Add matching fields to `MetricsSnapshot`:

```rust
pub udp_repl_drops: u64,
pub udp_ctrl_drops: u64,
pub udp_bytes_out: u64,
```

Wire them through in `snapshot()`:

```rust
udp_repl_drops: self.udp_repl_drops,
udp_ctrl_drops: self.udp_ctrl_drops,
udp_bytes_out: self.udp_bytes_out,
```

**Tests:**

Add to the existing test module in `metrics.rs`:

1. `snapshot_udp_drops_default_zero` -- Fresh collector has all three fields at 0 in snapshot.
2. `snapshot_udp_drops_in_json` -- Set `udp_repl_drops = 5`, `udp_ctrl_drops = 2`, `udp_bytes_out = 12345`. Serialize to JSON, verify all three keys and values appear.
3. `snapshot_udp_drops_accumulate` -- Set values, take snapshot, verify. Set new values, take another snapshot, verify the new values (not accumulated -- the collector stores current values, not deltas).

**Done when:** `cargo test -p sim-server metrics::tests` passes. MetricsSnapshot JSON contains `udp_repl_drops`, `udp_ctrl_drops`, and `udp_bytes_out`.

---

### WI-101: Write outbound drop counters from game loop to MetricsCollector

**Size:** small
**Depends on:** WI-100
**Files:**
- `crates/sim-server/src/game_loop.rs`

**Description:**

In the game loop Phase 6 (metrics recording, around line 472), add writes to the MetricsCollector for the drop counters and bytes.

For drops: the existing `udp_repl_drops` and `udp_ctrl_drops` variables accumulate across the 5-second logging window and are reset to 0 after logging. Change this so the game loop writes the current accumulated value to MetricsCollector every tick (before the 5-second reset). The MetricsCollector stores the running total; the 5-second reset in the log block (line 534, 545) stays as-is for the log output.

Specifically, in Phase 6 inside the `metrics.lock()` block:

```rust
m.udp_repl_drops = udp_repl_drops;
m.udp_ctrl_drops = udp_ctrl_drops;
```

These reflect "drops since last 5-second reset" which is the most useful window for monitoring.

For bytes: add a new local variable `udp_bytes_out: u64 = 0` at the top of the tick loop (near line 205). In the `try_enqueue` helper function (or at each call site), accumulate the byte count of each enqueued message. In Phase 6:

```rust
m.udp_bytes_out = udp_bytes_out;
```

Reset `udp_bytes_out = 0` at the start of each tick (line 392, near `repl_drops_this_tick = 0`).

The byte counting approach: the `enqueue_entities_to_peer` function has access to the serialized `bytes` Vec. Its `.len()` is the byte count. Thread a `&mut u64` bytes accumulator through the function. Alternatively, accumulate at each `try_enqueue` call site. The simplest approach: add a `bytes: usize` field to `UdpOutbound::Send` (but this changes the enum -- avoid). Better: count the bytes in `serialize_versioned` return value `.len()` right before the `try_enqueue` call.

Since `enqueue_entities_to_peer` already handles the serialization and enqueue, the cleanest approach is to add a `bytes_out: &mut u64` parameter to it and to the inline `try_enqueue` calls. Increment `*bytes_out += data.len() as u64` before each `try_enqueue`. This avoids changing the `UdpOutbound` enum.

**Tests:**

This is integration-level (the game loop is async). The unit tests from WI-100 verify the MetricsCollector side. The scene test in WI-107 will verify the end-to-end path. No new unit tests needed here, but verify `cargo build -p sim-server` compiles.

**Done when:** `cargo build -p sim-server` succeeds. The game loop writes `udp_repl_drops`, `udp_ctrl_drops`, and `udp_bytes_out` to MetricsCollector every tick.

---

## Phase 2: Per-Peer Inbound Health (receiver.rs + metrics.rs)

The receiver runs as a separate tokio task. It needs a shared structure to communicate per-peer health data to the game loop, which then writes it to MetricsCollector.

### WI-102: Add PeerHealthAccumulator shared structure

**Size:** small
**Depends on:** none
**Files:**
- `crates/sim-server/src/game_loop.rs` (struct definition and instantiation)

**Description:**

Define `PeerHealthAccumulator` and `SharedPeerHealth` in `game_loop.rs` (alongside the existing `SharedInbound`, `SharedCoordEvents`, etc.):

```rust
/// Per-peer network health statistics accumulated by the UDP receiver.
/// Read and reset by the game loop for metrics reporting.
pub struct PeerHealthAccumulator {
    /// Latest tick received from each peer.
    pub latest_tick: HashMap<u16, u64>,
    /// Packets received from each peer since last reset.
    pub packets_received: HashMap<u16, u32>,
    /// Chunk reassembly evictions per peer since last reset.
    pub chunk_evictions: HashMap<u16, u32>,
}

/// Thread-safe wrapper for PeerHealthAccumulator.
pub struct SharedPeerHealth {
    pub stats: std::sync::Mutex<PeerHealthAccumulator>,
}
```

In `run_game_loop_ecs`, create an `Arc<SharedPeerHealth>` and pass it to:
- The UDP receiver (new parameter on `run_udp_receiver`)
- The game loop itself (for reading in Phase 6)

This requires adding a `peer_health: Arc<SharedPeerHealth>` parameter to `run_game_loop_ecs`. The caller in `main.rs` creates the Arc and passes clones to both the game loop and the receiver.

**Tests:**

1. `peer_health_accumulator_default` -- Default PeerHealthAccumulator has empty maps.
2. `peer_health_accumulator_increment` -- Insert latest_tick for peer 2, increment packets_received, verify.
3. `peer_health_accumulator_multiple_peers` -- Track two peers independently, verify isolation.

**Done when:** `cargo build -p sim-server` succeeds. PeerHealthAccumulator and SharedPeerHealth are defined and instantiated.

---

### WI-103: Instrument the UDP receiver with per-peer health tracking

**Size:** medium
**Depends on:** WI-102
**Files:**
- `crates/sim-server/src/receiver.rs`

**Description:**

Add `peer_health: Arc<SharedPeerHealth>` parameter to `run_udp_receiver`.

Inside the receiver loop, after each successfully decoded `PeerMessage`:

1. **Packet count**: Increment `packets_received` for the source server_id. For `EntityStateUpdate` and `ChunkedEntityUpdate`, the server_id is in the message. For `AuthorityTransfer`, `ForwardedInput`, `ForwardedRemove`, `TransferAck`, use the `from_server` or look up the server_id from the source address. The simplest approach: extract the server_id from each message variant. All variants that carry a server_id use it. For variants without one (ForwardedInput, ForwardedRemove, TransferAck), we can either skip counting (they're rare) or attribute them to a "control" bucket. Simplest: skip counting for these -- they're rare and not relevant to saturation detection.

   Specifically, for `EntityStateUpdate { server_id, .. }` and `ChunkedEntityUpdate { server_id, .. }`, these are the bulk replication packets (20Hz). Increment:
   ```rust
   let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
   *ph.packets_received.entry(server_id.0).or_insert(0) += 1;
   ```

2. **Latest tick**: The receiver already tracks `latest_tick: HashMap<u16, u64>` locally for chunk reassembly. After updating the local `latest_tick`, also update the shared accumulator:
   ```rust
   ph.latest_tick.insert(server_id.0, tick);
   ```
   This can be done in the same lock acquisition as the packet count.

3. **Chunk evictions**: In the periodic sweep (receiver.rs:43-59), after computing `evicted`, attribute evictions to their peers:
   ```rust
   // Before the retain call, collect which entries will be evicted
   let evicted_entries: Vec<(u16, u64)> = reassembly.iter()
       .filter(|&(&(sid, t), _)| {
           if let Some(&server_latest) = latest_tick.get(&sid) {
               server_latest.saturating_sub(t) > GLOBAL_STALE_TICKS
           } else {
               false
           }
       })
       .map(|(&k, _)| k)
       .collect();

   // Count evictions per peer
   if !evicted_entries.is_empty() {
       let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
       for &(sid, _) in &evicted_entries {
           *ph.chunk_evictions.entry(sid).or_insert(0) += 1;
       }
   }

   // Then do the actual retain
   reassembly.retain(...);
   ```

   Also count the inline eviction at receiver.rs:185 (the `tick - t <= 2` cleanup):
   ```rust
   let before_inline = reassembly.len();
   reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
   let inline_evicted = before_inline - reassembly.len();
   if inline_evicted > 0 {
       let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
       *ph.chunk_evictions.entry(server_id.0).or_insert(0) += inline_evicted as u32;
   }
   ```

   And the hard cap eviction at receiver.rs:62-75 (attribute to evicted peer IDs).

**Important**: Minimize mutex hold time. Acquire the lock once per packet for the common path (packet count + latest_tick update). The eviction path is rare (every 100 iterations for the sweep, occasional for inline cleanup) so separate lock acquisitions there are fine.

**Tests:**

Unit testing the receiver is difficult because it's an async loop over a UDP socket. The tests for this are covered by the scene test in WI-107. Verify `cargo build -p sim-server` compiles.

For manual verification: start a 3-server cluster, check logs for the existing reassembly sweep messages, then read `/metrics` to confirm peer health data appears.

**Done when:** `cargo build -p sim-server` succeeds. The receiver updates PeerHealthAccumulator on every replication packet and on every chunk eviction.

---

### WI-104: Add PeerHealthSnapshot to MetricsCollector and MetricsSnapshot

**Size:** small
**Depends on:** WI-100
**Files:**
- `crates/sim-server/src/metrics.rs`

**Description:**

Add the per-peer health snapshot type:

```rust
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct PeerHealthSnapshot {
    /// Latest tick received from this peer.
    pub latest_tick: u64,
    /// How many ticks behind our current tick this peer's latest data is.
    pub staleness_ticks: u64,
    /// Packets received from this peer in the current reporting window (~5s).
    pub packets_received: u32,
    /// Incomplete chunk reassemblies evicted for this peer in the current reporting window.
    pub chunk_evictions: u32,
}
```

Add to `MetricsCollector`:

```rust
/// Per-peer health data. Written by game loop from PeerHealthAccumulator.
pub peer_health: HashMap<u16, PeerHealthSnapshot>,
```

Initialize as `HashMap::new()` in `new()`.

Add to `MetricsSnapshot`:

```rust
pub peer_health: HashMap<u16, PeerHealthSnapshot>,
```

Wire through in `snapshot()`:

```rust
peer_health: self.peer_health.clone(),
```

**Tests:**

1. `snapshot_peer_health_default_empty` -- Fresh collector has empty peer_health map.
2. `snapshot_peer_health_in_json` -- Insert a PeerHealthSnapshot for peer 2, serialize to JSON, verify the structure appears with correct keys and values.
3. `snapshot_peer_health_multiple_peers` -- Insert entries for peers 2 and 3, verify both appear in JSON output.

**Done when:** `cargo test -p sim-server metrics::tests` passes. MetricsSnapshot JSON contains `peer_health` map.

---

### WI-105: Wire PeerHealthAccumulator into game loop Phase 6

**Size:** medium
**Depends on:** WI-102, WI-103, WI-104
**Files:**
- `crates/sim-server/src/game_loop.rs`
- `crates/sim-server/src/main.rs` (pass Arc to both tasks)

**Description:**

In `run_game_loop_ecs`, add the `peer_health: Arc<SharedPeerHealth>` parameter.

In Phase 6 (metrics recording), after locking the MetricsCollector, also lock the PeerHealthAccumulator and build the per-peer health map:

```rust
// Read peer health from receiver
let current_tick = world.resource::<TickCounter>().0;
let active_peers: HashSet<u16> = world.resource::<PeerAddresses>().0.keys().copied().collect();
{
    let ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
    let mut health_map = HashMap::new();
    for &peer_id in &active_peers {
        let latest = ph.latest_tick.get(&peer_id).copied().unwrap_or(0);
        let staleness = if latest > 0 {
            current_tick.saturating_sub(latest)
        } else {
            u64::MAX // never received from this peer
        };
        health_map.insert(peer_id, PeerHealthSnapshot {
            latest_tick: latest,
            staleness_ticks: staleness,
            packets_received: ph.packets_received.get(&peer_id).copied().unwrap_or(0),
            chunk_evictions: ph.chunk_evictions.get(&peer_id).copied().unwrap_or(0),
        });
    }
    m.peer_health = health_map;
}
```

For the counter reset: reset `packets_received` and `chunk_evictions` every 5 seconds (align with the existing 5-second debug logging block at game_loop.rs:509). Inside that block, after the drop logging:

```rust
// Reset peer health counters every 5s reporting window
{
    let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
    ph.packets_received.clear();
    ph.chunk_evictions.clear();
    // Don't clear latest_tick -- it's a high-water mark, not a counter
}
```

This means `packets_received` shows "packets in the last 5 seconds" and `chunk_evictions` shows "evictions in the last 5 seconds". At 20Hz with 2 peers, healthy values are ~100 packets per peer per 5-second window.

In `main.rs`, update the call sites:

1. Create `Arc<SharedPeerHealth>` before spawning tasks.
2. Pass `Arc::clone(&peer_health)` to `run_udp_receiver`.
3. Pass `Arc::clone(&peer_health)` to `run_game_loop_ecs`.

**Tests:**

Compile-level verification: `cargo build -p sim-server`. The game loop integration test (if any exist in game_loop.rs tests) should still pass. The full end-to-end verification is in WI-107.

**Done when:** `cargo build -p sim-server` succeeds. The game loop reads PeerHealthAccumulator and writes PeerHealthSnapshot to MetricsCollector every tick. Counters reset every 5 seconds.

---

## Phase 3: Scene Test and Verification

### WI-106: Update existing game_loop.rs tests for new metrics fields

**Size:** small
**Depends on:** WI-100, WI-101
**Files:**
- `crates/sim-server/src/game_loop.rs` (test section)

**Description:**

The game_loop.rs test module has helper functions like `setup_world()` and tests that create a MetricsCollector. Update these:

1. Any test that constructs a `MetricsCollector::new(N)` and calls `.snapshot()` should verify the new fields default to zero.

2. The `run_game_loop_ecs` signature changed (new `peer_health` parameter). If any integration-style tests call it directly, update them to pass a dummy `Arc<SharedPeerHealth>`.

3. If the `setup_world` helper or any test constructs an `Arc<std::sync::Mutex<MetricsCollector>>`, no changes needed -- the new fields auto-default.

Verify all existing tests still pass after the changes.

**Tests:**

Run `cargo test -p sim-server game_loop::tests` and `cargo test -p sim-server metrics::tests`. All must pass.

**Done when:** All existing tests pass with the new code. No regressions.

---

### WI-107: Add network-telemetry scene test

**Size:** medium
**Depends on:** WI-105, WI-106
**Files:**
- `tests/scenes/` (new test script or addition to existing scene test)

**Description:**

Add a scene test that verifies the network telemetry fields are present and populated in a running cluster.

The test should:

1. Start a 3-server cluster (or use `--no-start` with an existing one).
2. Wait for mesh to stabilize (5-10 seconds for peer discovery and initial replication).
3. Read `/metrics` from each of the 3 sim-servers (ports 8101, 8102, 8103).
4. Verify for each server:
   - `udp_repl_drops` field exists and is a number (>= 0).
   - `udp_ctrl_drops` field exists and is a number (>= 0).
   - `udp_bytes_out` field exists and is a number (> 0 if entities exist).
   - `peer_health` field exists and is an object.
   - `peer_health` has entries for the other 2 servers (e.g., S1's peer_health has keys "2" and "3").
   - Each peer health entry has `latest_tick`, `staleness_ticks`, `packets_received`, `chunk_evictions`.
   - `staleness_ticks` for each peer is < 50 (< 2.5 seconds, indicating healthy communication).
   - `packets_received` is > 0 for each peer (indicating packets are flowing).
5. Spawn some entities (via HTTP or by hitting the test spawn endpoint) to ensure `udp_bytes_out` is nonzero.
6. Report pass/fail with details.

Implement as a bash script in the `tests/scenes/` directory, using `curl` and `jq` for JSON parsing (matching the style of existing scene tests).

If there's an existing `mesh-verify` or `network-telemetry` scenario in test.sh, integrate there. Otherwise, add as a new scenario.

**Tests:**

Run the scene test against a local 3-server cluster:
```sh
./test-scene.sh network-telemetry
```

Also verify it works with `--no-start` against an already-running cluster.

**Done when:** The scene test passes on a local 3-server cluster. All existing scene tests still pass (no regressions from the metrics changes).

---

### WI-108: Update roadmap

**Size:** small
**Depends on:** WI-107
**Files:**
- `docs/mesh-architect/roadmap.md`

**Description:**

Add entry 011 to the roadmap under "Active" with status "Implemented". Move to "Completed" section after all WIs are verified.

**Done when:** Roadmap reflects the current state of network saturation telemetry.

---

## Dependency Graph

```
WI-100 (metrics fields) -----> WI-101 (game loop writes drops/bytes)
                          |
                          +--> WI-104 (peer health in metrics) ---+
                                                                   |
WI-102 (shared struct) -----> WI-103 (instrument receiver) ------+---> WI-105 (wire into Phase 6)
                                                                         |
WI-100 + WI-101 ---------> WI-106 (fix existing tests) --------+        |
                                                                 |        |
                                                                 +--------+--> WI-107 (scene test)
                                                                                    |
                                                                                    v
                                                                              WI-108 (roadmap)
```

**Parallelizable:**
- WI-100 and WI-102 have no dependencies and can be done simultaneously.
- WI-101 and WI-104 can be done simultaneously (after WI-100).
- WI-103 can be done after WI-102 (independent of WI-100/WI-101).

**Critical path:** WI-100 -> WI-104 -> WI-105 -> WI-107 (or WI-102 -> WI-103 -> WI-105 -> WI-107).

## Summary

| WI | Title | Size | Depends on |
|----|-------|------|-----------|
| WI-100 | Add outbound drop counters to MetricsCollector/Snapshot | small | none |
| WI-101 | Write outbound drops from game loop to MetricsCollector | small | WI-100 |
| WI-102 | Add PeerHealthAccumulator shared structure | small | none |
| WI-103 | Instrument UDP receiver with per-peer health tracking | medium | WI-102 |
| WI-104 | Add PeerHealthSnapshot to MetricsCollector/Snapshot | small | WI-100 |
| WI-105 | Wire PeerHealthAccumulator into game loop Phase 6 | medium | WI-102, WI-103, WI-104 |
| WI-106 | Update existing game_loop.rs tests for new fields | small | WI-100, WI-101 |
| WI-107 | Add network-telemetry scene test | medium | WI-105, WI-106 |
| WI-108 | Update roadmap | small | WI-107 |
