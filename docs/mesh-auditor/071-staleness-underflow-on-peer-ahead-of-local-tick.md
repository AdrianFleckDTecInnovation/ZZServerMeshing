# Staleness Calculation Incorrect When Peer Is Ahead of Local Tick

## Severity: medium

## Category: protocol

## Summary

The staleness calculation in Phase 6 computes `tick_val.saturating_sub(latest)`, which produces zero when a peer's latest received tick is ahead of the local server's tick. This masks a potentially important diagnostic signal: a peer being ahead indicates either clock drift (tick counters started at different times) or that the local server is falling behind. Reporting zero staleness in this case is misleading -- it suggests the peer is perfectly healthy when in fact the tick relationship is abnormal.

## Evidence

The staleness calculation at `game_loop.rs:549-554`:

```rust
let latest = ph.latest_tick.get(&pid).copied().unwrap_or(0);
let staleness = if latest > 0 {
    tick_val.saturating_sub(latest)
} else {
    u64::MAX
};
```

When `latest > tick_val` (peer is ahead), `saturating_sub` returns 0 instead of representing the negative offset.

This situation occurs naturally because:
1. Tick counters are local to each server (finding #014). Each server starts counting from 0 at its own startup time.
2. If server S2 starts 10 seconds before server S1, when S1 reaches tick 100, S2 is at tick 300. S1's receiver gets packets with tick 300. S1's Phase 6 computes `100.saturating_sub(300) = 0`.
3. The metrics report `staleness_ticks: 0` for a peer that is 200 ticks ahead -- identical to the value reported for a perfectly synchronized peer.

The design doc at `docs/mesh-architect/designs/011-network-saturation-telemetry.md:114` defines staleness as "our current tick minus their latest tick" and the example shows `staleness_ticks: 2` as normal. The doc does not address the ahead-of-local case.

## Impact

For the primary use case (detecting WiFi saturation by observing rising staleness), this is a minor issue. WiFi saturation causes the local server to stop receiving fresh packets, so the peer's latest_tick falls behind the local tick -- staleness rises correctly.

The issue manifests during:
1. **Server startup staggering**: A newly started server sees all peers at staleness 0 for potentially hundreds of ticks, making it impossible to distinguish "peer is healthy" from "peer started before us."
2. **Monitoring dashboards**: An operator reading `staleness_ticks: 0` cannot tell if the connection is good or if the local server's tick counter is just behind.
3. **Automated health checks**: A health check that triggers on `staleness > 20` will never fire for a server that is falling behind its peers.

In the current deployment (all 3 servers started within seconds by `start.sh`), the tick offset is small (< 100 ticks, or 5 seconds). The practical impact is low.

## Recommended Fix

Report staleness as a signed value, or add a separate field indicating whether the peer is ahead:

**Option A: Use i64 staleness (breaking JSON change)**

```rust
let staleness: i64 = if latest > 0 {
    (tick_val as i64) - (latest as i64)
} else {
    i64::MAX
};
```

Negative staleness = peer is ahead. Positive = peer is behind. This is the most informative representation but changes the JSON schema.

**Option B: Add a `peer_tick_offset` field (additive)**

Keep `staleness_ticks` as-is (clamped to 0) for backward compatibility. Add `tick_offset: i64` that shows the signed difference. Dashboards can use the new field for full visibility while existing consumers are unaffected.

**Option C: Use 0 for "in sync or ahead", document the semantic (cheapest)**

Add a code comment and documentation note that `staleness_ticks: 0` means "peer's latest tick >= our tick, connection is live." This is technically what the code does now; the fix is just to document it explicitly so operators do not misinterpret it.

Option C is the pragmatic choice for the current deployment. Option B is better for a production system with independent server lifecycles.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:549-554 (staleness calculation)
- crates/sim-server/src/metrics.rs:52 (staleness_ticks field, u64)
- docs/mesh-architect/designs/011-network-saturation-telemetry.md:114 (design doc definition)
- docs/network-telemetry-api.md:28 (API documentation)
- docs/mesh-auditor/014-tick-counter-u64-not-synchronized.md (related: tick counters not synced)
