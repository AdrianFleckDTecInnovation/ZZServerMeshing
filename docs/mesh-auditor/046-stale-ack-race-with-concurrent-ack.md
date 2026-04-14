# STALE-ACK-RACE: Stale-ACK check has a benign race with concurrent client ACKs

## Severity: low

## Category: race-condition

## Summary
The stale-ACK detection reads `acked_tick` with `Ordering::Relaxed` from the game loop while the `handle_client` async task writes it concurrently. If a client ACK arrives right as the stale check fires, the game loop may read the pre-ACK value (stale) or the post-ACK value (fresh), depending on timing. This is a benign race: the worst case is one unnecessary full snapshot, which is correct behavior (the client gets a full resync and continues with deltas).

## Evidence
The stale check at `systems.rs:1195-1198`:
```rust
let acked = handle.acked_tick.load(std::sync::atomic::Ordering::Relaxed).min(tick.0);
let stale_ack = handle.last_sent_tick > 0
    && handle.baseline.is_some()
    && handle.last_sent_tick.saturating_sub(acked) > ACK_STALE_TICKS;
```

The ACK writer at `network.rs:183-186`:
```rust
let prev = acked_tick.load(Ordering::Relaxed);
if tick > prev {
    acked_tick.store(tick, Ordering::Relaxed);
}
```

Both use `Ordering::Relaxed`. No happens-before relationship is guaranteed. Scenario:
1. Game loop reads `acked_tick = 50`, `last_sent_tick = 92`, gap = 42 > 40. Stale detected.
2. Simultaneously, `handle_client` stores `acked_tick = 91` (client caught up).
3. Game loop sends full snapshot (unnecessary but harmless).
4. Next tick: game loop reads `acked_tick = 91`, gap = 1. Not stale. Normal deltas resume.

The recovery test at `systems.rs:2252-2289` (`ack_stale_recovery_resumes_delta`) proves the system recovers correctly after a stale trigger.

## Impact
At most one extra full snapshot per stale-ACK event. No data loss, no stuck state. The full snapshot resets the baseline, so even if the stale detection was a false positive, the client state is correct.

The `Ordering::Relaxed` is appropriate here because: (a) `acked_tick` is a monotonically increasing counter, so a stale read is always a valid prior value; (b) the consequence of reading stale is a full snapshot, which is the safe default; (c) `Ordering::Acquire`/`Release` would add unnecessary memory barriers on every tick for every client.

## Recommended Fix
No action needed. The race is benign by design. Document the intentional Relaxed ordering in a code comment if desired.

## Effort: n/a

## Related Files
- crates/sim-server/src/systems.rs:1195-1198 (stale-ACK read)
- crates/sim-server/src/network.rs:183-186 (ACK write)
- crates/sim-server/src/systems.rs:2252-2289 (recovery test)
