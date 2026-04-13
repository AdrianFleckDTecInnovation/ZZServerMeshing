# ACK-PARSING: Binary ACK parsing accepts any tick value without validation

## Severity: low

## Category: protocol

## Summary
The server-side ACK parser accepts any u64 value from the client without validating it against the server's actual tick counter. A malicious or buggy client can send ACK ticks from the future (tick 999999999) or from the distant past. While the ACK value is currently unused (see #021), if it were wired into baseline management, an invalid ACK could cause the server to skip full snapshots or advance the baseline incorrectly.

## Evidence
In `crates/sim-server/src/network.rs:171-176`:
```rust
Some(Ok(Message::Binary(data))) => {
    if data.len() == 9 && data[0] == 0x03 {
        let tick = u64::from_le_bytes(
            data[1..9].try_into().unwrap_or([0; 8]),
        );
        acked_tick.store(tick, Ordering::Relaxed);
    }
    // Ignore other binary messages from client
}
```

No validation is performed:
- No check that `tick <= current_server_tick`
- No check that `tick >= previously_acked_tick` (monotonicity)
- No check that `tick > 0`
- No rate limiting on ACK messages

A client can send 0x03 frames at any rate with any tick value. The `Ordering::Relaxed` atomic store means there is no happens-before relationship with the game loop's reads.

Additionally, the `unwrap_or([0; 8])` fallback for `try_into` silently converts malformed data to tick 0. This is defensive but masks potential issues.

## Impact
Currently none, since `acked_tick` is never read (finding #021). But this is a latent vulnerability:

1. If ACK-based baseline management is implemented (as suggested in #021), a client sending `acked_tick = u64::MAX` could trick the server into thinking it has confirmed the most recent state, preventing necessary full snapshots.

2. There is no rate limiting on binary messages from clients, so a client can flood the server with ACK frames. Each one triggers an atomic store, which is cheap but still unnecessary work.

3. The `Ordering::Relaxed` means the game loop could theoretically read a stale or reordered ACK value, though in practice this is harmless for a monotonically increasing counter.

## Recommended Fix
Add basic validation when the ACK is eventually consumed:

```rust
// In the ACK parser:
if data.len() == 9 && data[0] == 0x03 {
    let tick = u64::from_le_bytes(data[1..9].try_into().unwrap_or([0; 8]));
    // Only accept if tick is reasonable (not from the future)
    let prev = acked_tick.load(Ordering::Relaxed);
    if tick > prev && tick > 0 {
        acked_tick.store(tick, Ordering::Relaxed);
    }
}
```

When wiring ACKs into baseline management, the game loop should also validate: `acked_tick <= current_tick`.

## Effort: small

## Related Files
- crates/sim-server/src/network.rs:171-176 (ACK parsing)
- crates/sim-server/src/resources.rs:166-167 (acked_tick field)
- client/src/network.ts:164-170 (client ACK sending)
