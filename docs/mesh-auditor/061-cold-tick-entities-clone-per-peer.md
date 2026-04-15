# Cold Tick Clones Entity Vec Per Peer

## Severity: medium

## Category: bottleneck

## Summary

On cold ticks (every 4th tick), the tiered filter includes all entities for each peer. Each peer's entity list is built by `.cloned().collect()` on the full `entities` Vec, producing a separate heap allocation per peer. With 2000 entities and 3 peers, that is 3 full clones of a ~100KB Vec every 200ms. The hot tick path has the same clone pattern but for fewer entities.

## Evidence

In `systems.rs:1227-1246`, the filter-and-clone:

```rust
entities
    .iter()
    .filter(|e| {
        // ... is_hot || is_cold_tick
    })
    .cloned()
    .collect()
```

On a cold tick, `is_cold_tick` is true, so every entity passes the filter. The result is a full clone of the `entities` Vec. This happens inside the loop at `systems.rs:1216`:

```rust
for (&peer_id, _all_positions) in &peer_all_positions {
```

So with 3 peers, 3 full clones are produced. Each `EntityState` is approximately 48 bytes (UUID + Vec3 + Vec3 + enum + ServerId + u32). At 2000 entities, each clone is ~96KB. Three clones = ~288KB per cold tick.

The `entities` Vec itself is already a clone (built at line 1131-1134 by iterating the ECS query). So the total allocations on a cold tick are: 1 (entities vec) + N_peers (per-peer vecs) = 4 Vecs of ~96KB each.

### Contrast with full snapshot path

The full snapshot path at line 1146-1149 avoids per-peer cloning:
```rust
outbound.full_snapshot = Some(entities); // moves, no clone
```

But the tiered path cannot use this optimization because each peer may have a different hot/cold split.

## Impact

On hot-only ticks (3 out of 4), the clone cost is small because few entities pass the filter. On cold ticks (1 out of 4), the cost is proportional to `E_auth * N_peers`. At 2000 entities and 3 peers, the 288KB of allocations take ~0.3ms on the UNO Q boards (memory allocation + memcpy). At 5000 entities and 5 peers, this grows to ~1.2MB and ~1ms.

This is not a correctness issue. It is a tick budget concern at scale. The existing per-client clone finding (#057) notes a similar pattern for client snapshots.

## Recommended Fix

On cold ticks where all entities will be included for a peer, reference-count the Vec instead of cloning it per-peer:

```rust
// Pre-compute: on cold ticks, all peers with player positions get all entities.
let cold_all: Option<Arc<Vec<EntityState>>> = if is_cold_tick {
    Some(Arc::new(entities.clone()))
} else {
    None
};

for (&peer_id, _all_positions) in &peer_all_positions {
    let relevant = if is_cold_tick {
        // ... directly use/clone from the Arc'd vec or restructure OutboundReplication
        // to accept Arc<Vec<EntityState>> per peer
    } else {
        // Hot-only path: filter as before
    };
}
```

Alternatively, since `OutboundReplication.per_peer` takes `Vec<EntityState>`, the simplest optimization is to detect when all entities pass (cold tick + tiered path) and use `entities.clone()` directly rather than filtering:

```rust
if is_cold_tick && player_positions.is_some() {
    hot_this_peer = /* count hot */;
    cold_this_peer = entities.len() - hot_this_peer;
    outbound.per_peer.insert(peer_id, entities.clone());
    continue;
}
```

This still clones per-peer but avoids the filter overhead. The real fix is restructuring `OutboundReplication` to support shared references, but that requires changes to the game loop consumer.

## Effort: small (skip filter), medium (Arc sharing)

## Related Files
- crates/sim-server/src/systems.rs:1227-1246 (filter + clone)
- crates/sim-server/src/systems.rs:1131-1134 (entities Vec construction)
- crates/sim-server/src/resources.rs:117-130 (OutboundReplication)
- crates/sim-server/src/game_loop.rs:420-432 (per_peer consumption)
