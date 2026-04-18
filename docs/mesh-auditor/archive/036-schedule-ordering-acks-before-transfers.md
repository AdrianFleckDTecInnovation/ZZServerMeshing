# Schedule Ordering: ACKs Processed Before New Transfers Initiated

## Severity: low

## Category: protocol

## Summary
The schedule ordering places `process_transfer_acks` before `process_transfers` and `process_drain`. This is correct and intentional -- ACKs from previous ticks are consumed before new transfers are initiated. However, there is a subtle interaction: an entity could be despawned by an ACK in `process_transfer_acks`, and then a new rebalance directive in the same tick could try to reference that entity. The current code handles this correctly because `process_transfers` only operates on entities with `Authoritative` that are still in the world.

## Evidence

The schedule in schedule.rs:12-30:
```rust
systems::drain_inbound_events,
systems::process_transfer_acks.after(systems::drain_inbound_events),
systems::process_transfer_timeouts.after(systems::process_transfer_acks),
...
systems::process_transfers.after(systems::clamp_positions),
systems::process_drain.after(systems::process_transfers),
```

The ordering is:
1. `drain_inbound_events` -- accept transfers, create shadows, process ACK queue swap
2. `process_transfer_acks` -- despawn entities with TransferOutbound whose ACKs arrived
3. `process_transfer_timeouts` -- retry or abort timed-out transfers
4. (simulation systems)
5. `process_transfers` -- initiate new transfers from rebalance directives
6. `process_drain` -- initiate drain transfers

**The one concern:** `process_coord_events` runs in parallel with the transfer ACK/timeout systems (only constrained to be `.after(drain_inbound_events)`). A rebalance directive processed in `process_coord_events` sets `PendingTransfers`, which is read later by `process_transfers`. Since `process_coord_events` and `process_transfer_acks` both run after `drain_inbound_events` without an explicit ordering between them, Bevy could schedule them in either order -- but this is safe because they access different resources (CoordEventQueue vs InboundQueue/TransferOutbound).

## Impact
No bug. The schedule ordering is correct for the current implementation. This finding documents the ordering rationale for future maintainers, and flags the implicit parallelism between `process_coord_events` and `process_transfer_acks` as something to be aware of when adding new resource access to either system.

## Recommended Fix
No code change needed. Consider adding a comment in `schedule.rs` documenting why `process_transfer_acks` runs before `process_transfers` (ACKs from previous ticks must be consumed before initiating new transfers to avoid transferring already-transferred entities).

## Effort: small

## Related Files
- crates/sim-server/src/schedule.rs:12-30 (schedule ordering)
- crates/sim-server/src/systems.rs:255-280 (process_transfer_acks)
- crates/sim-server/src/systems.rs:287-351 (process_transfer_timeouts)
- crates/sim-server/src/systems.rs:734-816 (process_transfers)
