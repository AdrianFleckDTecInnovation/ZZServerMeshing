# NPC-SCATTER: NPC spawn range scales with WORLD_EXTENT, scattering entities beyond any client's AoI

## Severity: medium

## Category: scalability

## Summary
The NPC spawn range uses `WORLD_EXTENT * 0.8`, which now generates NPCs across a [-4000, 4000] range on both X and Z axes. With an AoI radius of 500 units, a client at the origin can only see entities within a 500-unit circle -- an area of pi * 500^2 = 785,398 square units. The total spawn area is (8000)^2 = 64,000,000 square units. A uniformly distributed NPC has only a 1.2% chance of being within any single client's AoI. With 100 NPCs per server, a client would see approximately 1-2 NPCs on average, making the world appear nearly empty despite the server simulating 100 entities.

This is not a bug in the AoI implementation -- the filtering works correctly. The issue is that the NPC distribution strategy was designed for a 500-unit world where everything was in range, and it was not updated for the 5000-unit world where AoI filtering matters.

## Evidence
`crates/sim-server/src/game_loop.rs:147-151`:
```rust
let pos = Vec3::new(
    rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
    0.0,
    rng.gen_range(-WORLD_EXTENT * 0.8..WORLD_EXTENT * 0.8),
);
```

`crates/shared/src/spatial.rs:7`:
```rust
pub const WORLD_EXTENT: f32 = 5000.0;
```

`crates/shared/src/spatial.rs:49`:
```rust
pub const AOI_RADIUS: f32 = 500.0;
```

The spawn range is [-4000, 4000] on each axis. The total spawn area is 8000 * 8000 = 64,000,000 square units.

AoI circle area = pi * 500^2 = 785,398 square units.

Fraction visible to a client at origin: 785,398 / 64,000,000 = 1.23%.

With 100 NPCs per server and 3 servers (300 total NPCs), expected NPCs visible to a client: 300 * 0.0123 = 3.7.

NPC movement at speed 1-3 units/tick means they drift ~50-150 units/second. An NPC at the edge of AoI will exit within 3-10 seconds and wander into the void. NPCs will perpetually drift in and out of the AoI boundary, creating a sparse, flickering world view.

## Impact
The world will feel empty. Players will see 1-5 NPCs at any given time despite 100-300 being simulated. This undermines the visual demonstration of server meshing -- the whole point is to see many entities managed by different servers.

Additionally, the server simulates all 100+ NPCs regardless of whether any client can see them. The AoI filter saves bandwidth but not CPU: the server still runs `npc_behavior`, `physics_integration`, and `clamp_positions` for all authoritative entities.

For the UNO Q boards specifically, the boards are simulating entities that no client will ever see, burning CPU cycles with zero visual payoff.

## Recommended Fix
This is a game design / scenario design issue, not an AoI bug. Two approaches:

**Option A: Cluster NPCs near spawn points.** Instead of uniform random across the world, spawn NPCs in clusters around designated "activity zones" that clients are likely to be near:

```rust
// Spawn NPCs within a 500-unit radius of origin (guaranteed AoI visibility)
let angle = rng.gen_range(0.0..std::f32::consts::TAU);
let dist = rng.gen_range(0.0..400.0);  // slightly less than AOI_RADIUS
let pos = Vec3::new(angle.cos() * dist, 0.0, angle.sin() * dist);
```

**Option B: Use NPC attraction points.** Add a simple behavior where NPCs wander toward the nearest player entity when they are far from any player. This concentrates entities in areas where clients can see them.

**Option C: Scale initial_npcs down and spawn on demand.** Start with fewer NPCs globally and spawn more in regions where clients connect. This aligns NPC density with observer density.

For immediate testing, Option A is the simplest: make the initial spawn range configurable or tied to `AOI_RADIUS` rather than `WORLD_EXTENT`.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:147-151 (NPC spawn range)
- crates/shared/src/spatial.rs:7 (WORLD_EXTENT = 5000)
- crates/shared/src/spatial.rs:49 (AOI_RADIUS = 500)
- crates/sim-server/src/systems.rs:799-823 (NPC wandering behavior)
