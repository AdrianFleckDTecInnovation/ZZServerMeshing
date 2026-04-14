# Global Reassembly Sweep Stale Threshold May Evict Legitimately Slow Peers

## Severity: low

## Category: scalability

## Summary
The global reassembly sweep in receiver.rs (lines 39-55) evicts entries older than `GLOBAL_STALE_TICKS = 40` ticks behind the maximum tick from ANY server. This threshold is based on the assumption that all servers maintain roughly synchronized tick counters. However, tick counters are independent per server (finding #014) -- a server that starts later or restarts will have a tick counter significantly behind its peers. Entries from this legitimately slow server will be evicted by the global sweep because they are more than 40 ticks behind the fastest server's tick.

## Evidence

The sweep logic in receiver.rs:39-55:
```rust
if loop_counter % GLOBAL_SWEEP_INTERVAL == 0 {
    if let Some(&global_max) = latest_tick.values().max() {
        reassembly.retain(|&(_sid, t), _| {
            global_max.saturating_sub(t) <= GLOBAL_STALE_TICKS
        });
    }
}
```

The `global_max` is the highest tick from any server. If Server 1 is at tick 1000 and Server 2 just restarted at tick 50, the sweep evicts all of Server 2's reassembly entries because `1000 - 50 = 950 > 40`.

The per-server cleanup (line 150) already handles the per-server stale case with a 2-tick window. The global sweep was added to handle the case where a server crashes and its ticks never advance (finding #029). But it also catches servers that are legitimately behind.

**Startup scenario**: When a new sim-server joins the mesh:
1. It starts at tick 0.
2. It begins sending chunked updates to peers.
3. Peers have `global_max` at whatever their oldest server is (e.g., tick 5000).
4. The peer's global sweep evicts ALL of the new server's reassembly entries because `5000 - 0 = 5000 > 40`.
5. The new server's replication is silently dropped until its tick catches up to within 40 of the global max.
6. At 20 ticks/second, it takes `(5000 - 40) / 20 = ~248 seconds` to catch up.

## Impact
- **Mild**: The per-server stale cleanup (2-tick window, line 150) handles the normal case correctly. The global sweep only fires every 100 loop iterations, and the scenario requires a large tick divergence. Non-chunked updates (`EntityStateUpdate`) bypass reassembly entirely and are unaffected.
- **Startup delay**: A server joining a long-running mesh will have its chunked updates dropped for the first few minutes. Unchunked updates (under 100 entities) work fine.
- **Restart recovery**: After a server crash and restart, its tick resets to 0. Chunked replication from this server is dropped by peers until the tick catches up. This delays shadow creation on peers.

## Recommended Fix

Change the global sweep to use per-server latest ticks instead of a global max:

```rust
if loop_counter % GLOBAL_SWEEP_INTERVAL == 0 {
    reassembly.retain(|&(sid, t), _| {
        if let Some(&server_latest) = latest_tick.get(&sid) {
            server_latest.saturating_sub(t) <= GLOBAL_STALE_TICKS
        } else {
            // No latest tick for this server = it has never sent a non-chunked update.
            // Keep entries for a grace period based on any available reference.
            true
        }
    });
}
```

This preserves the crash cleanup behavior (crashed server's ticks never advance, so entries are evicted based on that server's own last tick) while avoiding false evictions of legitimately slow peers.

## Effort: small

## Related Files
- crates/sim-server/src/receiver.rs:27-55 (global sweep constants and logic)
- crates/sim-server/src/receiver.rs:146-151 (per-server cleanup, 2-tick window)
- crates/shared/src/tick.rs (TICK_RATE=20, independent per server)
