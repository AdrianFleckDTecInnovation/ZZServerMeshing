# Drain System Despawns Entities While Iterating Over Collected Handles

## Severity: high

## Category: race-condition

## Summary
The `process_drain` and `process_transfers` systems collect entity handles into a Vec, then iterate over that Vec calling `despawn_and_remove` for each entity. After despawning an entity, subsequent iterations access the same `world` to read components of other entities. While this is safe in Bevy ECS (despawned entity handles simply return `None` for component queries), there is a more subtle problem: `despawn_and_remove` calls `world.despawn(entity)`, which can invalidate internal ECS storage, and then the loop continues to call `world.get::<Component>(*entity)` on later entries that may reference relocated storage.

In practice, Bevy ECS handles this correctly (entity handles are stable after despawn), but the pattern creates a fragile dependency on Bevy's internal guarantees that could break on a major version upgrade.

More concretely, the `process_drain` system at lines 567-601 despawns entities *while still iterating over the collected list*, and each despawn modifies the `EntityLookup` resource. If the Vec contains duplicate entity handles (which shouldn't happen but isn't asserted), despawning the same entity twice would be a logic error.

## Evidence
`systems.rs` lines 567-601 (process_drain):
```rust
for (i, (entity, eid)) in all_entities.iter().enumerate() {
    // ... build entity state from components ...
    let nid = world.get::<NetworkId>(*entity).unwrap();  // Could panic if entity was despawned
    let pos = world.get::<Position>(*entity).unwrap();
    // ...

    despawn_and_remove(world, *entity, *eid);  // Despawns entity, modifies EntityLookup
}
```

The `.unwrap()` calls on component access are safe because we just collected these entities from a query and nothing else despawns them between collection and this loop. However, the use of `unwrap()` rather than `if let Some(...)` means a panic would crash the server if any invariant is violated.

`systems.rs` lines 501-528 (process_transfers):
```rust
for (entity, eid, _) in npc_candidates.iter().take(transfer_count) {
    let nid = world.get::<NetworkId>(*entity).unwrap();
    // ...
    despawn_and_remove(world, *entity, *eid);
}
```

Same pattern -- `unwrap()` on component access for entities being despawned in the same loop.

## Impact
- **Normal operation**: Works correctly because entity handles are unique and Bevy's `get` returns `None` only if the entity doesn't exist.
- **Panic risk**: If any invariant is violated (e.g., an entity is despawned by another code path between collection and iteration), `unwrap()` panics and crashes the server.
- **Data loss during drain**: A server crash during drain means all entities being drained are lost (the source server crashes, and UDP transfer packets may or may not have been sent).

## Recommended Fix
1. Replace `unwrap()` with `if let Some(...)` guards and `continue` on failure:
```rust
let Some(nid) = world.get::<NetworkId>(*entity) else { continue };
let Some(pos) = world.get::<Position>(*entity) else { continue };
```

2. Separate the "collect state" phase from the "despawn" phase:
```rust
// Phase 1: Collect all transfer data
let transfers: Vec<(Entity, EntityId, EntityState, SocketAddr)> = ...;

// Phase 2: Send all transfers
for (_, _, state, addr) in &transfers {
    // serialize and queue
}

// Phase 3: Despawn all transferred entities
for (entity, eid, _, _) in &transfers {
    despawn_and_remove(world, *entity, *eid);
}
```

This makes the phases cleanly separated and eliminates the interleaved mutation pattern.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:500-528 (process_transfers -- despawn during iteration)
- crates/sim-server/src/systems.rs:567-601 (process_drain -- despawn during iteration)
- crates/sim-server/src/conversion.rs:65-74 (despawn_and_remove)
