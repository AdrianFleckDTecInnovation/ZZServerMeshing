# Cold Tier Distance Check Is O(E_auth * P_players) Per Peer Per Tick

## Severity: medium

## Category: scalability

## Summary

The tiered replication classifies every authoritative entity as hot or cold by checking distance to every Player shadow on each peer. On cold ticks this is harmless (all entities pass regardless), but on hot-only ticks the system still runs O(E_auth * P_players_per_peer * num_peers) distance checks. With 5000 entities, 3 peers, and 5 player shadows per peer, that is 75,000 distance calculations per tick. While each check is cheap (4 float ops), this cost grows quadratically if either entity count or player count scales.

## Evidence

In `systems.rs:1227-1244`, the hot/cold filter on the tiered path:

```rust
entities
    .iter()
    .filter(|e| {
        let is_hot = players.iter().any(|pp| {
            let dx = e.position.x - pp.x;
            let dz = e.position.z - pp.z;
            (dx * dx + dz * dz) <= PEER_AOI_RADIUS_SQ
        });
        if is_hot {
            hot_this_peer += 1;
            true
        } else if is_cold_tick {
            cold_this_peer += 1;
            true
        } else {
            false
        }
    })
    .cloned()
    .collect()
```

The inner `players.iter().any()` is short-circuiting (stops on first match), which helps when entities are nearby. But in the worst case (entity is far from all players), every player position is checked.

### Cost at scale

| Entities | Players/peer | Peers | Distance checks/tick |
|----------|-------------|-------|---------------------|
| 500 | 2 | 2 | 2,000 |
| 1,000 | 5 | 2 | 10,000 |
| 5,000 | 5 | 3 | 75,000 |
| 5,000 | 20 | 5 | 500,000 |
| 10,000 | 20 | 5 | 1,000,000 |

At 4 float ops per check, 75,000 checks is roughly 300,000 float ops. On the UNO Q boards (Cortex-A53 at ~1.2 GHz, no SIMD for f32), this takes ~0.5ms. At 500,000 checks, it approaches 2-3ms -- 5% of the tick budget.

## Impact

At current scale (500 entities, 2-3 peers, 1-2 players per peer), the cost is negligible (~0.1ms). The concern is future scaling:

- With 5000+ entities and 5+ players per peer, the distance checks alone could consume 1-5ms of the 50ms tick budget.
- On cold ticks, the distance checks are wasted because all entities pass. The filter runs `is_hot` computation for every entity even though `is_cold_tick` means all entities will be included.

## Recommended Fix

Two optimizations, in priority order:

1. **Skip distance checks on cold ticks**: On cold ticks, all entities pass. Skip the per-entity distance loop entirely and clone the full entity Vec:

```rust
let relevant: Vec<EntityState> = if is_cold_tick {
    // Cold tick: all entities go to every peer
    cold_this_peer = entities.len();
    // Still classify hot for metrics accuracy
    for e in &entities {
        if players.iter().any(|pp| /* distance check */) {
            hot_this_peer += 1;
            cold_this_peer -= 1;
        }
    }
    entities.clone()
} else {
    // Hot-only tick: filter by distance
    entities.iter().filter(/* existing logic */).cloned().collect()
};
```

Or, if accurate hot/cold metrics on cold ticks are not critical, simplify to:

```rust
if is_cold_tick {
    cold_this_peer = entities.len();
    entities.clone()
}
```

This eliminates all distance checks on 25% of ticks (cold ticks).

2. **Spatial grid pre-filtering**: For large entity counts, use the existing spatial grid (`shared/spatial.rs`) to pre-filter entities by cell before doing precise distance checks. Only entities in cells within PEER_AOI_RADIUS of any player cell need the distance check.

## Effort: small (optimization 1), medium (optimization 2)

## Related Files
- crates/sim-server/src/systems.rs:1222-1244 (tiered filter loop)
- crates/shared/src/spatial.rs:1-6 (CELL_SIZE, spatial grid)
- crates/shared/src/spatial.rs:109-113 (PEER_AOI_RADIUS_SQ)
