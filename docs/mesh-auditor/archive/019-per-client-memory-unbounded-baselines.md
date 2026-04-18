# PER-CLIENT-MEMORY: Per-client baseline HashMaps grow unbounded with entity count

## Severity: medium

## Category: scalability

## Summary
Each connected client holds a full `HashMap<EntityId, CompactEntityState>` baseline in the `ClientRegistry`. With N clients and E entities, total memory for baselines is O(N * E). On resource-constrained hardware (UNO Q boards with 512MB RAM), this creates a meaningful memory pressure point. The baseline HashMaps also never shrink their allocation -- entities come and go, but the HashMap capacity only grows.

## Evidence
In `crates/sim-server/src/resources.rs:169-170`:
```rust
pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
```

`CompactEntityState` (in `delta.rs:20-28`) contains:
- `position: Vec3` (12 bytes)
- `velocity: Vec3` (12 bytes)
- `entity_type: EntityType` (1 byte + padding)
- `authority_server: ServerId` (2 bytes + padding)
- `last_input_seq: u32` (4 bytes)

Total per-entry: ~40 bytes for the struct + 24 bytes for the HashMap entry (EntityId key = 16 bytes + pointer + metadata) = ~64 bytes per entity per client.

At 500 entities and 10 clients: 500 * 64 * 10 = 320KB.
At 1000 entities and 20 clients: 1000 * 64 * 20 = 1.28MB.
At 5000 entities and 50 clients: 5000 * 64 * 50 = 16MB.

The `build_baseline` function in `delta.rs:83-88` creates a new HashMap every time a full snapshot is sent, causing an allocation spike. The old HashMap is replaced and deallocated, but the new one may be a different size, causing fragmentation.

Additionally, `update_baseline` in `delta.rs:91-102` inserts entries but HashMap never shrinks its bucket array when entries are removed. Over time, if entities are created and destroyed, the HashMap capacity drifts upward.

## Impact
On UNO Q boards (512MB total RAM), 16MB for baselines alone at high entity/client counts is significant. The allocation pattern (periodic full rebuild every 100 ticks) means every 5 seconds per client, a new HashMap is allocated and the old one freed, contributing to allocator churn.

This is not a crisis at current scale (20-60 entities, 1-3 clients per server), but it will become a problem when scaling to the target entity counts described in the mesh-improvements roadmap.

## Recommended Fix
Two improvements:

1. **Reuse baseline HashMap on full snapshot instead of replacing it.** Instead of `handle.baseline = Some(build_baseline(&entities))`, clear and repopulate the existing map:
```rust
if let Some(ref mut baseline) = handle.baseline {
    baseline.clear();
    for e in &entities {
        baseline.insert(e.id, CompactEntityState::from(e));
    }
} else {
    handle.baseline = Some(build_baseline(&entities));
}
```
This avoids the allocation/deallocation churn and preserves the HashMap's capacity.

2. **Periodically shrink.** After clearing, call `baseline.shrink_to(entities.len())` to release excess capacity if the entity count has dropped significantly.

3. **Consider shared baseline.** If all clients on the same server see the same entity set (which is currently true -- no interest management), a single shared baseline with per-client "last seen tick" could reduce memory by N-fold. This is a larger refactor and only worthwhile after interest management is added (when baselines diverge per client).

## Effort: small

## Related Files
- crates/sim-server/src/resources.rs:169 (baseline field)
- crates/sim-server/src/delta.rs:83-88 (build_baseline)
- crates/sim-server/src/delta.rs:91-102 (update_baseline)
- crates/sim-server/src/systems.rs:835 (full snapshot baseline rebuild)
