# DELTA-FALLBACK-BASELINE: Fallback full snapshot correctly rebuilds baseline

## Severity: low

## Category: protocol

## Summary
The delta fallback heuristic (>80% entities changed triggers full snapshot) correctly rebuilds the baseline from the full entity set on success. The baseline, `ticks_since_full`, and `last_sent_tick` are all updated identically to the normal full-snapshot path. Verified that subsequent ticks correctly produce deltas against the new baseline. No baseline drift or stuck-in-full-snapshot loop.

## Evidence
The fallback path at `systems.rs:1245-1260`:
```rust
if change_ratio > DELTA_FALLBACK_THRESHOLD {
    let frame = ClientFrame::Shared(Arc::clone(&full_frame));
    match handle.tx.try_send(frame) {
        Ok(()) => {
            handle.baseline = Some(build_baseline(&entities));
            handle.ticks_since_full = 1;
            handle.last_sent_tick = tick.0;
        }
        Err(TrySendError::Full(_)) => {
            handle.baseline = None;  // Forces full next tick
        }
        Err(TrySendError::Closed(_)) => {}
    }
}
```

This is identical to the explicit full-snapshot path at `systems.rs:1214-1227`:
```rust
let frame = ClientFrame::Shared(Arc::clone(&full_frame));
match handle.tx.try_send(frame) {
    Ok(()) => {
        handle.baseline = Some(build_baseline(&entities));
        handle.ticks_since_full = 1;
        handle.last_sent_tick = tick.0;
    }
    // ... same error handling
}
```

Both paths:
1. Use the same `Arc::clone(&full_frame)` -- no extra allocation
2. Call `build_baseline(&entities)` with the same entity set
3. Set `ticks_since_full = 1` (not 0, correctly counting this tick)
4. Update `last_sent_tick` for stale-ACK tracking

The test at `systems.rs:2401-2435` (`delta_fallback_resets_baseline_and_ticks_since_full`) verifies:
- Baseline is `Some` after fallback
- `ticks_since_full` is 1
- Next unchanged tick produces delta (0x02), not another full

The boundary test at `systems.rs:2438-2476` verifies the 80% threshold is strict `>`: 4/5 = 80% produces delta, 5/5 = 100% produces full.

Division-by-zero guard at `systems.rs:1239-1243`:
```rust
let change_ratio = if entities.is_empty() {
    0.0
} else {
    delta.updates.len() as f32 / entities.len() as f32
};
```

Test at `systems.rs:2352-2372` (`delta_fallback_empty_entities_no_panic`) confirms no panic with 0 entities.

## Impact
None. The fallback implementation is correct. This finding documents the verification.

One minor observation: the fallback calls `build_baseline(&entities)` which allocates a new `HashMap`, identical to the normal full path. This means on a tick where all NPCs move (common case), the fallback fires every tick for every client, rebuilding baselines every tick. This is the same cost as the non-delta path and is the correct behavior (the old full-snapshot-every-tick approach). Finding #020 (O(N*E) delta computation) still applies -- the delta is computed first, then thrown away when the fallback triggers. This is wasted CPU but is bounded by a single `compute_delta` call per client.

## Recommended Fix
No action needed for correctness. For the minor CPU waste (compute delta then discard), a future optimization could check a coarse "any entity unchanged?" flag before computing the full delta. But this is premature until profiling shows it matters.

## Effort: n/a

## Related Files
- crates/sim-server/src/systems.rs:1232-1260 (delta path with fallback)
- crates/sim-server/src/systems.rs:1204-1231 (normal full snapshot path)
- crates/sim-server/src/systems.rs:1239-1243 (division by zero guard)
- crates/sim-server/src/systems.rs:2401-2435 (baseline rebuild test)
- crates/sim-server/src/systems.rs:2438-2476 (boundary precision test)
