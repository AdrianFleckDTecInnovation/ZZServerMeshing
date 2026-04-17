# TransferOutbound Stores Full Serialized Message Per Entity

## Severity: medium

## Category: scalability

## Summary
Each entity in a two-phase transfer stores the full bincode-serialized `AuthorityTransfer` message in `TransferOutbound.serialized_msg: Vec<u8>`. For a single entity, the serialized message is approximately 100-150 bytes (UUID + 3 Vec3s + enums + tick + server IDs). During a drain with hundreds or thousands of entities, every entity carries its own copy of this serialized data in ECS memory, adding ~100-150KB overhead per 1000 entities. On embedded hardware with limited RAM, this is a concern during bulk transfers.

## Evidence

The component definition in components.rs:42-48:
```rust
pub struct TransferOutbound {
    pub target_server: u16,
    pub target_addr: std::net::SocketAddr,
    pub sent_tick: u64,
    pub retry_count: u8,
    pub serialized_msg: Vec<u8>,
}
```

The serialized message is created and cloned in systems.rs:790-804:
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes.clone()));
    world.entity_mut(*entity).insert(TransferOutbound {
        ...
        serialized_msg: bytes,
    });
}
```

And in the drain path, systems.rs:881-894, the same pattern:
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes.clone()));
    world.entity_mut(*entity).insert(TransferOutbound {
        ...
        serialized_msg: bytes,
    });
}
```

The `bytes.clone()` means two copies of the serialized message exist: one in `OutboundTransfers` (sent and freed at end of tick) and one in the ECS component (held until ACK or timeout).

## Impact
- **Memory**: With a bincode-serialized `AuthorityTransfer` at ~120 bytes per entity, a drain of 1000 entities stores ~120KB of redundant serialized data. Not catastrophic, but it adds to the memory pressure on UNO Q boards (512MB).
- **Amplified by retry fix**: If finding #030 is fixed by re-serializing on retry, the `serialized_msg` field becomes unnecessary and can be removed entirely, eliminating this overhead.
- **Heap fragmentation**: Each `Vec<u8>` is a separate heap allocation. 1000 small heap allocations during drain contribute to fragmentation.

## Recommended Fix

If finding #030 is implemented (re-serialize on retry from current ECS state), remove the `serialized_msg` field from `TransferOutbound` entirely. The component becomes:
```rust
pub struct TransferOutbound {
    pub target_server: u16,
    pub target_addr: std::net::SocketAddr,
    pub sent_tick: u64,
    pub retry_count: u8,
}
```

If retries must use cached serialization for performance, consider storing a shared `Arc<Vec<u8>>` to avoid the clone, or accept the ~120 bytes per entity as acceptable overhead.

## Effort: small

## Related Files
- crates/sim-server/src/components.rs:42-48 (TransferOutbound definition)
- crates/sim-server/src/systems.rs:790-804 (process_transfers serialized_msg creation)
- crates/sim-server/src/systems.rs:881-894 (process_drain serialized_msg creation)
- crates/sim-server/src/systems.rs:311-314 (retry re-sends cached serialized_msg)
