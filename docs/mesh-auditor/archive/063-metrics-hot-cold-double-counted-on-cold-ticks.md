# Metrics hot_count Includes Entities Also Counted as Cold on Cold Ticks

## Severity: low

## Category: operational

## Summary

On cold ticks, an entity that is within PEER_AOI_RADIUS of a player is counted as `hot_this_peer` (and included), but a distant entity is counted as `cold_this_peer`. The hot entity is NOT counted as cold. This means on cold ticks, `hot_count + cold_count` across all peers equals the total entities sent, which is correct for total accounting. However, `hot_count` on a cold tick is identical to `hot_count` on a hot-only tick (same entities, same distance check). The metric does not distinguish "entities sent because hot" from "entities that happened to be hot but were sent anyway because cold tick." This is a minor clarity issue, not a correctness bug.

## Evidence

In `systems.rs:1229-1244`:
```rust
let is_hot = players.iter().any(|pp| {
    let dx = e.position.x - pp.x;
    let dz = e.position.z - pp.z;
    (dx * dx + dz * dz) <= PEER_AOI_RADIUS_SQ
});
if is_hot {
    hot_this_peer += 1;
    true
} else if is_cold_tick {
    cold_this_peer += 1;
    true
} else {
    false
}
```

On cold tick 4 with 5 entities (3 hot, 2 cold):
- `hot_count` = 3 (same as ticks 1-3)
- `cold_count` = 2 (only far entities)
- Total sent = 5

This is verified by the test at `systems.rs:7316-7318`:
```rust
assert_eq!(outbound.hot_count, 3, "tick 4: 3 hot entities (same as always)");
assert_eq!(outbound.cold_count, 7, "tick 4: 7 cold entities across both peers");
```

The metric semantics are: `hot_count` = entities within PEER_AOI_RADIUS, `cold_count` = entities outside it that were included because cold tick. This is internally consistent and the tests verify it. The concern is purely about dashboard interpretation: an operator might expect `cold_count` to equal the total entity count on cold ticks, but it only includes the far-away entities.

## Impact

No functional impact. The metrics are consistent with the implementation logic. An operator reading the metrics dashboard would see `hot_count` stay constant across hot and cold ticks, which might be confusing but is not wrong.

## Recommended Fix

Consider adding a `total_sent_count` metric that equals `hot_count + cold_count` (across all peers), making it obvious that cold ticks send more. Or document the metric semantics in the metrics endpoint or dashboard.

This is informational -- no code change needed unless the dashboard is misleading.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1229-1244 (hot/cold counting)
- crates/sim-server/src/game_loop.rs:484-488 (metrics recording)
- crates/sim-server/src/metrics.rs:34-36 (hot/cold fields)
