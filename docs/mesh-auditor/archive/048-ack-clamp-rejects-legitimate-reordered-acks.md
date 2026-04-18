# ACK-CLAMP: Server tick clamp cannot reject legitimate ACKs, but monotonicity check silently drops reordered ones

## Severity: low

## Category: protocol

## Summary
The ACK validation has two layers: monotonicity in `handle_client` (network.rs) and server-tick clamp in `prepare_client_snapshot` (systems.rs). Neither can reject legitimate ACKs from a well-behaved client. However, the monotonicity check silently drops reordered ACKs -- if the client sends ACK(5) then ACK(3) arrives out of order, ACK(3) is dropped. This is correct because WebSocket over TCP guarantees in-order delivery, so out-of-order ACKs on a single connection are impossible in normal operation.

Tick wraparound is not a concern: `TickCounter` is `u64`, and at 20Hz it would take 29 billion years to overflow.

## Evidence
Monotonicity check in `network.rs:182-186`:
```rust
if tick > 0 {
    let prev = acked_tick.load(Ordering::Relaxed);
    if tick > prev {
        acked_tick.store(tick, Ordering::Relaxed);
    }
}
```

Server tick clamp in `systems.rs:1195`:
```rust
let acked = handle.acked_tick.load(Ordering::Relaxed).min(tick.0);
```

Analysis of legitimate ACK rejection:
1. Client sends ACK with tick=N where N was the tick from the last received frame.
2. Since the server sent this tick, N <= current server tick. The `.min(tick.0)` clamp has no effect.
3. Since frames arrive in order (TCP), N >= all previously ACK'd ticks. The `tick > prev` check passes.
4. Therefore: no legitimate ACK from a well-behaved client is ever rejected.

The only ACKs rejected are:
- `tick = 0` (invalid, the server never sends tick 0 since TickCounter starts at 0 and increments before first frame send)
- `tick > current_server_tick` (malicious or buggy client -- clamped to current tick, which is still a valid ACK)
- `tick <= previously_acked_tick` (duplicate or reordered -- impossible over TCP, valid rejection)

The `u64::MAX` test at `systems.rs:2206-2247` confirms the clamp handles the extreme case correctly.

The `ack_stale_ticks_constant_value` test at `systems.rs:2201-2203` confirms the constant is 40 (2 seconds at 20Hz).

## Impact
None. Both validation layers are correct and cannot cause false rejections of legitimate ACKs. The only theoretical concern is if WebSocket were replaced with an unreliable transport (WebRTC data channels), where reordering becomes possible. In that case, the monotonicity check would drop valid ACKs, but the stale-ACK detection would compensate by triggering a full snapshot after 2 seconds.

## Recommended Fix
No action needed. The implementation is correct.

## Effort: n/a

## Related Files
- crates/sim-server/src/network.rs:172-186 (ACK parsing + monotonicity)
- crates/sim-server/src/systems.rs:1195 (server tick clamp)
- crates/sim-server/src/systems.rs:2206-2247 (u64::MAX clamp test)
- crates/sim-server/src/resources.rs:162 (ACK_STALE_TICKS = 40)
