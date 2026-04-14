# VEC3-THRESHOLD: distance_squared is slightly more permissive than per-component on single-axis movement

## Severity: low

## Category: protocol

## Summary
The fix for #022 changed `vec3_changed` from per-component `abs() > 0.01` to `distance_squared() > 0.0001`. For axis-aligned movement this is equivalent: moving 0.01 on one axis gives `distance_squared = 0.0001`, which is NOT greater than the threshold (strict `>`). But for the old per-component code, moving 0.01 on X gave `abs(0.01) > 0.01 = false` -- also not triggered. So the boundary behavior is identical for single-axis movement.

However, there is one behavioral difference: a single-axis movement of 0.0099 units was always suppressed by both methods, but a diagonal movement of (0.008, 0, 0.008) was suppressed by the old per-component check (each component < 0.01) and is now correctly detected by `distance_squared` (0.000128 > 0.0001). This is the intended fix. The new threshold is strictly more permissive for diagonal movement and identical for axis-aligned movement.

## Evidence
Old code (`delta.rs`, before fix):
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    (a.x - b.x).abs() > CHANGE_THRESHOLD
        || (a.y - b.y).abs() > CHANGE_THRESHOLD
        || (a.z - b.z).abs() > CHANGE_THRESHOLD
}
```

New code (`delta.rs:117-119`):
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) > CHANGE_THRESHOLD_SQ
}
```

Boundary analysis:
- Single-axis 0.01: old `0.01 > 0.01 = false`, new `0.0001 > 0.0001 = false` -- same
- Single-axis 0.011: old `0.011 > 0.01 = true`, new `0.000121 > 0.0001 = true` -- same
- Single-axis 0.009: old `false`, new `0.000081 > 0.0001 = false` -- same
- Diagonal (0.008, 0, 0.008): old `false && false = false`, new `0.000128 > 0.0001 = true` -- FIX WORKS

No regressions. The test at `delta.rs:574-610` covers both diagonal-above and diagonal-below cases.

## Impact
None. The fix is correct. This finding documents the behavioral equivalence analysis for the record.

## Recommended Fix
No action needed. The implementation correctly resolves #022.

## Effort: n/a

## Related Files
- crates/sim-server/src/delta.rs:13-18 (CHANGE_THRESHOLD, CHANGE_THRESHOLD_SQ)
- crates/sim-server/src/delta.rs:117-119 (vec3_changed)
- crates/sim-server/src/delta.rs:574-610 (diagonal movement tests)
