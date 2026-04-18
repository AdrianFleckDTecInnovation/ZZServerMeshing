# Authority Transfer Is Fire-and-Forget Over UDP

## Severity: critical

## Category: data-loss

## Summary
When a server transfers authority of an entity (during rebalance or drain), it serializes the transfer message, queues it for UDP send, and immediately despawns the entity locally. If the UDP packet is lost, the entity is permanently destroyed -- it no longer exists on the source server, and the target server never received it. There is no acknowledgment, no retry, and no recovery mechanism.

## Evidence
`systems.rs` `process_transfers`, lines 519-528:
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world
        .resource_mut::<OutboundTransfers>()
        .messages
        .push((target_addr, bytes));
    transferred += 1;
}

// Despawn IMMEDIATELY -- before the UDP packet is even sent
despawn_and_remove(world, *entity, *eid);
```

The actual UDP send happens later in `game_loop.rs` lines 184-188:
```rust
for (addr, bytes) in &transfers {
    if let Err(e) = udp_socket.send_to(bytes, addr).await {
        tracing::warn!("Failed to send transfer: {e}");
    }
}
```

The same pattern exists in `process_drain` (lines 593-601), which transfers ALL entities during graceful shutdown. If even one packet is lost during drain, that entity is gone forever.

## Impact
- **Rebalance**: Losing a transfer packet loses an entity permanently. With 10% of entities transferred per rebalance cycle, even a 1% packet loss rate means ~0.1% of entities are destroyed per rebalance.
- **Drain**: During graceful shutdown, ALL entities are transferred in a single tick. A single lost packet means a permanently lost entity. On congested networks (e.g., UNO Q boards on WiFi), this is not unlikely.
- **Player entities**: While `process_transfers` only transfers NPCs, `process_drain` transfers ALL authoritative entities including players. A player entity lost during drain means the player's state is destroyed.

## Recommended Fix
Implement a two-phase transfer protocol:

**Phase 1 (Initiate)**: Source sends `AuthorityTransfer` but keeps the entity in a "pending transfer" state (new component `PendingTransfer { target: ServerId, sent_tick: u64 }`). Entity continues to be simulated and replicated.

**Phase 2 (Acknowledge)**: Target sends `TransferAck { entity_id }` back to source. Source despawns the entity only after receiving the ack.

**Timeout**: If no ack within N ticks (e.g., 40 = 2 seconds), re-send the transfer. After M retries, abort the transfer and keep the entity.

This requires:
1. New `PeerMessage::TransferAck` variant in `shared/protocol.rs`
2. New `PendingTransfer` component in `components.rs`
3. A `process_transfer_acks` system that handles incoming acks and despawns confirmed entities
4. A `retry_pending_transfers` system that re-sends stale pending transfers

**Interim mitigation** (much simpler): Don't despawn until the next tick after sending. Keep the entity as "transferring" and only despawn when a shadow update arrives from the target server (proving it received the entity). This is imperfect but prevents the immediate despawn-before-send race.

## Effort: medium

## Related Files
- crates/sim-server/src/systems.rs:464-537 (process_transfers)
- crates/sim-server/src/systems.rs:544-613 (process_drain)
- crates/sim-server/src/game_loop.rs:183-195 (UDP send)
- crates/shared/src/protocol.rs:57-78 (PeerMessage)
