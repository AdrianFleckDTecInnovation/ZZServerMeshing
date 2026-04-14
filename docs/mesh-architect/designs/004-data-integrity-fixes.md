# 004: Data Integrity Fixes (Delta Compression and Baseline Management)

**Status:** Designed, ready for implementation
**Date:** 2026-04-13
**Audit source:** `docs/mesh-auditor/findings-review.md` Phase 1

This document covers 7 data integrity findings from the audit review, all related to the delta compression and baseline management system. Three are already fixed in code; four remain open.

---

## Findings Status Assessment

Before designing fixes, the current code was compared against each finding. Three of the seven findings have already been addressed:

| # | Title | Status | Evidence |
|---|-------|--------|----------|
| **018** | Baseline drift on dropped frames | **FIXED** | `systems.rs:1181-1234` -- baseline updates after `try_send` succeeds; cleared on drop |
| **023** | Ghost entities from baseline drift | **FIXED** | Server-side: #018 fix prevents drift. Client-side: `world-view.ts:266-279` -- staleness sweep at 6s in `interpolate()` |
| **024** | Full snapshot cloned per client | **FIXED** | `resources.rs:14-18` -- `ClientFrame::Shared(Arc<Vec<u8>>)` enum. `systems.rs:1177,1191` -- Arc-wrapped snapshot, Arc::clone per client |
| **021** | ACK infrastructure unused | **OPEN** | `resources.rs:199` -- `acked_tick` has `#[allow(dead_code)]`. Never read in `prepare_client_snapshot` |
| **022** | Change threshold per-component | **OPEN** | `delta.rs:112-116` -- per-component `abs() > 0.01`, not vector distance |
| **027** | NPC movement defeats delta | **OPEN** | No fallback heuristic in `prepare_client_snapshot` when most entities changed |
| **025** | ACK parsing no tick validation | **OPEN** | `network.rs:173-177` -- accepts any u64 tick value |

The remaining design covers the 4 open findings: #021, #022, #027, and #025.

---

## Issue 1: Wire ACKs into Baseline Management (#021)

### Problem Statement

The client sends a binary ACK (0x03 frame with tick number) after every received frame. The server stores it in an `AtomicU64` per client. But `prepare_client_snapshot` never reads this value. The baseline advances optimistically after `try_send` succeeds rather than waiting for client confirmation.

This matters because `try_send` succeeding means the frame was queued into the per-client mpsc channel -- not that the client received and applied it. If the WebSocket connection drops a frame after the mpsc succeeds (browser tab backgrounding, JS exception during apply, etc.), the baseline diverges from the client's actual state. Recovery only comes at the next periodic full snapshot (5 seconds).

The ACK infrastructure is pure overhead without function: 9 bytes per frame from every client, AtomicU64 writes, Arc sharing -- all for a value nobody reads.

### Design

Wire the `acked_tick` into `prepare_client_snapshot` as an additional full-snapshot trigger. The baseline management strategy remains optimistic (advance on successful `try_send`) because that is correct for the common case (TCP-backed WebSocket delivers reliably). The ACK adds a safety net for the uncommon case.

**Algorithm change in `prepare_client_snapshot`:**

The current full-snapshot triggers are:
1. `handle.baseline.is_none()` -- no baseline yet (new client)
2. `handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL` -- periodic 5-second refresh

Add a third trigger:
3. `acked_tick` has not advanced for `ACK_STALE_TICKS` (40 ticks = 2 seconds) while deltas have been sent

This detects clients that are receiving frames via the mpsc channel (try_send succeeds) but are not processing them (no ACK comes back). The most common cause is browser tab backgrounding, where the WebSocket buffers frames but the JS event loop does not execute.

**New fields on `ClientHandle`:**

```rust
pub struct ClientHandle {
    pub tx: mpsc::Sender<ClientFrame>,
    pub player_id: Option<EntityId>,
    pub acked_tick: Arc<AtomicU64>,
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub ticks_since_full: u32,
    pub last_sent_tick: u64,       // NEW: tick of most recently sent frame
}
```

**Decision logic (pseudocode):**

```
let acked = handle.acked_tick.load(Relaxed);
let stale_ack = handle.last_sent_tick > 0
    && acked < handle.last_sent_tick
    && (current_tick - handle.last_sent_tick) > ACK_STALE_TICKS;

let send_full = handle.baseline.is_none()
    || handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL
    || stale_ack;
```

When `stale_ack` triggers a full snapshot, the baseline is rebuilt from scratch. This ensures convergence even if the client missed several delta frames.

**Constant:**

```rust
/// Ticks without ACK before forcing a full snapshot. 40 ticks = 2 seconds at 20Hz.
/// Shorter than FULL_SNAPSHOT_INTERVAL (100 ticks = 5s) to provide faster recovery.
const ACK_STALE_TICKS: u64 = 40;
```

**Why not ACK-gated baselines?**

A stricter design would only advance the baseline when the ACK confirms receipt. This is the approach described in the #021 finding (Option B). We avoid it because:

1. It requires a "pending baseline" that doubles per-client memory.
2. Under normal operation (TCP-backed WebSocket), every frame is delivered, so the pending baseline is promoted every tick -- pure overhead.
3. The optimistic approach with ACK-based full-snapshot fallback provides the same convergence guarantee with simpler code.
4. If we later move to WebRTC data channels (unreliable transport), we can revisit this decision.

### Files Changed

- `crates/sim-server/src/resources.rs` -- add `last_sent_tick` to `ClientHandle`, add `ACK_STALE_TICKS` constant
- `crates/sim-server/src/systems.rs` -- read `acked_tick` in `prepare_client_snapshot`, add stale-ACK trigger

### Performance Cost

Reading an `AtomicU64` with `Relaxed` ordering is a single load instruction. Per-client, per-tick cost: effectively zero (no contention, no memory barriers). For 50 clients: 50 loads -- negligible.

### Failure Modes

- **Client sends ACKs but server never reads them (current state):** No change from today until this fix ships. After the fix, ACKs are read and used.
- **Client stops sending ACKs (tab backgrounded, JS crash):** After 2 seconds, server forces a full snapshot. On the next frame the client processes, it gets a complete world state. Recovery is bounded at 2 seconds instead of 5.
- **ACK arrives late (network jitter):** The stale-ACK check requires the gap to persist for 40 ticks. A single late ACK resets the counter. False positives are unlikely.
- **Client sends fake ACKs (malicious):** The ACK only skips full-snapshot forcing. A malicious client sending high ACK ticks would prevent the stale-ACK safety net, but the periodic full snapshot at 5 seconds still fires. Combined with #025 (tick validation), this is bounded.

---

## Issue 2: Vector Distance for Change Detection (#022)

### Problem Statement

Delta compression uses per-component comparison: `(a.x - b.x).abs() > 0.01 || (a.y - b.y).abs() > 0.01 || (a.z - b.z).abs() > 0.01`. An entity moving diagonally at 0.007 per axis has a total displacement of ~0.012 world units (above what should be considered noise), but each individual component is below 0.01, so the change is suppressed.

### Design

Replace per-component comparison with squared distance comparison. This treats all movement directions equally and is computationally cheaper (no per-component branches).

**Before (delta.rs:112-116):**
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    (a.x - b.x).abs() > CHANGE_THRESHOLD
        || (a.y - b.y).abs() > CHANGE_THRESHOLD
        || (a.z - b.z).abs() > CHANGE_THRESHOLD
}
```

**After:**
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) > CHANGE_THRESHOLD_SQ
}
```

**Constants:**
```rust
const CHANGE_THRESHOLD: f32 = 0.01;
const CHANGE_THRESHOLD_SQ: f32 = CHANGE_THRESHOLD * CHANGE_THRESHOLD; // 0.0001
```

`distance_squared` uses `glam::Vec3::distance_squared`, which computes `(a-b).length_squared()` -- three subtractions, three multiplications, two additions. No square root. This is the same computational cost as three `abs()` comparisons and often faster because it avoids branching.

### Behavioral Change

This is a relaxation of the threshold for axis-aligned movement and a tightening for diagonal movement:

| Direction | Per-component threshold | Vector distance threshold |
|-----------|------------------------|--------------------------|
| Axis-aligned (X only) | Triggers at 0.0101 on X | Triggers at 0.0101 on X |
| Diagonal (X+Z equal) | Triggers at 0.0101 per axis (0.0143 total) | Triggers at 0.0101 total (0.00714 per axis) |
| All three axes equal | Triggers at 0.0101 per axis (0.0175 total) | Triggers at 0.0101 total (0.00583 per axis) |

For axis-aligned movement, behavior is identical. For diagonal movement, the vector distance threshold catches smaller per-axis changes because it considers the combined displacement. This is correct -- a 0.012 world-unit diagonal move should be treated the same as a 0.012 axis-aligned move.

**Impact on delta sizes:** Slightly more entities may be included in deltas for diagonal movement. In practice this is negligible because NPC speeds (1-3 units/sec at 20Hz = 0.05-0.15 per tick) are well above the threshold in any direction.

### Files Changed

- `crates/sim-server/src/delta.rs` -- modify `vec3_changed`, add `CHANGE_THRESHOLD_SQ`

### Testing

Update the existing threshold boundary tests. Add a test for diagonal movement at the boundary. The existing `below_threshold_not_in_delta` and `above_threshold_in_delta` tests need updating since they test axis-aligned movement and the threshold behavior is unchanged for that case. Add a new test for diagonal movement that was previously suppressed but should now trigger.

---

## Issue 3: Delta Fallback Heuristic (#027)

### Problem Statement

NPCs move every tick (velocity applied by physics integration). With most entities being NPCs, nearly every entity changes every tick, making delta frames as large as (or 4 bytes larger than) full snapshots. The delta computation (HashMap lookups, HashSet construction) adds CPU cost for zero bandwidth benefit.

### Design

Add a heuristic after computing the delta: if >80% of entities are in the update set, discard the delta and send a full snapshot instead. This avoids the overhead of encoding a delta that is larger than a full snapshot.

**Implementation in `prepare_client_snapshot`:**

```rust
let delta = compute_delta(&entities, baseline);
let change_ratio = if entities.is_empty() {
    0.0
} else {
    delta.updates.len() as f32 / entities.len() as f32
};

if change_ratio > DELTA_FALLBACK_THRESHOLD {
    // Too many changes -- full snapshot is smaller and cheaper to encode
    let frame = ClientFrame::Shared(Arc::clone(&full_frame));
    match handle.tx.try_send(frame) {
        Ok(()) => {
            handle.baseline = Some(build_baseline(&entities));
            handle.ticks_since_full = 1;
        }
        // ... error handling same as existing full snapshot path
    }
} else {
    // Normal delta path
    let frame = ClientFrame::Owned(encode_binary_delta(...));
    // ...
}
```

**Constant:**
```rust
/// When this fraction of entities changed, send full snapshot instead of delta.
/// Full snapshot has a 4-byte smaller header (27 vs 31), so it is always smaller
/// when all entities changed. At 80%, the delta has ~20% fewer entity bytes but
/// the header overhead and removal UUIDs make it a wash. 80% is the breakpoint.
const DELTA_FALLBACK_THRESHOLD: f32 = 0.80;
```

**Why 80%?**

- Full snapshot: 27 header + N * 47 entity bytes
- Delta with all N entities changed: 31 header + N * 47 entity bytes + M * 16 removal bytes

At 100% change rate with zero removals, the delta is 4 bytes larger. At 80% change rate with zero removals, the delta saves 20% of entity bytes (~9.4 bytes per entity at 47 bytes each) but adds 4 header bytes. The breakeven is around 1 entity (4 / 47 * N < 0.2 * N), so the delta wins as soon as there are a few unchanged entities. But the CPU cost of the delta computation matters on the boards -- at 80%+ changes, the HashMap lookups provide negligible bandwidth savings while consuming measurable CPU time.

**When does this trigger?**

In the current codebase, it triggers on nearly every tick because all NPCs move every tick. Once interest management filters entities per-client, it will trigger less often. This is the intended behavior -- the heuristic correctly identifies when delta compression is not worth the CPU cost.

### Files Changed

- `crates/sim-server/src/systems.rs` -- add fallback check in `prepare_client_snapshot` delta path
- `crates/sim-server/src/delta.rs` -- add `DELTA_FALLBACK_THRESHOLD` constant (or put it in systems.rs since it is a system-level policy)

### Performance Impact

When the fallback triggers, the server skips `encode_binary_delta` (building the delta Vec) and instead sends the already-computed `Arc<Vec<u8>>` full snapshot. This saves:
- Delta encoding time: ~5-10us per client per tick
- The extra allocation for the delta `Vec<u8>`

The `compute_delta` call still happens (to determine the change ratio), but this is unavoidable. The cost is the HashMap lookups (~10-20us for 500 entities), which is spent before the decision can be made. A future optimization could compute the change count without building the full DeltaResult, but that is not worth the complexity now.

---

## Issue 4: ACK Tick Validation (#025)

### Problem Statement

The binary ACK parser in `network.rs` accepts any u64 tick value without validation. A client can send tick `u64::MAX`, which -- once ACKs are wired into baseline management (#021) -- would trick the server into thinking the client has confirmed the most recent state.

### Design

Add monotonicity and bounds checking to the ACK parser. This must be done after #021 wires ACKs into baseline management, because the validation uses the server's current tick which is only meaningful once ACKs affect behavior.

**Before (network.rs:172-178):**
```rust
Some(Ok(Message::Binary(data))) => {
    if data.len() == 9 && data[0] == 0x03 {
        let tick = u64::from_le_bytes(
            data[1..9].try_into().unwrap_or([0; 8]),
        );
        acked_tick.store(tick, Ordering::Relaxed);
    }
}
```

**After:**
```rust
Some(Ok(Message::Binary(data))) => {
    if data.len() == 9 && data[0] == 0x03 {
        let tick = u64::from_le_bytes(
            data[1..9].try_into().unwrap_or([0; 8]),
        );
        let prev = acked_tick.load(Ordering::Relaxed);
        // Only accept if monotonically increasing and not from the future.
        // server_tick is passed into handle_client or read from a shared atomic.
        if tick > prev && tick <= server_tick.load(Ordering::Relaxed) {
            acked_tick.store(tick, Ordering::Relaxed);
        }
    }
}
```

**Accessing the server tick from handle_client:**

The `handle_client` function runs as an async task, not in the ECS. It does not have access to `TickCounter`. Two options:

1. **Shared AtomicU64 for server tick.** Create an `Arc<AtomicU64>` that the game loop updates each tick. Pass it to `run_websocket_server`. Each `handle_client` task reads it for validation.

2. **Validate in the ECS system instead.** The ACK store remains unvalidated in `handle_client`, but `prepare_client_snapshot` validates when reading: `let acked = handle.acked_tick.load(Relaxed).min(current_tick)`. This is simpler and keeps the validation close to the consumer.

**Option 2 is recommended.** It avoids adding another shared atomic and another parameter to `run_websocket_server`. The validation is a single `min()` call in the system.

**Monotonicity check stays in `handle_client`:** The `tick > prev` check prevents a stale/reordered ACK from regressing the value. This is cheap and correct regardless of server tick access.

### Files Changed

- `crates/sim-server/src/network.rs` -- add monotonicity check (`tick > prev`)
- `crates/sim-server/src/systems.rs` -- clamp acked_tick to current_tick when reading in `prepare_client_snapshot`

### Testing

- Unit test: send ACK with tick 100, then tick 50. Verify AtomicU64 stays at 100 (monotonicity).
- Unit test: send ACK with tick 0. Verify it is accepted only if prev is 0 (edge case: first ACK can be 0 if client sends tick 0). Actually, the first valid tick is 1 (server starts at tick 0 and increments before first frame). Add `tick > 0` to the validation.
- System test: verify `prepare_client_snapshot` clamps acked_tick to current_tick, so a malicious client sending tick u64::MAX gets clamped.

---

## Cross-Cutting Concerns

### Dependency Order

```
#022 (vec3 distance)    -- independent, no deps
#027 (delta fallback)   -- independent, no deps
#021 (wire ACKs)        -- independent, but #025 should follow
#025 (ACK validation)   -- depends on #021 (validation only matters once ACKs are used)
```

All four can be implemented in any order, but #025 should not ship before #021 because adding validation to an unused code path is pointless churn. If both are implemented together, the validation is immediately meaningful.

### Backward Compatibility

None of these changes affect the wire protocol. The binary frame formats (0x01, 0x02, 0x03) are unchanged. The delta fallback sends a full snapshot (0x01) where a delta (0x02) would have been sent -- the client already handles both. Existing clients work without changes.

### Risk Assessment

- **#022 (vec3 distance):** Low risk. Behavioral change is minimal and only affects sub-threshold diagonal movement. Existing tests provide good coverage.
- **#027 (delta fallback):** Low risk. Falls back to a code path (full snapshot) that is already exercised every 100 ticks. If the heuristic misfires, the worst case is sending a few extra full snapshots (slightly more bandwidth, less CPU).
- **#021 (wire ACKs):** Medium risk. Introduces a new full-snapshot trigger. If the stale-ACK detection has false positives (ACK latency > 2 seconds under normal conditions), clients get unnecessary full snapshots. Mitigated by the 40-tick threshold, which is generous for a 20Hz loop.
- **#025 (ACK validation):** Low risk. Only affects the ACK store path. Worst case: a valid ACK is rejected (client experiences a delayed full snapshot). Mitigated by the periodic full snapshot safety net.

---

## Summary

| # | Title | Action | Complexity | Risk |
|---|-------|--------|------------|------|
| 018 | Baseline drift | Already fixed | -- | -- |
| 023 | Ghost entities | Already fixed | -- | -- |
| 024 | Full snapshot clone | Already fixed | -- | -- |
| 021 | Wire ACKs | Implement | Small | Medium |
| 022 | Vec3 distance | Implement | Small | Low |
| 027 | Delta fallback | Implement | Small | Low |
| 025 | ACK validation | Implement | Small | Low |
