# Design Doc Claims SHADOW_STALE_TICKS = 100 But Code Uses 300

## Severity: medium

## Category: operational

## Summary

Design 010 states "SHADOW_STALE_TICKS (100 = 5 seconds) provides ample margin" for cold-tier crash recovery. The actual code value is 300 (15 seconds). While the higher value is safer (more margin before cleanup), the design doc's safety analysis is based on incorrect assumptions. More importantly, the 5Hz cold tier refresh guarantee is used as the primary justification for disabling full snapshots, and the safety margin calculation in the design doc (5 seconds / 200ms = 25 cold-tick chances) is wrong by 3x.

## Evidence

Design doc at `docs/mesh-architect/designs/010-tiered-aoi-shadow-replication.md:398`:
```
| SHADOW_STALE_TICKS | 100 (5s) | sim-server systems | Shadow cleanup (unchanged) |
```

And at line 266:
```
The cold tier at 5Hz ensures every entity reaches every peer at least every 200ms.
This is far better than the previous 1-per-second full snapshot for crash recovery purposes.
SHADOW_STALE_TICKS (100 = 5 seconds) provides ample margin.
```

And at line 323:
```
SHADOW_STALE_TICKS = 100 (5s) gives 25 cold-tick chances before staleness cleanup.
Losing all 25 consecutive cold packets for a specific entity is effectively impossible.
```

Actual code at `systems.rs:24`:
```rust
const SHADOW_STALE_TICKS: u64 = TICK_RATE as u64 * 15; // 15 seconds
```

This is 300 ticks, not 100. The actual margin is 75 cold-tick chances (300 / 4), not 25.

## Impact

The mismatch is benign in this direction (code is safer than the doc claims). However:

1. The design doc is used as a reference for future development decisions. If someone reads it and assumes SHADOW_STALE_TICKS is 100, they might set it lower, not realizing the cold tier has a failure mode (#058) that makes the 15-second window important.

2. The `delta.rs:21` file also has a stale comment: "100 ticks = 5 seconds at 20Hz. Matches SHADOW_STALE_TICKS." -- but SHADOW_STALE_TICKS is 300, so this comment is wrong too.

## Recommended Fix

Update the design doc to reference the correct value (300 = 15 seconds). Update the delta.rs comment. Consider adding a compile-time assertion to keep the constant and documentation in sync:

```rust
const _: () = assert!(SHADOW_STALE_TICKS == 300, "Update design docs if changed");
```

## Effort: small

## Related Files
- docs/mesh-architect/designs/010-tiered-aoi-shadow-replication.md:266,323,398
- crates/sim-server/src/systems.rs:24 (actual constant)
- crates/sim-server/src/delta.rs:21 (stale comment)
