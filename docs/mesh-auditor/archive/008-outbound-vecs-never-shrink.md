# Outbound Vec Buffers Grow But Never Shrink

## Severity: medium

## Category: scalability

## Summary
The outbound message buffers (`OutboundTransfers.messages`, `OutboundForwards.messages`) are Vecs that are drained with `std::mem::take` each tick, which replaces them with empty Vecs. However, the replacement Vec has zero capacity. On the next tick, if messages are pushed again, the Vec re-allocates. This creates an allocation-deallocation cycle every tick when transfers or forwards are active.

Conversely, the inbound buffers in `SharedInbound` are drained with `std::mem::take` in the game loop but the async tasks continuously push to them. The async-side Vecs grow to accommodate burst traffic and never shrink, accumulating high-water-mark capacity.

## Evidence
`game_loop.rs` lines 132-133:
```rust
let transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
let forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);
```

`std::mem::take` for a `Vec` replaces with `Vec::new()` (capacity 0). If the next tick pushes 10 messages, it allocates for 10. Then take drains it to capacity 0 again. This oscillation causes allocation every tick that has outbound messages.

For the inbound side, `receiver.rs` line 38:
```rust
let mut q = inbound.queue.lock().await;
q.peer_updates.push(PeerUpdate { ... });
```

The async-side queue grows but is only emptied by `std::mem::take` in the game loop. Between takes, the Vec capacity grows to the peak burst size and stays there. With 3 peers sending 500-entity updates, `peer_updates` can grow to capacity 3 and `PeerUpdate.entities` to capacity 500 each. This ~75KB stays allocated in the async queue permanently.

## Impact
- **Outbound oscillation**: Causes heap fragmentation from repeated alloc/dealloc of small Vecs. With 10 transfers per rebalance, this is ~10 * (sizeof(SocketAddr) + sizeof(Vec<u8>)) = ~640 bytes per tick during rebalancing. Minor but avoidable.
- **Inbound high-water mark**: The async-side Vecs never shrink, so peak memory usage persists even after a burst subsides. Not a problem for typical workloads but could matter on memory-constrained hardware (UNO Q boards with 8GB RAM, running multiple servers).

## Recommended Fix
For outbound buffers, use `drain(..)` instead of `take` to preserve capacity:
```rust
let transfers: Vec<_> = world.resource_mut::<OutboundTransfers>().messages.drain(..).collect();
```

Or, even better, swap with a pre-allocated buffer:
```rust
// Keep a reusable buffer in the game loop
let mut transfer_buf = Vec::with_capacity(16);
std::mem::swap(&mut transfer_buf, &mut world.resource_mut::<OutboundTransfers>().messages);
// Process transfer_buf
// At end of tick:
transfer_buf.clear();
// transfer_buf retains capacity for next tick
```

For inbound buffers, periodically shrink if capacity exceeds 4x the typical size:
```rust
if q.peer_updates.capacity() > q.peer_updates.len() * 4 && q.peer_updates.capacity() > 64 {
    q.peer_updates.shrink_to(q.peer_updates.len() * 2);
}
```

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:129-133 (outbound take)
- crates/sim-server/src/game_loop.rs:95-98 (inbound take)
- crates/sim-server/src/receiver.rs:37-43 (async push to inbound)
- crates/sim-server/src/resources.rs:59-75 (outbound resource definitions)
