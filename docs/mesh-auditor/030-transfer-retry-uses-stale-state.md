# Transfer Retry Sends Stale Entity State

## Severity: high

## Category: data-loss

## Summary
When a two-phase authority transfer times out and is retried, the retry re-sends the original serialized message (`TransferOutbound.serialized_msg`) which contains the entity state from the moment the transfer was first initiated. Meanwhile, the entity has continued to be simulated on the source server for 500ms+ (10+ ticks). The target server receives outdated position, velocity, and last_input_seq data, causing a visible teleport backward and potential input sequence regression for player entities.

## Evidence

The retry in process_transfer_timeouts (systems.rs:311-330):
```rust
for (entity, eid, target_addr, serialized_msg) in to_retry {
    world
        .resource_mut::<OutboundTransfers>()
        .messages
        .push((target_addr, serialized_msg));  // <-- stale bytes from initial send

    if let Some(mut outbound) = world.get_mut::<TransferOutbound>(entity) {
        outbound.retry_count += 1;
        outbound.sent_tick = current_tick;
```

The `serialized_msg` is captured at transfer initiation time (systems.rs:790-804):
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes.clone()));
    world.entity_mut(*entity).insert(TransferOutbound {
        ...
        serialized_msg: bytes,  // <-- snapshot of entity state at this tick
    });
```

The retry updates `sent_tick` but does NOT rebuild the serialized message from current component state. The entity has been simulated for `TRANSFER_TIMEOUT_TICKS` (10 ticks = 500ms) since the original send.

## Impact
- **NPC teleport**: An NPC moving at 3 units/sec for 500ms has moved 1.5 units. The retry sends the old position. When the target accepts, the entity appears to jump backward by 1.5 units on all clients connected to the target. The source then despawns it, so the old position disappears from source clients. Net effect: visible teleport glitch.
- **Player input regression**: For player entities (during drain), `last_input_seq` in the stale message is from 500ms ago. The target server starts with an old seq number. The client's prediction reconciliation may replay already-acknowledged inputs, causing the player to rubber-band.
- **Velocity mismatch**: The stale velocity may not match the entity's current movement direction, causing the entity to briefly move in the wrong direction on the target server.
- **Interaction with finding #028**: Since the source continues simulating during the ACK window (finding #028), the staleness is amplified -- the retry creates an even larger state gap.

## Recommended Fix

When retrying, rebuild the `AuthorityTransfer` message from the entity's current ECS components instead of re-sending stale bytes. Replace the clone approach with a re-serialization:

```rust
// In process_transfer_timeouts, for retries:
let nid = world.get::<NetworkId>(entity).unwrap();
let pos = world.get::<Position>(entity).unwrap();
let vel = world.get::<Velocity>(entity).unwrap();
let kind = world.get::<EntityKind>(entity).unwrap();
let seq = world.get::<LastInputSeq>(entity);
let entity_state = to_entity_state(nid, pos, vel, kind,
    &AuthorityServer(ServerId(outbound.target_server)), seq);

let msg = PeerMessage::AuthorityTransfer {
    tick: current_tick,
    entity_id: eid,
    from_server: server_id,
    to_server: ServerId(outbound.target_server),
    full_state: entity_state,
};
let bytes = bincode::serialize(&msg).unwrap();
world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes.clone()));
outbound.serialized_msg = bytes;  // update stored message too
outbound.sent_tick = current_tick;
outbound.retry_count += 1;
```

This also means `TransferOutbound.serialized_msg` can be removed entirely if the recommended fix for #028 is also applied (stop simulating the entity, so no state divergence).

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:291-330 (process_transfer_timeouts)
- crates/sim-server/src/systems.rs:790-804 (initial TransferOutbound creation with serialized_msg)
- crates/sim-server/src/components.rs:42-48 (TransferOutbound struct with serialized_msg field)
