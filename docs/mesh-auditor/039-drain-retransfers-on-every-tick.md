# Drain System Re-Sends Transfer Messages Every Tick for Non-TransferOutbound Entities

## Severity: medium

## Category: bottleneck

## Summary
The `process_drain` system runs every tick while `DrainState.draining` is true. On each tick, it queries for authoritative entities WITHOUT `TransferOutbound`, builds transfer messages, serializes them, and sends them. In the steady state (after the first drain tick), all entities already have `TransferOutbound`, so the query returns zero entities and the system does minimal work. However, if any entities are added between ticks (e.g., by crash recovery promoting shadows, or by a rebalance arriving simultaneously), they get transfer-messaged on the next tick -- which is correct behavior. The concern is that the system performs an ECS query and multiple resource accesses every tick even when there is nothing to do, and the drain completion check runs a second ECS query every tick.

More importantly, there is an interaction between the drain and the timeout system: if `process_transfer_timeouts` aborts a transfer (removes `TransferOutbound` after max retries), `process_drain` on the next tick will re-initiate the transfer for that same entity. This creates an infinite loop of transfer-abort-retransfer for entities whose target server is unreachable, consuming network bandwidth and CPU until the drain timeout (30 seconds) forces shutdown.

## Evidence

`process_drain` at systems.rs:825-933 runs every tick when draining. On each tick:
1. Queries `(With<Authoritative>, Without<TransferOutbound>)` (line 846-851) -- finds entities not yet in transfer
2. For each, builds and serializes an `AuthorityTransfer` message, sends it, adds `TransferOutbound`
3. Checks completion: queries `(With<Authoritative>, Without<TransferOutbound>)` again (line 907) and `With<TransferOutbound>` (line 911)

If `process_transfer_timeouts` (systems.rs:287-351) aborts a transfer at tick T:
- It removes `TransferOutbound` from the entity (line 342)
- At tick T+1, `process_drain` finds the entity (no `TransferOutbound`) and re-initiates the transfer
- The new transfer times out after 10 ticks, is retried once, then aborted again
- At tick T+22, the cycle repeats

This means the entity bounces between transfer and abort, generating ~1 transfer message every 22 ticks to an unreachable server, for up to 600 ticks (30 seconds drain timeout).

## Impact
- **Wasted bandwidth**: Each re-initiated transfer sends a ~120 byte UDP packet to an unreachable server. With 100 entities targeting an unreachable server, that is ~100 packets every 22 ticks, or ~90 packets/second. Not catastrophic, but wasteful.
- **Log noise**: Each cycle generates error-level log messages for the abort and info-level messages for the re-initiation, flooding the logs.
- **Delayed shutdown**: The drain keeps trying for 30 seconds before forcing shutdown, even when it is clear the target is unreachable. The server is in a degraded state (draining but not making progress) for the entire timeout period.

## Recommended Fix

1. **Track failed targets.** When `process_transfer_timeouts` aborts a transfer to server X, record X in a `DrainState.failed_targets: HashSet<u16>`. In `process_drain`, skip targets in `failed_targets`.

2. **Reduce drain timeout when targets are unreachable.** If all targets are in `failed_targets`, force immediate shutdown instead of waiting 30 seconds.

3. **Alternative: Don't re-transfer aborted drain entities.** Keep the `TransferOutbound` component even after abort during drain, just mark it as `failed`. The completion check counts failed transfers as "done" for shutdown purposes.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:825-933 (process_drain)
- crates/sim-server/src/systems.rs:332-350 (process_transfer_timeouts abort path)
- crates/sim-server/src/systems.rs:844-851 (drain re-queries entities without TransferOutbound)
