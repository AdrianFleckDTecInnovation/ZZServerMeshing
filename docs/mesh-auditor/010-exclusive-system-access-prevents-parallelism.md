# Exclusive World Access Prevents ECS System Parallelism

## Severity: medium

## Category: bottleneck

## Summary
Six of the twelve ECS systems use `fn(world: &mut World)` (exclusive system access) instead of Bevy's query-based system parameters. Exclusive systems cannot run in parallel with any other system because they hold a mutable borrow on the entire World. This means Bevy's automatic parallelism is completely disabled -- all twelve systems run sequentially, even though several of them (like `npc_behavior`, `physics_integration`, `clamp_positions`) are independent and could run concurrently.

## Evidence
Systems using exclusive `&mut World` access:
1. `drain_inbound_events(world: &mut World)` -- line 26
2. `process_client_inputs(world: &mut World)` -- line 177
3. `process_coord_events(world: &mut World)` -- line 311
4. `process_transfers(world: &mut World)` -- line 464
5. `process_drain(world: &mut World)` -- line 544
6. `shadow_cleanup(world: &mut World)` -- line 704

Systems using proper query parameters (could run in parallel):
1. `npc_behavior` -- line 408
2. `physics_integration` -- line 437
3. `clamp_positions` -- line 451
4. `prepare_replication` -- line 620
5. `prepare_client_snapshot` -- line 654
6. `record_metrics` -- line 725

The schedule (`schedule.rs`) explicitly chains all systems sequentially with `.after()` constraints, so even the parallelizable systems are forced to run one after another.

The exclusive systems use `&mut World` because they need to:
- `drain_inbound_events`: Spawns/modifies entities via `spawn_from_entity_state_direct`
- `process_client_inputs`: Spawns entities, modifies components, reads/writes `EntityLookup`
- `process_coord_events`: Modifies `PeerAddresses`, modifies entity components during recovery
- `process_transfers`: Reads components, serializes, despawns entities
- `process_drain`: Same as transfers
- `shadow_cleanup`: Despawns entities

These genuinely need exclusive access because they spawn/despawn entities. However, some could be refactored to use `Commands` for deferred spawning, moving the exclusive access to Bevy's command application phase.

## Impact
On a single-threaded game loop (which this is -- the schedule runs on a single thread via `schedule.run(&mut world)`), the impact is zero. All systems already run sequentially.

The impact becomes significant if the game loop is ever migrated to Bevy's multi-threaded executor, which is a natural scaling path for handling more entities. With the current design, that migration would provide zero benefit because all systems are either exclusive or explicitly chained.

Even on a single thread, the exclusive systems prevent Bevy from applying archetype-level optimizations (like batched component access) because `&mut World` bypasses the query system entirely.

## Recommended Fix
For the systems that only need to read/write specific resources and modify entities, refactor to use `Commands` for deferred entity operations:

```rust
// Instead of:
pub fn process_client_inputs(world: &mut World) {
    // ...
    spawn_from_entity_state_direct(world, &state, true, None);
}

// Refactor to:
pub fn process_client_inputs(
    mut commands: Commands,
    mut queue: ResMut<InboundQueue>,
    config: Res<ServerConfig>,
    mut lookup: ResMut<EntityLookup>,
    // ...
) {
    let inputs = std::mem::take(&mut queue.client_inputs);
    for input in inputs {
        // Use commands.spawn() instead of world.spawn()
    }
}
```

This is a larger refactoring effort. As an incremental step, the schedule ordering constraints should be relaxed where possible to allow `npc_behavior`, `physics_integration`, and `clamp_positions` to run in parallel (they only access `Authoritative` entities and don't conflict with each other beyond write access to `Position` and `Velocity`).

## Effort: large

## Related Files
- crates/sim-server/src/systems.rs:26 (drain_inbound_events -- exclusive)
- crates/sim-server/src/systems.rs:177 (process_client_inputs -- exclusive)
- crates/sim-server/src/systems.rs:311 (process_coord_events -- exclusive)
- crates/sim-server/src/systems.rs:464 (process_transfers -- exclusive)
- crates/sim-server/src/systems.rs:544 (process_drain -- exclusive)
- crates/sim-server/src/systems.rs:704 (shadow_cleanup -- exclusive)
- crates/sim-server/src/schedule.rs:12-28 (fully sequential schedule)
