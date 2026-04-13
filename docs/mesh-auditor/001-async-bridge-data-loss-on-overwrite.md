# Async-Bridge Swap Overwrites Unconsumed Inbound Data

## Severity: high

## Category: data-loss

## Summary
The game loop's Phase 1 swaps inbound data from async buffers into ECS resources using `std::mem::take`, but it unconditionally replaces the ECS-side queue without checking whether the previous tick's data was fully consumed. If the `drain_inbound_events` system fails to fully drain the ECS `InboundQueue` (e.g., because the schedule didn't run due to an earlier panic, or a new code path forgets to drain a field), the next swap silently discards whatever was left.

More critically, the swap pattern itself has a subtle bug: it acquires the `ecs_queue` resource, takes data out of the *async* queue, drops the ECS resource reference, then re-acquires the ECS resource to write the data in. Between the drop and the re-acquire, nothing can race (this is single-threaded World access), but the existing data in the ECS queue from a previous incomplete drain **is unconditionally overwritten**.

## Evidence
`game_loop.rs` lines 91-106:
```rust
let mut iq = inbound.queue.lock().await;
let ecs_queue = world.resource_mut::<InboundQueue>();
// Takes from async buffer
let peer_updates = std::mem::take(&mut iq.peer_updates);
// ...
drop(ecs_queue);  // drops without reading existing ECS data

let mut ecs_queue = world.resource_mut::<InboundQueue>();
ecs_queue.peer_updates = peer_updates;  // OVERWRITES, doesn't extend
```

Meanwhile, `drain_inbound_events` (systems.rs lines 31-35) drains `peer_updates`, `authority_transfers`, and `forwarded_inputs` -- but does NOT drain `client_inputs`. That field is only drained by `process_client_inputs` (line 179). Both systems run in the same tick, so under normal operation this works. But if `process_client_inputs` were to skip (due to a schedule ordering error or conditional logic), the client inputs from that tick would be overwritten on the next swap.

Additionally, the game loop acquires `ecs_queue` at line 93 but only uses it to take from the *async* queue at lines 95-98. It never reads or extends the ECS-side data. The first `ecs_queue` reference is wasted.

## Impact
Under normal operation, this is safe because the schedule runs to completion every tick and all systems consume their data. The risk materializes if:
1. A system panics mid-schedule (ECS data partially consumed, then overwritten next tick)
2. Future refactoring adds conditional drain logic
3. The schedule order is modified so drain happens after swap (all prior data lost)

For authority transfers specifically, losing even one transfer message means an entity permanently disappears -- the source server already despawned it, and the target never received it.

## Recommended Fix
Change the swap pattern to **extend** rather than **replace**:
```rust
let mut ecs_queue = world.resource_mut::<InboundQueue>();
ecs_queue.peer_updates.extend(std::mem::take(&mut iq.peer_updates));
ecs_queue.authority_transfers.extend(std::mem::take(&mut iq.authority_transfers));
ecs_queue.forwarded_inputs.extend(std::mem::take(&mut iq.forwarded_inputs));
ecs_queue.client_inputs.extend(std::mem::take(&mut iq.client_inputs));
```
This ensures that unconsumed data from a prior tick is preserved and processed eventually, rather than silently dropped.

Also remove the dead first `ecs_queue` acquisition at line 93 (it serves no purpose).

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:91-106
- crates/sim-server/src/systems.rs:26-170 (drain_inbound_events)
- crates/sim-server/src/systems.rs:177-304 (process_client_inputs)
