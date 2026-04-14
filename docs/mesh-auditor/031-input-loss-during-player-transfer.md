# Player Input Lost During Authority Transfer Window

## Severity: high

## Category: data-loss

## Summary
When a player entity is being transferred (has `TransferOutbound`), the source server still accepts and processes MoveInput locally because the input handler only checks for `Authoritative`, not `TransferOutbound`. These inputs modify the entity's velocity and last_input_seq on the source, but this updated state is never propagated to the target server. The target has the entity state from the transfer initiation moment. The player's inputs during the entire ACK window (100-200ms+) are silently dropped.

## Evidence

The `process_client_inputs` system handles MoveInput at systems.rs:463-484:
```rust
if world.get::<Authoritative>(bevy_entity).is_some() {
    // Process locally
    let kind = world.get::<EntityKind>(bevy_entity).map(|k| k.0);
    let speed = match kind {
        Some(EntityType::Player) => 5.0,
        _ => 2.0,
    };
    if let Some(mut vel) = world.get_mut::<Velocity>(bevy_entity) {
        vel.0 = dir.normalize_or_zero() * speed;
    }
    if let Some(mut input_seq) = world.get_mut::<LastInputSeq>(bevy_entity) {
        input_seq.0 = seq;
    }
```

There is no check for `TransferOutbound`. The entity still has `Authoritative`, so the input is processed. But once the ACK arrives and the entity is despawned (systems.rs:255-280), all those post-transfer inputs are lost.

The same applies to forwarded inputs (systems.rs:196-222): if a forwarded input arrives for an entity with `TransferOutbound`, it is applied locally and lost.

**The target server never sees these inputs.** The target has the entity state from the `full_state` field of the `AuthorityTransfer` message, which was serialized before these inputs arrived. The target starts simulating from that snapshot.

**The client's prediction is broken.** The client sends inputs with increasing seq numbers. The source processes them (incrementing `last_input_seq`), but the target starts with the old `last_input_seq` from the transfer message. When the client reconciles against the target's snapshot, it sees a regression in `last_input_seq` and must replay all inputs since the regression point -- causing visible rubber-banding.

## Impact
- **Input loss duration**: TRANSFER_TIMEOUT_TICKS is 10 (500ms), plus 1-2 ticks for the ACK roundtrip, giving a typical 150-600ms window where player inputs are accepted but ultimately lost.
- **Player experience**: The player sees their character moving (prediction), then when the transfer completes and the target starts broadcasting, the character snaps back to where it was 150-600ms ago. The prediction system then replays unacknowledged inputs, causing rubber-banding.
- **During drain**: All entities including players are transferred. The drain process may take multiple seconds, during which the player is actively playing and sending inputs that are all silently dropped.
- **Forwarded inputs doubly lost**: If the player's authority was on server A, which forwards to server B (the current authority), and server B initiates a transfer to server C -- the forwarded input arrives at server B, is applied to the TransferOutbound entity, and is never seen by server C.

## Recommended Fix

**Option A (Pair with #028 fix):** If finding #028 is fixed by excluding TransferOutbound entities from simulation, also change the input handler to forward inputs for TransferOutbound entities to the target server:

```rust
if world.get::<Authoritative>(bevy_entity).is_some() {
    if let Some(outbound) = world.get::<TransferOutbound>(bevy_entity) {
        // Entity is being transferred -- forward input to target
        let msg = PeerMessage::ForwardedInput { entity_id: eid, direction, seq };
        if let Ok(bytes) = bincode::serialize(&msg) {
            world.resource_mut::<OutboundForwards>()
                .messages.push((outbound.target_addr, bytes));
        }
        continue;
    }
    // Normal local processing...
}
```

**Option B (Simpler):** Accept the input loss and mitigate on the client. After a transfer, the client receives a Reconnect message pointing to the new server. The prediction system can detect the seq regression and handle it gracefully (clear prediction buffer, start fresh). This is simpler but gives a worse player experience.

**Option C (Best):** Combine with the retry fix (#030) -- when the ACK arrives, the source re-serializes the current entity state (with all accumulated inputs) and sends a final state update to the target before despawning. This ensures no inputs are lost, but adds protocol complexity.

## Effort: small (Option A), medium (Option C)

## Related Files
- crates/sim-server/src/systems.rs:451-484 (process_client_inputs MoveInput handling)
- crates/sim-server/src/systems.rs:196-222 (forwarded input handling)
- crates/sim-server/src/systems.rs:255-280 (process_transfer_acks despawns entity)
- crates/sim-server/src/components.rs:42-48 (TransferOutbound has target_addr)
