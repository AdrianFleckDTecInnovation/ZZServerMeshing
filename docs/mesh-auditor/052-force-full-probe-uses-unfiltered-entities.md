# FORCE-FULL-PROBE-MISMATCH: Pre-loop force_full probe compares unfiltered entities against AoI-filtered baselines

## Severity: high

## Category: bottleneck

## Summary
The `force_full` probe at the top of the per-client loop compares the full unfiltered entity list against a single client's baseline. Because baselines are now built from AoI-filtered entity sets (only entities within 500 units of the client's player), entities outside any client's AoI appear as "new" to the baseline. In a large world with E total entities and E_aoi entities within AoI, the probe computes a change ratio of at least (E - E_aoi) / E. When this exceeds 80%, delta compression is skipped for ALL clients on EVERY tick, defeating the purpose of delta compression entirely.

## Evidence
`crates/sim-server/src/systems.rs:1215-1220`:
```rust
let force_full = registry
    .clients
    .values()
    .find_map(|h| h.baseline.as_ref())
    .map(|baseline| change_ratio(&entities, baseline) > DELTA_FALLBACK_THRESHOLD)
    .unwrap_or(false);
```

`entities` is the full unfiltered entity list built at line 1145-1178 (all authoritative + shadow + transfer-outbound entities).

`baseline` is a `HashMap<EntityId, CompactEntityState>` built from the AoI-filtered `client_entities` at line 1276: `handle.baseline = Some(build_baseline(&client_entities))`.

`change_ratio` at `crates/sim-server/src/delta.rs:64-83` counts any entity in `current` that is not in `baseline` as a "change":
```rust
for entity in current {
    match baseline.get(&entity.id) {
        None => changed += 1,  // <-- entities outside AoI hit this
        Some(base) => { ... }
    }
}
changed as f32 / current.len() as f32
```

With E=2000 total entities and E_aoi=200 within a client's AoI, at least 1800 entities are not in the baseline. `change_ratio` returns at minimum 1800/2000 = 0.9, which exceeds `DELTA_FALLBACK_THRESHOLD` (0.80). Therefore `force_full = true` on every tick.

When `force_full` is true, the system encodes a per-client full snapshot every tick at line 1272, bypassing `compute_delta` entirely. This means AoI filtering saves zero bandwidth because every client gets a full snapshot instead of a delta.

## Impact
In any world where a client's AoI covers less than 20% of total entities, delta compression is permanently disabled. This directly contradicts the AoI design goal of reducing bandwidth and CPU.

Concrete thresholds:
- E=500, E_aoi=500 (old 500-unit world): all in AoI, no problem -- change ratio is 0% baseline misses
- E=500, E_aoi=100: 80% misses, force_full fires on every tick
- E=2000, E_aoi=200: 90% misses, force_full always fires
- E=5000, E_aoi=100: 98% misses, force_full always fires

Bandwidth impact: at E_aoi=200, a delta frame would be ~31 + (changed * 47) + (removed * 16) bytes. A full snapshot is 27 + 200 * 47 = 9,427 bytes. If only 10 of 200 entities changed, the delta would be ~31 + 10 * 47 = 501 bytes -- an 18x savings that is completely lost.

CPU impact: `encode_binary_snapshot` runs for all 200 AoI entities every tick per client, instead of `compute_delta` running over only the changed subset. This is a regression from the pre-AoI behavior, which correctly used deltas.

## Recommended Fix
Pass the AoI-filtered `client_entities` to `change_ratio` instead of the unfiltered `entities` vec. Since each client has a different AoI, the probe must be moved inside the per-client loop.

In `crates/sim-server/src/systems.rs`, remove the pre-loop probe (lines 1210-1220) and move the check inside the per-client loop after the AoI filter:

```rust
for (_client_id, handle) in registry.clients.iter_mut() {
    let client_entities: Vec<EntityState> = /* ... AoI filter ... */;

    // Per-client force_full probe (replaces the pre-loop probe)
    let force_full = handle.baseline.as_ref()
        .map(|baseline| change_ratio(&client_entities, baseline) > DELTA_FALLBACK_THRESHOLD)
        .unwrap_or(false);

    let send_full = handle.baseline.is_none()
        || handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL
        || stale_ack
        || force_full;
    // ...
}
```

This restores the original intent of the probe (skip delta when >80% of the CLIENT'S entity set changed) while working correctly with per-client AoI filtering.

The pre-loop optimization (compute once, apply to all clients) was valid when all clients saw the same entity set. With AoI, clients have different entity sets and different baselines, so the probe must be per-client. The cost of `change_ratio` is O(E_aoi) per client, which is much cheaper than O(E) because the AoI filter already reduced the entity count.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1215-1220 (force_full probe)
- crates/sim-server/src/systems.rs:1232-1246 (AoI filter)
- crates/sim-server/src/systems.rs:1260 (force_full used in send_full decision)
- crates/sim-server/src/delta.rs:64-83 (change_ratio function)
