# Dual Authority During Two-Phase Transfer Window

## Severity: high

## Category: race-condition

## Summary
During the two-phase authority transfer, the entity exists as Authoritative on BOTH the source server (marked with TransferOutbound) and the target server (which accepted and ACK'd). Between the target accepting the transfer and the source receiving the ACK, both servers simulate the entity, replicate it to peers, and broadcast it to clients -- each with diverging state. This creates a window where inputs are applied on both sides, peer replication carries conflicting data, and clients see phantom duplicates.

## Evidence

**Source server keeps simulating.** The entity retains `Authoritative` (systems.rs:797-798):
```rust
// Two-phase: mark entity as TransferOutbound instead of despawning.
// Entity keeps Authoritative -- it stays on this server until ACK arrives.
world.entity_mut(*entity).insert(TransferOutbound { ... });
```

The `prepare_replication` system (systems.rs:940-967) queries `With<Authoritative>` with no exclusion for `TransferOutbound`:
```rust
pub fn prepare_replication(
    ...
    query: Query<..., With<Authoritative>>,
) {
```

The `physics_integration` system (systems.rs:707-713) also queries `With<Authoritative>` without filtering:
```rust
pub fn physics_integration(
    mut query: Query<(&mut Position, &Velocity), With<Authoritative>>,
)
```

Same for `npc_behavior` (systems.rs:678-700), `clamp_positions` (systems.rs:721-727).

**Target server starts simulating immediately.** Upon receiving `AuthorityTransfer`, the target promotes the shadow to `Authoritative` or spawns as authoritative (systems.rs:150-192) and sends the ACK. From that point the target server simulates, replicates, and broadcasts the entity.

**Both servers replicate to peers.** The source sends the entity as authoritative (still has `Authoritative`), while the target also sends it as authoritative. Every peer server receives two conflicting `EntityStateUpdate` messages for the same entity, from different servers, with different positions/velocities. The receiver at systems.rs:86-118 uses tick-based conflict resolution, but both servers may be on the same or adjacent ticks.

**Input processing on source.** The `process_client_inputs` system checks `world.get::<Authoritative>(bevy_entity).is_some()` (systems.rs:463) with no check for `TransferOutbound`. A client input for a player entity being transferred is processed locally on the source, modifying velocity -- but the target will never see that input since it arrived after the transfer was sent.

## Impact
- **Duration**: The window lasts from when the target processes the transfer (at most 1 tick after the UDP send) until the source receives and processes the ACK (1+ ticks for UDP roundtrip). At 20Hz on embedded hardware with WiFi, this is typically 2-4 ticks (100-200ms). With a retry, it extends to 500ms+.
- **Replication conflicts**: Peer servers receive two `EntityStateUpdate` messages with the same entity ID but different `authority_server` values and different positions. The tick-based resolution picks the higher tick, but if both are at the same tick, the last-received wins nondeterministically.
- **Client confusion**: Clients connected to the source server still see the entity simulated locally. Clients connected to the target also see it. If both broadcast to the same client (via shadow data), the client may see positional jitter.
- **Player input loss**: For player entities during drain, a MoveInput arriving on the source during the ACK window is applied locally but never reaches the target. The target has the state from the transfer moment, not the post-input state.

## Recommended Fix

**Option A (Minimal):** Exclude `TransferOutbound` entities from simulation and replication on the source.

In `prepare_replication`, change the query filter from `With<Authoritative>` to `(With<Authoritative>, Without<TransferOutbound>)`. Apply the same filter to `physics_integration`, `npc_behavior`, `clamp_positions`, and `prepare_client_snapshot` (for the auth_query).

For input handling in `process_client_inputs`, when the entity has `TransferOutbound`, forward the input to the target server instead of processing locally.

**Option B (Complete):** Replace `Authoritative` with a state machine: `Authoritative`, `TransferPending`, `TransferredAway`. Only entities in `Authoritative` state are simulated and replicated. `TransferPending` entities are frozen and excluded from simulation/replication. `TransferredAway` entities are cleaned up after ACK.

Option A is simpler and sufficient. The key insight is that once a transfer is initiated, the source should stop treating the entity as active -- it should be a frozen snapshot awaiting confirmation.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:796-804 (TransferOutbound insertion)
- crates/sim-server/src/systems.rs:940-967 (prepare_replication, no TransferOutbound filter)
- crates/sim-server/src/systems.rs:707-713 (physics_integration, no TransferOutbound filter)
- crates/sim-server/src/systems.rs:678-700 (npc_behavior, no TransferOutbound filter)
- crates/sim-server/src/systems.rs:463 (process_client_inputs checks Authoritative but not TransferOutbound)
- crates/sim-server/src/systems.rs:150-192 (target immediately starts simulating)
