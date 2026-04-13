# Multiple Rebalance Directives Overwrite -- Only Last Survives

## Severity: medium

## Category: data-loss

## Summary
When multiple rebalance directives arrive in the same tick window, `process_coord_events` processes them all but each one overwrites `PendingTransfers.transfers`, so only the last directive takes effect. Earlier directives are silently discarded.

## Evidence
`systems.rs` lines 347-351:
```rust
// RebalanceDirective
for (entity_ids, target_server) in rebalance_directives {
    world.resource_mut::<PendingTransfers>().transfers =
        Some((entity_ids, target_server));  // OVERWRITES previous directive
}
```

If the coordinator sends two directives in rapid succession (e.g., "transfer entities A,B to S2" and "transfer entities C,D to S3"), and both arrive within the same tick window (50ms), only the second directive survives. The first is overwritten without being processed.

The `process_transfers` system then runs (after `process_coord_events` in the schedule) and processes only the surviving directive.

## Impact
- **Normal operation**: The coordinator sends at most one rebalance directive per 5-second cycle, so directives rarely overlap. This is a low-probability issue.
- **Burst scenario**: If the coordinator detects multiple imbalances simultaneously (e.g., after crash recovery, multiple servers need rebalancing), it may send several directives within one tick window.
- **Result**: Some entity transfers are silently dropped, leading to persistent load imbalance that the coordinator has to detect and correct in a subsequent cycle. Not data loss (entities aren't destroyed), but causes delayed convergence.

## Recommended Fix
Change `PendingTransfers` to accumulate directives rather than overwrite:

```rust
#[derive(Resource, Default)]
pub struct PendingTransfers {
    pub directives: Vec<(Vec<String>, u16)>,
}
```

Then `process_transfers` iterates over all pending directives:
```rust
let directives = std::mem::take(&mut world.resource_mut::<PendingTransfers>().directives);
for (entity_ids, target_server) in directives {
    // process each directive
}
```

Alternatively, since the `entity_ids` field in the directive is currently unused (the system ignores it and selects its own entities to transfer), the directives could be merged: collect all target servers and distribute entities across them.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:347-351 (overwrite in process_coord_events)
- crates/sim-server/src/systems.rs:464-537 (process_transfers -- consumes single directive)
- crates/sim-server/src/resources.rs:59-63 (PendingTransfers -- single Option)
