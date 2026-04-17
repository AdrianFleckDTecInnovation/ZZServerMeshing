# Backpressure Skips Cold Ticks With No Recovery Path

## Severity: high

## Category: data-loss

## Summary

When UDP backpressure is active, the game loop skips ALL per-peer replication -- including cold ticks. With full snapshots disabled by default, there is no mechanism to send entity state to peers while under backpressure. Shadows will go stale and be cleaned up after SHADOW_STALE_TICKS (300 ticks = 15 seconds), causing entity loss across the mesh.

## Evidence

In `game_loop.rs:394-433`, the replication dispatch logic has two branches:

```
if let Some(entities) = &repl_full {
    // Full replication tick: ALWAYS broadcast (bypass backpressure)
    ...
} else if !repl_per_peer.is_empty() && !repl_backpressure {
    // Filtered replication tick: skip when under backpressure
    ...
}
```

The `repl_full` path bypasses backpressure, but `repl_full` is only populated when `full_snapshot_enabled` is true (default: false, set via `--full-snapshot` CLI flag). When full snapshots are disabled, `repl_full` is always `None`, so the only replication path is the `else if` branch, which is gated by `!repl_backpressure`.

The comment at line 417-418 explicitly says:
```
// ... we stop sending per-tick filtered updates and rely
// on the 1/sec full snapshot for shadow recovery.
```

This comment is stale. The "1/sec full snapshot" no longer exists by default after design 010. The cold tier at 5Hz was supposed to replace it, but cold ticks go through the same `per_peer` map and are blocked by the same backpressure gate.

Backpressure is enabled at `game_loop.rs:531` when `udp_repl_drops > 0` at the 5-second logging boundary, and disabled at line 535 when drops return to zero. During the entire backpressure window (minimum 5 seconds, potentially much longer), zero entity state reaches peers.

## Impact

1. **Shadow staleness cascade**: Under backpressure, no entity updates reach peers. After 15 seconds (SHADOW_STALE_TICKS = 300), all shadows from this server are cleaned up on every peer. When backpressure lifts, the server has entities but no peers have shadows of them.

2. **Crash recovery failure**: If this server crashes while under backpressure, peers have no recent shadow data to recover from. All entities on this server are lost.

3. **Cold tier purpose defeated**: The design 010 rationale for disabling full snapshots was "the cold tier at 5Hz provides sufficient crash recovery coverage." But the cold tier shares the same backpressure gate, so this guarantee does not hold under load.

4. **Self-reinforcing loop**: Backpressure clears shadows on peers. When backpressure lifts, peers have no shadows. `prepare_replication` at `systems.rs:1206` returns early when `peer_all_positions` is empty ("If no shadows from any peer, skip replication on this tick"). The system never recovers because replication requires shadows, but shadows require replication.

## Recommended Fix

Cold ticks (tick % 4 == 0) should bypass the backpressure gate, similar to how full snapshot ticks do. The game loop dispatch should be:

```rust
let is_cold_tick = repl_tick % 4 == 0;

if let Some(entities) = &repl_full {
    // Full replication tick: ALWAYS broadcast
    ...
} else if !repl_per_peer.is_empty() && (!repl_backpressure || is_cold_tick) {
    // Cold ticks bypass backpressure to maintain shadow freshness
    ...
}
```

This sends entity state at 5Hz even under backpressure, which is the minimum rate needed to keep shadows alive. The 75% bandwidth reduction (5Hz vs 20Hz) provides meaningful relief while preventing the stale shadow cascade.

Also update the stale comment at line 417-418 to reflect the current design.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:394-433 (backpressure dispatch)
- crates/sim-server/src/game_loop.rs:517-536 (backpressure enable/disable)
- crates/sim-server/src/systems.rs:1204-1209 (empty shadow early return)
- crates/sim-server/src/systems.rs:1157 (cold tick modulo)
- crates/sim-server/src/main.rs:72 (full_snapshot default false)
