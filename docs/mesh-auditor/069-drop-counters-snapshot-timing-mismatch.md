# Drop Counters Report Inconsistent Time Windows

## Severity: medium

## Category: operational

## Summary

The `udp_repl_drops` and `udp_ctrl_drops` counters reported in the `/metrics` JSON endpoint represent different time windows depending on when in the 5-second reporting cycle the metrics endpoint is queried. The counters accumulate continuously but are reset to zero only on the 5-second boundary (tick % 100 == 0). The metrics snapshot captures the current counter value each tick, so a query at tick 99 sees the full 5 seconds of drops, while a query at tick 1 sees only drops from the current tick. This makes the drop counters unreliable for automated saturation detection.

## Evidence

The drop counters are declared as running accumulators at `game_loop.rs:224-225`:

```rust
let mut udp_repl_drops: u64 = 0;
let mut udp_ctrl_drops: u64 = 0;
```

They are incremented by `try_enqueue` throughout Phase 4 (game_loop.rs:374) and written to the MetricsCollector in Phase 6 at `game_loop.rs:539-541`:

```rust
m.udp_repl_drops = udp_repl_drops;
m.udp_ctrl_drops = udp_ctrl_drops;
```

They are reset to zero only inside the 5-second debug logging block at `game_loop.rs:606` and `game_loop.rs:617`:

```rust
if tick_val % (shared::tick::TICK_RATE as u64 * 5) == 0 {
    // ...
    if udp_repl_drops > 0 {
        // ... log ...
        udp_repl_drops = 0;  // line 606
    }
    // ...
    if udp_ctrl_drops > 0 {
        // ... log ...
        udp_ctrl_drops = 0;  // line 617
    }
}
```

This means:
- At tick 100 (5s boundary): counters are written to metrics, THEN reset. The snapshot contains the full 5s window.
- At tick 101: counters are written to metrics. The snapshot contains only 1 tick (~50ms) of drops.
- At tick 199: counters are written to metrics. The snapshot contains ~99 ticks (~5s minus one tick) of drops.

An external monitoring tool polling `/metrics` at 1-second intervals will see wildly varying drop counts that depend on where in the 5-second cycle the poll happens, not on actual network conditions.

The `udp_bytes_out` counter has a different issue: it is reset to zero at the START of each tick (game_loop.rs:434):

```rust
udp_bytes_out = 0;
```

This means `udp_bytes_out` always represents a single tick's bytes. This is correct and documented. But the inconsistency between `udp_bytes_out` (per-tick) and `udp_repl_drops` (per-5s-window, sawtooth) is confusing.

## Impact

Automated monitoring that triggers alerts on `udp_repl_drops > 0` will produce false negatives: most polls will hit a tick shortly after the 5-second reset, seeing zero drops even during sustained saturation. The documentation in `network-telemetry-api.md` describes the fields as "in the current 5-second window" but this is only approximately true. A poll at tick 105 would see only 5 ticks of drops, not a full window.

The `peer_health.packets_received` and `peer_health.chunk_evictions` fields have the same sawtooth issue: they accumulate in the PeerHealthAccumulator, are read every tick in Phase 6, but reset only at the 5-second boundary.

## Recommended Fix

Two options, pick one:

**Option A (simplest): Reset drop counters per-tick, document as per-tick values.**

Reset `udp_repl_drops` and `udp_ctrl_drops` at the start of each tick alongside `udp_bytes_out`. The metrics endpoint then always reports single-tick values. The 5-second logging can use separate shadow counters for its rate-limited log messages.

```rust
// At top of tick loop, after udp_bytes_out = 0:
let prev_repl_drops = udp_repl_drops;
let prev_ctrl_drops = udp_ctrl_drops;
udp_repl_drops = 0;
udp_ctrl_drops = 0;
// Use prev_repl_drops/prev_ctrl_drops for 5s logging accumulation
```

**Option B (more informative): Report both per-tick and windowed values.**

Add `udp_repl_drops_window` and `udp_ctrl_drops_window` fields that report the full 5-second count. Keep the per-tick value in the existing fields for consistency with `udp_bytes_out`.

Option A is simpler and matches the existing `udp_bytes_out` pattern. The network-telemetry-api.md documentation should be updated to reflect whichever approach is chosen.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:224-225 (counter declarations)
- crates/sim-server/src/game_loop.rs:434 (udp_bytes_out per-tick reset)
- crates/sim-server/src/game_loop.rs:539-541 (Phase 6 write to metrics)
- crates/sim-server/src/game_loop.rs:592-617 (5-second reset with conditional)
- crates/sim-server/src/game_loop.rs:619-625 (peer health 5s reset)
- docs/network-telemetry-api.md:17-19 (field documentation)
