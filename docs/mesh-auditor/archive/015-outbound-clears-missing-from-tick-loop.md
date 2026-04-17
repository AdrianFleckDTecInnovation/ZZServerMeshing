# Outbound Resources Not Cleared Between Ticks

## Severity: medium

## Category: race-condition

## Summary
The game loop uses `.take()` and `std::mem::take()` to extract outbound data after the schedule runs. However, it does not clear the outbound resources before the next schedule run. If the schedule were to run twice (e.g., due to a code bug), or if outbound data from a previous tick leaked through a partial extraction, stale data would be sent again.

More concretely, the schedule test at `schedule.rs` lines 108-117 demonstrates the expected pattern:
```rust
// Clear outputs from previous tick
world.resource_mut::<OutboundReplication>().snapshot = None;
world.resource_mut::<ClientSnapshot>().frame = None;
world.resource_mut::<OutboundTransfers>().messages.clear();
world.resource_mut::<OutboundForwards>().messages.clear();
```

But the actual game loop at `game_loop.rs` does NOT perform these clears. It relies solely on `.take()` at lines 129-133 to both extract AND clear. The issue is that `.take()` happens AFTER the schedule runs. So the outbound resources from tick N persist into the schedule run of tick N+1 if the schedule writes to them conditionally (only when there's data to write).

Looking at the actual systems:
- `prepare_replication` (line 641-646): Sets `outbound.snapshot` unconditionally every tick. Safe.
- `prepare_client_snapshot` (line 696): Sets `snapshot.frame` unconditionally every tick. Safe.
- `process_transfers` (line 519-524): Pushes to `OutboundTransfers.messages`. This Vec is NOT cleared between ticks by the schedule -- it accumulates across ticks if the game loop fails to take it.
- `process_drain` (line 593-598): Same as transfers.
- `process_client_inputs` (line 271-274): Pushes to `OutboundForwards.messages`. Same accumulation risk.

In the current game loop, the `.take()` at lines 132-133 extracts and clears the Vec by replacing it with an empty Vec. So under normal operation, the Vecs start empty each tick. But if `take()` fails (e.g., code is refactored to `.clone()` instead of `.take()`), or if a future system appends to these Vecs after the take, the data persists.

## Evidence
`game_loop.rs` lines 129-133:
```rust
let repl_data = world.resource_mut::<OutboundReplication>().snapshot.take();
let repl_tick = world.resource::<OutboundReplication>().tick;
let client_frame = world.resource_mut::<ClientSnapshot>().frame.take();
let transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
let forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);
```

No explicit clear before `schedule.run(&mut world)` at line 125.

Compare with the test at `schedule.rs` line 111-114 which explicitly clears before each tick. The test represents the intended pattern; the game loop deviates from it.

## Impact
Under current code, this is not a bug -- the take-after-schedule pattern works correctly because systems overwrite (for Options) or append to empty Vecs (for Vecs that were taken on the prior tick). The risk is purely about fragility: the correctness depends on the game loop always taking all outbound data after every schedule run, and no system running out of order.

## Recommended Fix
Add explicit clears before the schedule run, matching the test pattern:

```rust
// Before Phase 2:
world.resource_mut::<OutboundReplication>().snapshot = None;
world.resource_mut::<ClientSnapshot>().frame = None;
world.resource_mut::<OutboundTransfers>().messages.clear();
world.resource_mut::<OutboundForwards>().messages.clear();

// Phase 2: Run schedule
schedule.run(&mut world);
```

This is defensive programming -- it ensures outbound resources are clean regardless of what happened on the previous tick, and it matches the tested behavior.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:122-133 (schedule run + outbound extraction)
- crates/sim-server/src/schedule.rs:108-117 (test clears before each tick)
- crates/sim-server/src/systems.rs:619-647 (prepare_replication writes to OutboundReplication)
- crates/sim-server/src/systems.rs:519-524 (process_transfers appends to OutboundTransfers)
