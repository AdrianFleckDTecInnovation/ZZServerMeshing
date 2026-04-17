# RemoveEntity During Transfer Despawns Outbound Entity, Orphaning It On Target

## Severity: high

## Category: data-loss

## Summary
When a client disconnects and sends `RemoveEntity` for a player entity that has `TransferOutbound` (mid-transfer), the `process_client_inputs` handler matches on `Authoritative` (line 601) and calls `despawn_and_remove` immediately. This destroys the entity on the source server, including the `TransferOutbound` component. Meanwhile, the target server has already accepted the transfer and is simulating the entity as authoritative. The entity becomes permanently orphaned on the target -- no server knows to remove it, and it persists until the target server is restarted.

## Evidence

The `RemoveEntity` handler in `process_client_inputs` (systems.rs:595-643) checks `Authoritative` at line 601:
```rust
if world.get::<Authoritative>(bevy_entity).is_some() {
    // Case A: We are authoritative -- remove locally
    despawn_and_remove(world, bevy_entity, eid);
```

An entity with `TransferOutbound` still has `Authoritative` (the two-phase protocol keeps both). The handler does not check for `TransferOutbound`. The entity is despawned. When the target eventually sends a `TransferAck`, `process_transfer_acks` (line 283) looks up the entity in `EntityLookup` and finds nothing -- the ACK is silently ignored ("Transfer ACK for unknown entity, ignoring" at line 298-301).

The target server now owns the entity permanently. No `ForwardedRemove` is sent to the target because the handler takes the `Authoritative` branch (Case A), not the `Shadow` branch (Case B).

**Schedule ordering confirms this can happen**: `process_client_inputs` runs at schedule position after `drain_client_events`, which runs after `process_transfer_timeouts`. A `RemoveEntity` queued in the inbound queue is processed the same tick that `TransferOutbound` is present.

## Impact
- **Orphaned entity**: The entity lives forever on the target server as an NPC or player with no controlling client. For player entities, this is a ghost player that moves according to the last velocity (if the target did not receive a zero-velocity input).
- **Entity count inflation**: The coordinator sees the target server's entity count inflated by orphaned entities, skewing rebalance decisions.
- **During drain**: If the source is draining and a client disconnects, the player entity is despawned locally but persists on the drain target. The drain proceeds without it (entity gone from source), but the target now has an extra entity nobody asked for.
- **Probability**: This requires a client disconnect during the ACK window (100-600ms at 20Hz). During drain operations that take seconds, the probability for any individual player is low but nonzero. During rebalance, the window is shorter.

## Recommended Fix

In the `RemoveEntity` handler, check for `TransferOutbound` before despawning. If present, forward a `ForwardedRemove` to the target server AND despawn locally:

```rust
if world.get::<Authoritative>(bevy_entity).is_some() {
    // Check if entity is mid-transfer -- forward remove to target
    if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
        let target_addr = outbound.target_addr;
        let msg = PeerMessage::ForwardedRemove { entity_id: eid };
        if let Ok(bytes) = bincode::serialize(&msg) {
            world.resource_mut::<OutboundForwards>()
                .messages.push((target_addr, bytes));
        }
    }
    // Remove locally regardless
    despawn_and_remove(world, bevy_entity, eid);
}
```

This ensures the target server receives the remove request and cleans up the entity.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:595-643 (RemoveEntity handler, no TransferOutbound check)
- crates/sim-server/src/systems.rs:279-303 (process_transfer_acks, ignores ACK for unknown entity)
- crates/sim-server/src/systems.rs:249-270 (forwarded remove handler on target)
- crates/sim-server/src/components.rs:39-49 (TransferOutbound definition)
