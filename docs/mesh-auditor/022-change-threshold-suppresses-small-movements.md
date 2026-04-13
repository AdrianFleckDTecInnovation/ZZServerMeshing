# CHANGE-THRESHOLD: 0.01 threshold suppresses real entity movements and accumulates drift

## Severity: medium

## Category: protocol

## Summary
The delta comparison uses a fixed threshold of 0.01 world units to filter floating-point noise. But entity movement at 20Hz with low velocity produces per-tick position changes that are legitimately below this threshold. The suppressed changes accumulate over multiple ticks, causing the baseline to drift from reality. The client only sees corrections when the cumulative drift exceeds 0.01 or a full snapshot arrives.

## Evidence
In `crates/sim-server/src/delta.rs:13`:
```rust
const CHANGE_THRESHOLD: f32 = 0.01;
```

Used in `delta.rs:112-116`:
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    (a.x - b.x).abs() > CHANGE_THRESHOLD
        || (a.y - b.y).abs() > CHANGE_THRESHOLD
        || (a.z - b.z).abs() > CHANGE_THRESHOLD
}
```

Consider an NPC moving at velocity 0.1 units/second. At 20Hz, each tick moves 0.005 units. This is below the 0.01 threshold. The entity will never appear in a delta frame because each individual tick change is sub-threshold. The client only sees this entity's movement on the periodic full snapshot every 5 seconds, where it jumps 0.5 units.

**Calculation for player movement:** Player speed is 5.0 units/second (from `systems.rs` MoveInput handler). At 20Hz, that is 0.25 units/tick -- well above 0.01. Player movement is fine.

**Calculation for NPC movement:** NPC speed is randomized between 1.0-3.0 units/second (from `npc_behavior` system). At 20Hz, minimum is 0.05 units/tick -- above 0.01. NPC movement is fine for axis-aligned motion.

**However:** Diagonal movement splits velocity across axes. An NPC at 1.0 units/sec along both X and Z moves 0.05/sqrt(2) = 0.035 per axis per tick -- still above threshold. But an NPC at very low speed (approaching 0.2 units/sec, which could happen with rounding or future speed changes) would produce 0.01/axis movements.

**The real issue is accumulation.** Because the baseline is updated even when no delta is sent (via full snapshot at 100-tick intervals), the threshold is essentially harmless for position tracking. But it creates a correctness gap: between full snapshots, velocity changes below the threshold are invisible. If an entity's velocity oscillates by small amounts (e.g., jitter from physics or AI), the client never sees these changes via delta frames.

Additionally, the threshold is compared per-component (`abs() > THRESHOLD`), not per-vector (`distance > THRESHOLD`). An entity moving diagonally at 0.008 on each axis has a true distance change of 0.014 -- above what should reasonably be filtered -- but each component is below 0.01, so it is suppressed.

## Impact
At current NPC speeds (1.0-3.0 units/sec), this is unlikely to cause visible issues. But it is a correctness concern:

1. If NPC speeds are reduced or velocity smoothing is added, the threshold could suppress real movements.
2. The per-component comparison means diagonal micro-movements are filtered more aggressively than axis-aligned ones.
3. Velocity oscillation (e.g., AI direction changes) below 0.01/tick is invisible between full snapshots.

## Recommended Fix
Two changes:

1. **Use vector distance instead of per-component threshold:**
```rust
fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) > CHANGE_THRESHOLD * CHANGE_THRESHOLD
}
```
This treats diagonal motion correctly. `distance_squared` avoids the sqrt cost.

2. **Consider tracking "ticks since last sent" per entity in the baseline.** If an entity has not been sent for N ticks but has accumulated any change (even sub-threshold), force-include it in the next delta. This prevents indefinite suppression:

```rust
pub struct CompactEntityState {
    // ... existing fields ...
    pub ticks_since_sent: u16,
}
```

On each delta where the entity is not included, increment `ticks_since_sent`. If it reaches a limit (e.g., 20 = 1 second), include the entity regardless of threshold.

## Effort: small

## Related Files
- crates/sim-server/src/delta.rs:13 (CHANGE_THRESHOLD constant)
- crates/sim-server/src/delta.rs:104-116 (entity_changed, vec3_changed)
