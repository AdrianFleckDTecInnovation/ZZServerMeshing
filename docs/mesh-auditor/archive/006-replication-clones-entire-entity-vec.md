# Replication Clones Entire Entity Vec Every Tick

## Severity: medium

## Category: bottleneck

## Summary
The game loop extracts the replication snapshot from the ECS world and then clones the entire `Vec<EntityState>` when constructing the `PeerMessage::EntityStateUpdate` for serialization. With N authoritative entities, this produces N * sizeof(EntityState) bytes of heap allocation per tick on top of the already-allocated snapshot.

## Evidence
`game_loop.rs` lines 129-165:
```rust
// Phase 3: Extract outbound data
let repl_data = world.resource_mut::<OutboundReplication>().snapshot.take();
// ...

// Phase 4: Send UDP replication
if let Some(entities) = &repl_data {
    // ...
    let msg = PeerMessage::EntityStateUpdate {
        tick: repl_tick,
        server_id: server_id_u16,
        entities: entities.clone(),  // <-- FULL CLONE of Vec<EntityState>
    };

    if let Ok(bytes) = bincode::serialize(&msg) {
        // ...
```

`EntityState` contains a `Vec3` (12 bytes) for position, `Vec3` for velocity, a UUID (16 bytes), enum (1 byte), ServerId (2 bytes), and u32 (4 bytes) -- approximately 60 bytes per entity on the stack, plus serde overhead.

With 500 entities: 500 * ~60 bytes = ~30KB cloned per tick. At 20Hz: ~600KB/sec of unnecessary heap allocation.

The clone is needed because `bincode::serialize` takes ownership of the message. However, bincode can serialize references -- `bincode::serialize(&msg)` where `msg` borrows the entities would avoid the clone entirely.

## Impact
- **500 entities**: ~30KB clone per tick, ~600KB/sec heap churn. Minor but measurable.
- **2000 entities**: ~120KB clone per tick, ~2.4MB/sec. Starts to affect GC pressure.
- **5000 entities**: ~300KB clone per tick, ~6MB/sec. Significant allocation pressure in the hot loop.

The serialization itself (`bincode::serialize`) also allocates a Vec<u8> for the output. So per tick you have: snapshot Vec (from prepare_replication) + clone Vec (here) + serialized bytes Vec = three allocations proportional to entity count.

## Recommended Fix
Avoid the clone by constructing the `PeerMessage` with a borrow:

```rust
let msg = PeerMessage::EntityStateUpdate {
    tick: repl_tick,
    server_id: server_id_u16,
    entities: entities,  // move, not clone -- repl_data is consumed
};
```

Since `repl_data` is `Option<Vec<EntityState>>` and is taken with `.take()`, you already own it. Use `if let Some(entities) = repl_data` (move, not borrow) to avoid the clone:

```rust
if let Some(entities) = repl_data {
    // entities is now owned, no clone needed
    let msg = PeerMessage::EntityStateUpdate {
        tick: repl_tick,
        server_id: server_id_u16,
        entities,
    };
    // ...
}
```

For the serialized bytes, consider reusing a pre-allocated buffer (`Vec<u8>` with `clear()` instead of new allocation) via `bincode::serialize_into`.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:148-179 (replication send with clone)
- crates/sim-server/src/systems.rs:620-647 (prepare_replication - allocates snapshot Vec)
