# Shadow Player Positions Are One Tick Stale for Hot Classification

## Severity: low

## Category: race-condition

## Summary

The `prepare_replication` system reads Player shadow positions from the ECS to classify entities as hot or cold. These shadow positions were last updated by `drain_inbound_events` at the start of this tick, using data from the PREVIOUS tick's UDP replication. The hot/cold classification is therefore based on positions that are 1-2 ticks (50-100ms) old. This is a known design tradeoff documented in the design doc, and the 250-unit margin (750 vs 500) was explicitly chosen to absorb it.

## Evidence

Schedule ordering in `schedule.rs:12-29`:
```
drain_inbound_events → ... → prepare_replication → ...
```

`drain_inbound_events` processes peer updates from the inbound queue, which contains data from UDP packets received between ticks. A Player moving at 5 units/tick could be 5-10 units from where the shadow says they are.

Design doc at `010-tiered-aoi-shadow-replication.md:313-314`:
```
Player shadow positions can be up to 1 tick (50ms) stale. At player speed of 5 units/tick,
this is a 5-unit error. The 250-unit margin (750 vs 500) absorbs this entirely.
```

The margin is 750 (PEER_AOI_RADIUS) - 500 (AOI_RADIUS) = 250 units. At 5 units/tick, the player needs 50 ticks (2.5 seconds) to traverse this margin. Even with 2-tick staleness (10 units), the margin is more than sufficient.

## Impact

No practical impact at current player speeds. The 250-unit margin provides 50x safety factor over the worst-case staleness error.

The only theoretical concern is with much faster player movement (e.g., teleportation). If a player teleports more than 250 units in one tick, they could land in an area where hot entities from their old position are no longer relevant, and entities near their new position are not yet classified as hot. The cold tier (200ms) resolves this within 4 ticks.

## Recommended Fix

No fix needed. Document the margin calculation as a code comment near the `is_cold_tick` logic so future developers understand why 750 was chosen:

```rust
// Hot tier uses PEER_AOI_RADIUS (750) instead of client AOI_RADIUS (500).
// The 250-unit margin absorbs 1-2 tick staleness in shadow positions
// (shadow updated from previous tick's replication, player moves 5 units/tick).
```

## Effort: small (comment only)

## Related Files
- crates/sim-server/src/systems.rs:1229-1233 (distance check using shadow positions)
- crates/sim-server/src/systems.rs:1167-1176 (shadow position collection)
- crates/shared/src/spatial.rs:54-55 (PEER_AOI_RADIUS rationale)
- crates/sim-server/src/schedule.rs:13,26 (system ordering)
