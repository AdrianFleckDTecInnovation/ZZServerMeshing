# 004: Data Integrity Fixes -- Work Items

Design: `designs/004-data-integrity-fixes.md`

Total work items: 5 (4 implementation + 1 verification)
Estimated total: 4-6 hours

**Note:** Findings #018, #023, and #024 are already fixed in code. This work item list covers the 4 remaining open findings plus a verification pass.

---

### WI-020: Vector distance for change detection (#022)
**Size:** small (< 1 hour)
**Depends on:** none
**Files:**
- `crates/sim-server/src/delta.rs`

**Description:**
Replace the per-component change threshold in `vec3_changed` with a squared vector distance comparison.

Changes to `delta.rs`:

1. Add a derived constant below `CHANGE_THRESHOLD`:
```rust
const CHANGE_THRESHOLD: f32 = 0.01;
const CHANGE_THRESHOLD_SQ: f32 = CHANGE_THRESHOLD * CHANGE_THRESHOLD;
```

2. Replace `vec3_changed` (lines 112-116):
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) > CHANGE_THRESHOLD_SQ
}
```

This is a single function change. The `entity_changed` function that calls `vec3_changed` does not need modification -- it already calls `vec3_changed` separately for position and velocity.

**Tests:**

Update existing tests in `delta.rs`:

1. Update `below_threshold_not_in_delta`: The test moves by 0.005 on X axis only. With vector distance, 0.005^2 = 0.000025 < 0.0001. Still below threshold. Test passes unchanged.

2. Update `above_threshold_in_delta`: The test moves by 0.02 on X axis. 0.02^2 = 0.0004 > 0.0001. Still above threshold. Test passes unchanged.

3. Update `threshold_boundary_exactly_at_threshold`: Moves by exactly 0.01 on X. 0.01^2 = 0.0001. Comparison is `> 0.0001`, which is false. Test passes unchanged (strict `>` behavior preserved).

4. Update `threshold_boundary_just_above`: Moves by 0.011 on X. 0.011^2 = 0.000121 > 0.0001. Test passes unchanged.

5. Add new test `diagonal_movement_above_threshold`:
```rust
#[test]
fn diagonal_movement_above_threshold() {
    let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
    let baseline = build_baseline(&entities);

    // Move by 0.008 on X and Z. Per-component: each is below 0.01.
    // Vector distance: sqrt(0.008^2 + 0.008^2) = 0.01131 > 0.01.
    // Squared: 0.000064 + 0.000064 = 0.000128 > 0.0001.
    let mut current = entities.clone();
    current[0].position.x += 0.008;
    current[0].position.z += 0.008;

    let delta = compute_delta(&current, &baseline);
    assert_eq!(delta.updates.len(), 1,
        "diagonal movement above vector threshold should produce update");
}
```

6. Add new test `diagonal_movement_below_threshold`:
```rust
#[test]
fn diagonal_movement_below_threshold() {
    let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
    let baseline = build_baseline(&entities);

    // Move by 0.005 on X and Z.
    // Squared: 0.000025 + 0.000025 = 0.000050 < 0.0001.
    let mut current = entities.clone();
    current[0].position.x += 0.005;
    current[0].position.z += 0.005;

    let delta = compute_delta(&current, &baseline);
    assert_eq!(delta.updates.len(), 0,
        "diagonal movement below vector threshold should not produce update");
}
```

7. Update `velocity_below_threshold_ignored`: Moves velocity by 0.005 on X. 0.005^2 = 0.000025 < 0.0001. Test passes unchanged.

**Done when:**
- `vec3_changed` uses `distance_squared` instead of per-component `abs()`.
- All existing threshold tests pass.
- New diagonal movement tests pass.
- `cargo test -p sim-server` passes.

---

### WI-021: Delta fallback when most entities changed (#027)
**Size:** small (< 1 hour)
**Depends on:** none
**Files:**
- `crates/sim-server/src/systems.rs`

**Description:**
After computing a delta in `prepare_client_snapshot`, check if the change ratio exceeds 80%. If so, send a full snapshot instead of the delta. This avoids encoding a delta frame that is as large as or larger than a full snapshot.

Changes to `systems.rs` in `prepare_client_snapshot`, in the `else` (delta) branch after line 1211:

```rust
// Delta path: compute delta against baseline, then decide if delta is worth it.
let baseline = handle.baseline.as_ref().unwrap();
let delta = compute_delta(&entities, baseline);

// Heuristic: if >80% of entities changed, full snapshot is cheaper.
let change_ratio = if entities.is_empty() {
    0.0
} else {
    delta.updates.len() as f32 / entities.len() as f32
};

if change_ratio > DELTA_FALLBACK_THRESHOLD {
    // Too many changes -- send full snapshot instead
    let frame = ClientFrame::Shared(Arc::clone(&full_frame));
    match handle.tx.try_send(frame) {
        Ok(()) => {
            handle.baseline = Some(build_baseline(&entities));
            handle.ticks_since_full = 1;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(client_id = _client_id,
                "Client frame channel full, dropping fallback full snapshot");
            handle.baseline = None;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
} else {
    // Normal delta path (existing code)
    let removal_ids: Vec<EntityId> = delta.removals.clone();
    let frame = ClientFrame::Owned(encode_binary_delta(tick.0, &delta.updates, &removal_ids));
    match handle.tx.try_send(frame) {
        Ok(()) => {
            update_baseline(handle.baseline.as_mut().unwrap(), &delta);
            handle.ticks_since_full += 1;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(client_id = _client_id,
                "Client frame channel full, dropping delta -- forcing full snapshot next tick");
            handle.baseline = None;
            handle.ticks_since_full = 0;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}
```

Add the constant near the top of the file or in `delta.rs`:
```rust
/// When this fraction of entities changed, send full snapshot instead of delta.
const DELTA_FALLBACK_THRESHOLD: f32 = 0.80;
```

**Tests:**

1. Add test `delta_fallback_all_entities_changed`:
```
Setup: 10 entities, client has baseline from previous tick.
Action: Move all 10 entities.
Run: prepare_client_snapshot.
Verify: Client channel receives a frame. Decode the first byte: should be 0x01 (full snapshot),
not 0x02 (delta), because 100% > 80%.
Verify: Client baseline is rebuilt (not incrementally updated).
Verify: ticks_since_full is reset to 1.
```

2. Add test `delta_fallback_below_threshold`:
```
Setup: 10 entities, client has baseline.
Action: Move 7 of 10 entities (70% < 80%).
Run: prepare_client_snapshot.
Verify: Client receives 0x02 (delta) frame.
```

3. Add test `delta_fallback_at_boundary`:
```
Setup: 10 entities, client has baseline.
Action: Move 8 of 10 entities (80% = threshold, not exceeded).
Run: prepare_client_snapshot.
Verify: Client receives 0x02 (delta) frame (threshold is strict >).
```

4. Add test `delta_fallback_empty_entities`:
```
Setup: 0 entities, client has empty baseline.
Run: prepare_client_snapshot.
Verify: No panic on division by zero (change_ratio defaults to 0.0).
```

**Done when:**
- Delta path checks change ratio before encoding.
- When >80% entities changed, a full snapshot is sent via the shared Arc.
- Baseline is rebuilt (not incrementally updated) on fallback.
- All new tests pass.
- Existing tests pass (the fallback does not affect the full-snapshot or low-change-rate paths).
- `cargo test -p sim-server` passes.

---

### WI-022: Wire ACK into baseline management (#021)
**Size:** medium (1-2 hours)
**Depends on:** none (but #025 should be done after or alongside)
**Files:**
- `crates/sim-server/src/resources.rs`
- `crates/sim-server/src/systems.rs`

**Description:**
Read the per-client `acked_tick` in `prepare_client_snapshot` and use it as a stale-ACK detector. If a client has not ACK'd recent frames for 2 seconds, force a full snapshot.

**Changes to `resources.rs`:**

1. Add `last_sent_tick` field to `ClientHandle`:
```rust
pub struct ClientHandle {
    pub tx: tokio::sync::mpsc::Sender<ClientFrame>,
    pub player_id: Option<EntityId>,
    pub acked_tick: Arc<AtomicU64>,
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub ticks_since_full: u32,
    pub last_sent_tick: u64,  // NEW
}
```

2. Initialize `last_sent_tick: 0` in `register_from_event`.

3. Add constant:
```rust
/// Ticks without ACK progress before forcing a full snapshot.
/// 40 ticks = 2 seconds at 20Hz. Faster recovery than the 5-second periodic full.
pub const ACK_STALE_TICKS: u64 = 40;
```

4. Remove `#[allow(dead_code)]` from the `acked_tick` field since it is now read.

**Changes to `systems.rs` in `prepare_client_snapshot`:**

1. After the existing `send_full` computation, add the stale-ACK check:
```rust
let acked = handle.acked_tick.load(std::sync::atomic::Ordering::Relaxed);
let stale_ack = handle.last_sent_tick > 0
    && acked < handle.last_sent_tick.saturating_sub(ACK_STALE_TICKS)
    && handle.baseline.is_some();  // only if we have been sending deltas

let send_full = handle.baseline.is_none()
    || handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL
    || stale_ack;
```

The condition `acked < handle.last_sent_tick.saturating_sub(ACK_STALE_TICKS)` checks: "the client has not ACK'd any frame from the last 40 ticks." This is more precise than checking `current_tick - last_sent_tick > ACK_STALE_TICKS` because it detects the actual ACK gap.

Wait -- simpler formulation. What we want is: "we have sent frames, the client's ACK is more than 40 ticks behind what we last sent." That is:

```rust
let stale_ack = handle.last_sent_tick > 0
    && handle.baseline.is_some()
    && (handle.last_sent_tick - acked) > ACK_STALE_TICKS;
```

But `acked` could be 0 if the client has never ACK'd. So we should use `handle.last_sent_tick.saturating_sub(acked) > ACK_STALE_TICKS`. Actually, since both are u64 and acked starts at 0 while last_sent_tick starts at 0, the subtraction is safe as long as `last_sent_tick >= acked`, which is true because the server sends before the client ACKs.

2. Update `last_sent_tick` after successful send (both full and delta paths):
```rust
Ok(()) => {
    handle.last_sent_tick = tick.0;
    // ... existing baseline/ticks_since_full updates
}
```

3. When stale_ack triggers and a full snapshot is successfully sent, log it:
```rust
if stale_ack {
    tracing::info!(client_id = _client_id,
        acked_tick = acked,
        sent_tick = handle.last_sent_tick,
        "ACK stale, forcing full snapshot for client");
}
```

**Tests:**

1. `ack_stale_forces_full_snapshot`:
```
Setup: Register client. Set baseline (not None). Set last_sent_tick = 100.
Set acked_tick AtomicU64 to 50. Current tick = 101.
Compute: last_sent_tick - acked = 50 > ACK_STALE_TICKS (40). Stale.
Run: prepare_client_snapshot.
Verify: Client receives full snapshot (0x01).
Verify: Baseline is rebuilt.
```

2. `ack_fresh_sends_delta`:
```
Setup: Register client. Set baseline. Set last_sent_tick = 100.
Set acked_tick to 95. Current tick = 101.
Compute: 100 - 95 = 5 < 40. Not stale.
Run: prepare_client_snapshot.
Verify: Client receives delta (0x02).
```

3. `ack_stale_new_client_no_false_positive`:
```
Setup: Register client. Baseline is None (new client). last_sent_tick = 0.
acked_tick = 0.
Compute: last_sent_tick = 0, so stale_ack is false (first condition).
Run: prepare_client_snapshot.
Verify: Full snapshot sent (because baseline is None, not because of stale ACK).
```

4. `ack_at_boundary_not_stale`:
```
Setup: last_sent_tick = 40, acked_tick = 0.
Compute: 40 - 0 = 40, which is NOT > 40 (strict). Not stale.
Run: prepare_client_snapshot.
Verify: Delta sent (not forced full).
```

5. `last_sent_tick_updated_on_send`:
```
Setup: Register client, baseline is None.
Run: prepare_client_snapshot at tick 50.
Verify: handle.last_sent_tick == 50.
Run again at tick 51 (baseline now set).
Verify: handle.last_sent_tick == 51.
```

**Done when:**
- `acked_tick` is read in `prepare_client_snapshot`.
- Stale ACK (gap > 40 ticks) triggers a full snapshot.
- `last_sent_tick` is updated on every successful send.
- `#[allow(dead_code)]` is removed from `acked_tick`.
- All new tests pass.
- Existing tests pass.
- `cargo test -p sim-server` passes.

---

### WI-023: ACK tick validation (#025)
**Size:** small (< 1 hour)
**Depends on:** WI-022 (only matters once ACKs are used)
**Files:**
- `crates/sim-server/src/network.rs`
- `crates/sim-server/src/systems.rs`

**Description:**
Add monotonicity checking to the ACK parser in `handle_client`, and clamp the ACK value to the current server tick when reading it in `prepare_client_snapshot`.

**Changes to `network.rs` (handle_client, lines 172-178):**

Replace:
```rust
if data.len() == 9 && data[0] == 0x03 {
    let tick = u64::from_le_bytes(
        data[1..9].try_into().unwrap_or([0; 8]),
    );
    acked_tick.store(tick, Ordering::Relaxed);
}
```

With:
```rust
if data.len() == 9 && data[0] == 0x03 {
    let tick = u64::from_le_bytes(
        data[1..9].try_into().unwrap_or([0; 8]),
    );
    // Monotonicity: only accept if this ACK advances beyond the previous one.
    // Prevents stale/reordered ACKs from regressing the value.
    if tick > 0 {
        let prev = acked_tick.load(Ordering::Relaxed);
        if tick > prev {
            acked_tick.store(tick, Ordering::Relaxed);
        }
    }
}
```

**Changes to `systems.rs` (prepare_client_snapshot):**

When reading the ACK value, clamp to current tick:
```rust
let acked = handle.acked_tick.load(Ordering::Relaxed).min(tick.0);
```

This single `.min(tick.0)` call prevents a malicious client from sending a future tick and bypassing the stale-ACK check. It is placed at the read site (not the write site) because the server tick is easily accessible in the ECS system but not in the async `handle_client` task.

**Tests:**

1. `ack_monotonicity_rejects_lower`:
```
Setup: AtomicU64 starts at 0.
Action: Parse ACK with tick=100. Verify stored = 100.
Action: Parse ACK with tick=50. Verify stored still = 100.
Action: Parse ACK with tick=150. Verify stored = 150.
```

2. `ack_rejects_zero`:
```
Setup: AtomicU64 starts at 0.
Action: Parse ACK with tick=0. Verify stored remains 0.
Action: Parse ACK with tick=1. Verify stored = 1.
Action: Parse ACK with tick=0. Verify stored remains 1.
```

3. `ack_clamp_to_server_tick`:
```
Setup: Client acked_tick = u64::MAX. Server tick = 500.
Action: In prepare_client_snapshot, read acked = handle.acked_tick.load().min(tick.0).
Verify: acked == 500, not u64::MAX.
Verify: Stale ACK check uses the clamped value.
```

4. Update existing ACK tests in `network.rs`:
- `ack_parse_max_tick` should be kept but noted that the value will be clamped at the consumer.
- Add `ack_monotonicity` test as a new unit test.

**Done when:**
- ACK parser rejects tick=0 and non-monotonic ticks.
- `prepare_client_snapshot` clamps acked_tick to current server tick.
- A malicious client cannot bypass stale-ACK detection by sending future ticks.
- All new and existing tests pass.
- `cargo test -p sim-server` passes.

---

### WI-024: Verification pass -- confirm #018, #023, #024 are fixed
**Size:** small (< 30 min)
**Depends on:** none
**Files:**
- none (read-only verification)

**Description:**
Verify that findings #018 (baseline drift), #023 (ghost entities), and #024 (full snapshot clone) are actually fixed in the current codebase. This is a read-and-confirm task, not a code change.

**Checklist:**

1. **#018 -- Baseline drift on dropped frames:**
   - [ ] `prepare_client_snapshot` (systems.rs) updates baseline ONLY in the `Ok(())` arm of `try_send`.
   - [ ] On `TrySendError::Full`, baseline is set to `None` (forces full snapshot next tick).
   - [ ] Same pattern applies to both full-snapshot and delta paths.

2. **#023 -- Ghost entities from baseline drift:**
   - [ ] Server-side: #018 fix prevents baseline drift, so removals are computed against accurate baselines.
   - [ ] Client-side: `interpolate()` in `world-view.ts` has a staleness sweep that removes entities not updated for >6 seconds.
   - [ ] The staleness threshold (6s) is slightly longer than FULL_SNAPSHOT_INTERVAL (5s) to avoid false positives.

3. **#024 -- Full snapshot cloned per client:**
   - [ ] `ClientFrame` enum has `Shared(Arc<Vec<u8>>)` variant.
   - [ ] Full snapshot is wrapped in `Arc::new(...)` once.
   - [ ] Each client receives `Arc::clone(...)`, not `Vec::clone(...)`.
   - [ ] Only the backward-compat `ClientSnapshot.frame` does a `(*full_frame).clone()`.

**Done when:**
- All checklist items are confirmed by reading the current code.
- If any item is NOT confirmed, file a new work item to fix it.
- Update `docs/mesh-auditor/findings-review.md` to mark #018, #023, #024 as resolved with code references.

---

## Execution Order

```
Phase 1: Independent fixes (can be done in parallel)
  WI-020  Vector distance for change detection (#022)    [no deps, small]
  WI-021  Delta fallback heuristic (#027)                [no deps, small]
  WI-024  Verification of resolved findings              [no deps, small]

Phase 2: ACK wiring
  WI-022  Wire ACK into baseline management (#021)       [no deps, medium]

Phase 3: ACK hardening
  WI-023  ACK tick validation (#025)                     [after WI-022, small]
```

WI-020, WI-021, and WI-024 are fully independent and can be done in any order or in parallel.

WI-022 should be done before WI-023 because ACK validation is only meaningful once ACKs are consumed. The two can also be done together as a single implementation session since they touch adjacent code.

## Verification Milestones

**After Phase 1:** Delta compression uses correct vector distance thresholds. High-change-rate scenarios (NPC-heavy worlds) fall back to full snapshots automatically. Three findings are formally confirmed as resolved.

**After Phase 2:** Clients that stop processing frames (tab backgrounded) get a full snapshot within 2 seconds instead of waiting up to 5 seconds. The `acked_tick` field is no longer dead code.

**After Phase 3:** Malicious clients cannot spoof ACK ticks to manipulate baseline management. The ACK protocol is hardened against reordering, regression, and future-tick attacks.

## Key Constants

| Constant | Value | Location | Rationale |
|----------|-------|----------|-----------|
| `CHANGE_THRESHOLD` | 0.01 | `delta.rs` | Unchanged. 0.01 world units is sub-pixel. |
| `CHANGE_THRESHOLD_SQ` | 0.0001 | `delta.rs` | Derived. Used for distance_squared comparison. |
| `DELTA_FALLBACK_THRESHOLD` | 0.80 | `systems.rs` or `delta.rs` | When 80%+ entities changed, full snapshot is cheaper. |
| `ACK_STALE_TICKS` | 40 (2s) | `resources.rs` | Faster recovery than FULL_SNAPSHOT_INTERVAL (5s). |
| `FULL_SNAPSHOT_INTERVAL` | 100 (5s) | `delta.rs` | Unchanged. Periodic safety net. |
