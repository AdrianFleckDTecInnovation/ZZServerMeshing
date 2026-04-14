# Inbound Swap Holds Shared Mutex During ECS Resource Write

## Severity: low

## Category: bottleneck

## Summary
The Phase 1 inbound swap in the game loop holds the `std::sync::Mutex` for both the `mem::take` drain phase AND the ECS resource write phase. The ECS writes (field assignments) do not access the shared buffer and do not need the lock. The lock is held approximately twice as long as necessary, blocking the UDP receiver and WebSocket handler tasks from pushing new data during that window.

## Evidence
`game_loop.rs:179-206`:
```rust
{
    let mut iq = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
    // ... mem::take from iq (needs lock) ...
    let peer_updates = std::mem::take(&mut iq.peer_updates);
    // ... 9 more mem::take calls ...

    // Lock is STILL held here
    let mut ecs_queue = world.resource_mut::<InboundQueue>();
    ecs_queue.peer_updates = peer_updates;  // does not touch iq
    // ... 9 more assignments ...
}  // lock released
```

The lock guard `iq` is alive through the entire block. The ECS resource writes at lines 195-205 only touch local variables and the ECS world -- they do not access `iq`. But since `iq` is not explicitly dropped, the mutex remains locked.

Additionally, line 181 acquires `world.resource_mut::<InboundQueue>()` (the ECS resource), which is unused and immediately shadowed at line 193 `drop(ecs_queue)` then line 195 re-acquires. This double-borrow is dead code from a refactor.

## Impact
At 20Hz with `std::sync::Mutex`, the lock is held for ~10 `mem::take` calls (pointer swaps, sub-microsecond) plus ~10 field assignments (also pointer swaps, sub-microsecond). Total critical section: well under 1 microsecond. The extra time from holding the lock during ECS writes roughly doubles this to ~2 microseconds.

The receiver task and WebSocket handlers contend on this lock at packet arrival time. With 2 peers sending 20 packets/sec each (40 lock acquisitions/sec) and N clients, the probability of contention during the extra ~1 microsecond is negligible at current scale.

This is not a practical problem at the current entity counts. It becomes more relevant if the `InboundQueue` grows additional fields or if the ECS resource write becomes more expensive in the future.

## Recommended Fix
Drop the mutex guard before writing to ECS:
```rust
let (peer_updates, authority_transfers, /* ... */) = {
    let mut iq = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
    (
        std::mem::take(&mut iq.peer_updates),
        std::mem::take(&mut iq.authority_transfers),
        // ...
    )
};
// Lock released here
let mut ecs_queue = world.resource_mut::<InboundQueue>();
ecs_queue.peer_updates = peer_updates;
// ...
```

Also remove the unused `resource_mut::<InboundQueue>()` at line 181 and the `drop(ecs_queue)` at line 193.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:179-206 (inbound swap with lock held too long)
- crates/sim-server/src/game_loop.rs:207-219 (coord events swap -- same pattern, same issue)
