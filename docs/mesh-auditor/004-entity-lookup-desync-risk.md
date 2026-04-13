# EntityLookup Can Desync From ECS World

## Severity: high

## Category: race-condition

## Summary
The `EntityLookup` resource (a `HashMap<EntityId, Entity>`) is a manually maintained index that must stay in sync with the actual ECS world. Every spawn must insert, every despawn must remove. If any code path spawns or despawns entities without updating the lookup, the system enters an inconsistent state where the lookup points to despawned entities (dangling handles) or entities exist without lookup entries (invisible to network code).

## Evidence
There are two code paths for entity lifecycle:

**Safe path** -- `conversion.rs` functions:
- `spawn_from_entity_state_direct()` (line 8): spawns entity AND updates lookup
- `despawn_and_remove()` (line 65): removes from lookup AND despawns entity

**Unsafe paths** -- direct ECS operations that bypass the lookup:
1. `systems.rs` lines 388-395, crash recovery promotion:
```rust
for entity in to_promote {
    world.entity_mut(entity).remove::<Shadow>();
    world.entity_mut(entity).insert(Authoritative);
    if let Some(mut auth) = world.get_mut::<AuthorityServer>(entity) {
        auth.0 = server_id;
    }
}
```
This modifies entities but does NOT update the lookup. However, since the entity already existed as a shadow, its lookup entry is still valid. This path is correct.

2. The real risk is future code. There is no compile-time enforcement that entity creation/deletion goes through `spawn_from_entity_state_direct` / `despawn_and_remove`. Any developer adding a new system could directly call `world.spawn(...)` or `world.despawn(entity)` without updating the lookup.

3. The `despawn_and_remove` function (conversion.rs line 71) has a defensive check:
```rust
if world.get_entity(entity).is_ok() {
    world.despawn(entity);
}
```
But it does NOT check whether the entity in the lookup matches the passed entity handle. If the same `EntityId` is spawned twice (e.g., from a duplicate transfer), the lookup points to the second entity, and despawning the first entity via the lookup handle operates on the wrong entity.

4. There is a concrete double-spawn scenario: if an authority transfer arrives for an entity that already exists as authoritative (duplicate transfer), the code at `systems.rs` line 87-94 correctly rejects it. But if a peer update and an authority transfer arrive in the same tick for the same previously-unknown entity, `drain_inbound_events` processes peer updates first (spawning a shadow at line 68), then processes authority transfers (line 132 spawns as authoritative since the entity IS in the lookup as a shadow, but the promote path handles this). This specific path is actually safe.

The real risk is the lookup keyed on `EntityId` (UUID). If `spawn_from_entity_state_direct` is called twice with the same `EntityId`, the second call overwrites the lookup entry, and the first Bevy entity becomes orphaned -- it exists in the ECS world but is unreachable via the lookup. It will never be despawned (shadow cleanup queries by component, not by lookup, so shadows will eventually be cleaned up -- but authoritative orphans persist forever).

## Impact
- **Orphaned entities**: Entities that exist in the ECS world but have no lookup entry. They consume memory, are included in client snapshots and replication (because those systems query by component, not by lookup), and can never be despawned via `RemoveEntity` client messages.
- **Dangling lookup entries**: Lookup points to a despawned entity handle. Queries via the lookup return `None` for components, which is handled with `if let Some(...)` guards. Functional impact is that inputs for those entities silently fail.
- **Memory leak**: Orphaned entities accumulate over time, growing the ECS world and increasing per-tick query cost.

## Recommended Fix
1. Add a debug assertion to `spawn_from_entity_state_direct` that panics if the EntityId already exists in the lookup:
```rust
debug_assert!(
    !world.resource::<EntityLookup>().0.contains_key(&state.id),
    "EntityId {} already in lookup -- duplicate spawn",
    state.id
);
```

2. Add a periodic consistency check system (runs every 100 ticks) that verifies:
   - Every entity in the lookup maps to a live ECS entity
   - Every ECS entity with a `NetworkId` component has a corresponding lookup entry
   Log warnings for any discrepancies.

3. Consider making `EntityLookup` private and only accessible through `spawn_from_entity_state_direct` / `despawn_and_remove`, but this conflicts with the direct `world.resource::<EntityLookup>()` reads throughout the systems.

## Effort: small

## Related Files
- crates/sim-server/src/resources.rs:22-23 (EntityLookup definition)
- crates/sim-server/src/conversion.rs:8-43 (spawn_from_entity_state_direct)
- crates/sim-server/src/conversion.rs:65-74 (despawn_and_remove)
- crates/sim-server/src/systems.rs:26-170 (drain_inbound_events -- all lookup reads)
- crates/sim-server/src/systems.rs:464-537 (process_transfers -- lookup reads + despawn)
