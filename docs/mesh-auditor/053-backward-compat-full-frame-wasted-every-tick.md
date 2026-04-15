# BACKWARD-COMPAT-FULL-FRAME: Unfiltered full snapshot encoded every tick for unused ClientSnapshot resource

## Severity: medium

## Category: bottleneck

## Summary
The `prepare_client_snapshot` system encodes an unfiltered full snapshot of ALL entities into a `Vec<u8>` and writes it to the `ClientSnapshot` resource every tick for "backward compatibility" with schedule tests. No production code reads `ClientSnapshot.frame` after the schedule runs. With the 5000-unit world, this encodes all E entities (27 + E * 47 bytes) every tick for zero benefit in production, consuming both CPU and memory in the 50ms tick budget.

## Evidence
`crates/sim-server/src/systems.rs:1181-1183`:
```rust
let full_frame = Arc::new(encode_binary_snapshot(tick.0, &entities));
snapshot.frame = Some((*full_frame).clone());
```

The comment at line 1105-1106 confirms this is backward-compat only:
```rust
/// Also writes a full snapshot to ClientSnapshot for backward compatibility
/// (used by schedule tests that check ClientSnapshot.frame).
```

Production code in `game_loop.rs` never reads `ClientSnapshot.frame` after `schedule.run()`. The only consumers are test assertions:
- `crates/sim-server/src/systems.rs:1661` (test)
- `crates/sim-server/src/systems.rs:3942` (test)
- `crates/sim-server/src/systems.rs:4133` (test)
- `crates/sim-server/src/schedule.rs:98,288,545` (tests)
- `crates/sim-server/src/game_loop.rs:1038` (test)

The `encode_binary_snapshot` call at line 1182 allocates a `Vec<u8>` of size `27 + E * 47` bytes, encodes all E entities into it, wraps it in an `Arc`, then immediately dereferences and clones the inner Vec into `snapshot.frame`. This is two allocations (Arc + clone) plus O(E) encoding work, all for a resource that is never read in production.

## Impact
Cost per tick:
- E=500: 27 + 500 * 47 = 23,527 bytes allocated + encoded + cloned = ~47 KB heap churn
- E=2000: 27 + 2000 * 47 = 94,027 bytes = ~188 KB heap churn
- E=5000: 27 + 5000 * 47 = 235,027 bytes = ~470 KB heap churn

At 20 Hz:
- E=500: ~940 KB/sec of wasted allocation + encoding
- E=2000: ~3.7 MB/sec
- E=5000: ~9.4 MB/sec

On UNO Q boards with constrained memory, this is measurable. The encoding itself involves iterating all entities and calling `extend_from_slice` for each field -- not free at scale.

Additionally, the `Arc::new()` wrapping is vestigial. The `full_frame` Arc is never shared across clients because AoI filtering produces per-client Owned frames. The `ClientFrame::Shared` variant is marked `#[allow(dead_code)]` in `resources.rs:19`, confirming it is unused in the current code path.

## Recommended Fix
Remove the backward-compat encoding. Update the tests that read `ClientSnapshot.frame` to instead read from the per-client channel (which they already do in the AoI tests).

In `crates/sim-server/src/systems.rs`:
1. Remove lines 1181-1183 (the `full_frame` and `snapshot.frame` write).
2. Remove `mut snapshot: ResMut<ClientSnapshot>` from the system signature.
3. Update the affected tests to register a test client and read from the mpsc channel instead of reading `ClientSnapshot.frame`.

The `ClientSnapshot` resource and the `ClientFrame::Shared` variant can also be removed, but that is a larger cleanup.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1105-1106 (backward-compat comment)
- crates/sim-server/src/systems.rs:1181-1183 (full_frame encode + clone)
- crates/sim-server/src/resources.rs:19 (`#[allow(dead_code)]` on Shared variant)
- crates/sim-server/src/resources.rs:111-113 (ClientSnapshot resource)
- crates/sim-server/src/schedule.rs:98,288,545 (tests reading ClientSnapshot.frame)
- crates/sim-server/src/game_loop.rs:1038 (test reading ClientSnapshot.frame)
