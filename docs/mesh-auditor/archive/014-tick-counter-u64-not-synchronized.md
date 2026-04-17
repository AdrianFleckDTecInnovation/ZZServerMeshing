# Tick Counter u64 Overflow Harmless But Cross-Server Ticks Diverge

## Severity: low

## Category: protocol

## Summary
Each server maintains its own `TickCounter(u64)` that increments independently. Servers do not synchronize their tick counters. When server A sends an `EntityStateUpdate` with tick=1000 and server B receives it, server B compares it against its shadow's `last_seen_tick` -- but B's tick counter might be at 950 or 1050 depending on when it started and whether it has ever skipped ticks.

The tick staleness check in `drain_inbound_events` (line 50: `if update.tick < shadow.last_seen_tick`) compares the *sender's* tick against the *locally stored sender tick*. This is correct as long as you only compare ticks from the same source server. However, the `shadow_cleanup` system (line 710: `tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS`) compares the *local* tick against the *sender's* tick. If server B started 5 seconds after server A, then B's tick is ~100 behind A's. A shadow from A with `last_seen_tick=200` (A's tick) compared against B's local tick=100 gives `100 - 200 = 0` (saturating), which is "fresh". But 100 ticks later, B is at tick=200 and the shadow has `last_seen_tick=200` (never updated because A's tick didn't advance relative to B's storage). This actually works because A's continuous replication keeps updating `last_seen_tick` to A's latest tick, which advances independently.

The real issue: if A skips ticks (due to overload, MissedTickBehavior::Skip), A's tick counter jumps forward. A's shadows on B now have `last_seen_tick` that jumps forward too. B's shadow cleanup compares B's local tick (which didn't skip) against A's jumped tick, and the shadow appears "fresh" even if A hasn't actually sent data for several seconds.

## Evidence
`game_loop.rs` line 81:
```rust
interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
```

When ticks are skipped, `TickCounter` increments by 1 per actual tick, not per wall-clock tick. So skipped ticks don't inflate the counter. This means A's tick counter actually advances *slower* under load, not faster.

However, `TickCounter` is incremented at line 124:
```rust
world.resource_mut::<TickCounter>().0 += 1;
```

This increments by 1 regardless of skipped ticks. Over time, servers that skip more ticks have lower tick counters than servers that don't. The staleness check at shadow_cleanup line 710:
```rust
if tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS
```

uses the local tick, which may be lower than the sender's tick. If the local server is behind, `local_tick - sender_tick` saturates to 0, and the shadow is never cleaned up. If the local server is ahead, shadows from a slow server appear stale prematurely.

## Impact
- **Normal operation**: Servers start within seconds of each other. Tick drift is bounded by 20-40 ticks (1-2 seconds). The 100-tick stale threshold accommodates this with margin.
- **Overload scenario**: A server under heavy load skips 50% of ticks. Its tick counter advances at 10Hz instead of 20Hz. After 30 minutes, it is 36,000 ticks behind peers. Shadows from this server on other servers appear stale and are cleaned up, even though the server is still actively sending data. This causes entity flickering -- shadows deleted and re-created each tick.
- **u64 overflow**: At 20Hz, u64 overflows after ~29 billion years. Not a concern.

## Recommended Fix
Store per-source tick counters for shadow staleness, rather than comparing against the local tick. Change `Shadow` to track wall-clock time instead of (or in addition to) ticks:

```rust
pub struct Shadow {
    pub last_seen_tick: u64,        // sender's tick (for ordering)
    pub last_seen_at: Instant,      // wall clock (for staleness)
}
```

Shadow cleanup uses `last_seen_at.elapsed() > Duration::from_secs(5)` instead of tick arithmetic.

This requires passing an `Instant` through the `PeerUpdate` and into `drain_inbound_events`, which is straightforward since the timestamp is taken when the UDP packet arrives.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:50 (tick comparison for shadow ordering)
- crates/sim-server/src/systems.rs:710 (tick comparison for shadow cleanup)
- crates/sim-server/src/game_loop.rs:81 (MissedTickBehavior::Skip)
- crates/sim-server/src/game_loop.rs:124 (tick increment)
- crates/sim-server/src/components.rs:30-33 (Shadow component)
