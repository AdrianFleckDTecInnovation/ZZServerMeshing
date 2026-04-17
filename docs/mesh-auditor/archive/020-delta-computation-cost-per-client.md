# DELTA-PER-CLIENT: Delta computation runs O(E) per client, total O(N*E) per tick

## Severity: medium

## Category: bottleneck

## Summary
The `prepare_client_snapshot` system computes a separate delta for each connected client by iterating over all entities and comparing against that client's baseline. With N clients and E entities, total work is O(N * E) per tick. Since all clients currently see the same entity set with the same state, these N delta computations produce identical results. The duplicated work is wasted CPU time in the 50ms tick budget.

## Evidence
In `crates/sim-server/src/systems.rs:829-851`:
```rust
for (_client_id, handle) in registry.clients.iter_mut() {
    // ...
    let baseline = handle.baseline.as_mut().unwrap();
    let delta = compute_delta(&entities, baseline);
    // ...
}
```

`compute_delta` in `delta.rs:51-79` iterates over all current entities (building a HashSet) and all baseline keys:
- Line 58-70: O(E) iteration over current entities with HashMap lookup per entity
- Line 73-77: O(B) iteration over baseline keys with HashSet lookup per key
- Plus HashSet construction: O(E) with allocation

For 10 clients and 500 entities, that is 10 * (500 + 500) = 10,000 entity comparisons per tick, plus 10 HashSet allocations of 500 entries.

Additionally, `compute_delta` allocates a new `HashSet` on every call (`HashSet::with_capacity(current.len())` at line 56). This is a per-client, per-tick heap allocation in the hot path.

## Impact
At current scale (1-3 clients, 20-60 entities), the cost is negligible. At target scale:

| Clients | Entities | Comparisons/tick | HashSet allocs/tick |
|---------|----------|-----------------|-------------------|
| 3       | 60       | 360             | 3                 |
| 10      | 500      | 10,000          | 10                |
| 20      | 1000     | 40,000          | 20                |
| 50      | 2000     | 200,000         | 50                |

At 50 clients with 2000 entities, this could consume a meaningful fraction of the 50ms budget on UNO Q hardware. The allocations compound the issue by putting pressure on the allocator.

## Recommended Fix
Two approaches, in order of effort:

**Quick fix -- Cache the delta result when all clients share the same entity view:**
Before interest management is implemented, all clients see the same entities. Compute the delta once and share it:

```rust
// Pre-compute delta for clients that have baselines matching the "common" baseline
let full_frame = encode_binary_snapshot(tick.0, &entities);
snapshot.frame = Some(full_frame.clone());

// Compute delta only once per tick using a reference baseline
// (defer per-client work to just selecting full vs delta)
```

This requires recognizing that without interest management, baselines only differ on their first tick (full snapshot catch-up). After the first full snapshot, all baselines converge. A "common delta" computed against any single baseline would be valid for all clients with the same baseline age.

**Better fix -- Shared baseline with tick stamps:**
Replace per-client baselines with a single server-wide "last sent state" buffer. Track per-client only the tick of their last acknowledged full snapshot. If their tick matches the current baseline epoch, send the shared delta. If not, send a full snapshot.

**Immediate micro-optimization -- Reuse the HashSet:**
Pass a reusable `HashSet` into `compute_delta` instead of allocating one per call:

```rust
pub fn compute_delta(
    current: &[EntityState],
    baseline: &HashMap<EntityId, CompactEntityState>,
    scratch_set: &mut HashSet<EntityId>,
) -> DeltaResult {
    scratch_set.clear();
    // ...
}
```

## Effort: medium

## Related Files
- crates/sim-server/src/systems.rs:829-851 (per-client delta loop)
- crates/sim-server/src/delta.rs:51-79 (compute_delta with HashSet alloc)
- crates/sim-server/src/delta.rs:56 (HashSet::with_capacity allocation)
