# DELTA-BEFORE-FALLBACK: compute_delta runs before fallback check, wasting CPU when all entities move

## Severity: medium

## Category: bottleneck

## Summary
The delta fallback heuristic runs AFTER `compute_delta` completes. In the common case where all NPCs are moving (every tick), `compute_delta` does O(E) HashMap lookups and allocates a Vec of E updates, then the fallback check sees change_ratio > 80% and discards the entire result in favor of a full snapshot. The delta computation is pure waste in this scenario. With 500 entities and 10 clients, this is 5000 wasted HashMap lookups per tick (every 50ms).

## Evidence
In `systems.rs:1232-1260`, the delta path:
```rust
} else {
    let baseline = handle.baseline.as_ref().unwrap();
    let delta = compute_delta(&entities, baseline);  // <-- O(E) work

    let change_ratio = if entities.is_empty() {
        0.0
    } else {
        delta.updates.len() as f32 / entities.len() as f32
    };

    if change_ratio > DELTA_FALLBACK_THRESHOLD {
        // Too many changes -- send full snapshot instead of delta
        // The delta result is DISCARDED here
```

`compute_delta` in `delta.rs:56-85`:
- Iterates all current entities: O(E)
- For each, does a HashMap lookup in baseline: O(1) amortized
- Compares all fields via `entity_changed`: 2x `distance_squared` + 3 comparisons
- Allocates `updates` Vec and `current_ids` HashSet

For 500 entities with 10 clients:
- 10 * `compute_delta(500)` = 10 * (500 HashMap lookups + 500 field comparisons + Vec allocation)
- All 10 produce `change_ratio = 1.0 > 0.8`, so all results are discarded
- Then 10 * `build_baseline(500)` for the fallback full snapshot

The total per-tick cost is: 10 wasted `compute_delta` + 10 `build_baseline` = 20 * O(500) HashMap operations. Without the wasted delta, it would be 10 * O(500).

## Impact
At 500 entities and 10 clients, this doubles the HashMap work in the client snapshot system. At 20Hz, this is 10,000 extra HashMap lookups per second that produce no useful output.

Not critical because HashMap lookups are fast and the 50ms budget is large relative to this cost. But it is measurably wasteful and scales linearly with both client count and entity count.

## Recommended Fix
Track the previous tick's change ratio (or a simpler "all entities moved" flag) and skip `compute_delta` for clients where the last tick had >80% changes. Alternatively, compute the delta once for the first client, check the ratio, and skip for remaining clients if above threshold:

```rust
// Before the per-client loop:
let mut fallback_full = false;
// Compute delta once to check ratio (all clients share the same entity set)
if let Some(first_baseline) = registry.clients.values().find_map(|h| h.baseline.as_ref()) {
    let probe = compute_delta(&entities, first_baseline);
    let ratio = if entities.is_empty() { 0.0 } else { probe.updates.len() as f32 / entities.len() as f32 };
    if ratio > DELTA_FALLBACK_THRESHOLD {
        fallback_full = true;
    }
}
```

This reduces N `compute_delta` calls to 1 when the fallback fires. The probe result can also be reused for the first client.

Note: this optimization only applies when all clients have the same baseline age. If clients have different baselines (e.g., one just reconnected), the per-client delta is still necessary. A simpler approach: if all entities moved last tick, skip delta computation for all clients this tick.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1232-1260 (delta path with fallback)
- crates/sim-server/src/delta.rs:56-85 (compute_delta)
- crates/sim-server/src/systems.rs:40 (DELTA_FALLBACK_THRESHOLD = 0.80)
