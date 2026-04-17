# Transfer Timeout Uses Strict Greater-Than Causing Off-By-One

## Severity: low

## Category: protocol

## Summary
The transfer timeout check uses `saturating_sub` with a strict greater-than comparison, which means the timeout fires at tick `sent_tick + TRANSFER_TIMEOUT_TICKS + 1` rather than at tick `sent_tick + TRANSFER_TIMEOUT_TICKS`. This is a minor off-by-one that makes timeouts take one tick (50ms) longer than intended. Not harmful, but the constant name suggests 500ms and the actual timeout is 550ms.

## Evidence

In systems.rs:296:
```rust
if current_tick.saturating_sub(outbound.sent_tick) > TRANSFER_TIMEOUT_TICKS {
```

With `TRANSFER_TIMEOUT_TICKS = 10` and `sent_tick = 100`:
- At tick 110: `110 - 100 = 10`, `10 > 10` is false. Not timed out.
- At tick 111: `111 - 100 = 11`, `11 > 10` is true. Timed out.

The timeout fires at tick 111, which is 11 ticks (550ms) after the send, not 10 ticks (500ms).

The same pattern exists in the drain timeout check at systems.rs:923:
```rust
if tick.saturating_sub(started) > DRAIN_TIMEOUT_TICKS {
```

With `DRAIN_TIMEOUT_TICKS = 600`, the actual drain timeout is 601 ticks (30.05s).

## Impact
Minimal. The extra 50ms on the transfer timeout is negligible. The extra 50ms on the drain timeout is irrelevant. This is a correctness nit, not a production issue.

## Recommended Fix
Either change to `>=` or adjust the constant to `TRANSFER_TIMEOUT_TICKS = 9` to get the intended 500ms. The `>=` approach is clearer:

```rust
if current_tick.saturating_sub(outbound.sent_tick) >= TRANSFER_TIMEOUT_TICKS {
```

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:296 (transfer timeout check)
- crates/sim-server/src/systems.rs:923 (drain timeout check)
- crates/sim-server/src/systems.rs:24-27 (TRANSFER_TIMEOUT_TICKS, DRAIN_TIMEOUT_TICKS constants)
