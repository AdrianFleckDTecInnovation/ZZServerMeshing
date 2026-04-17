# Transfer ACK Is Fire-and-Forget Over UDP

## Severity: medium

## Category: data-loss

## Summary
The TransferAck message sent by the target server back to the source is a single UDP packet with no retransmission. If this ACK is lost, the source server never learns the transfer succeeded. After TRANSFER_TIMEOUT_TICKS (10 ticks / 500ms), the source retries the transfer (sending stale state per #030). After TRANSFER_MAX_RETRIES (1), the source aborts and keeps the entity. The target has already accepted the entity and is simulating it. The entity now exists as authoritative on both servers permanently.

## Evidence

The ACK is sent as a one-shot UDP message in systems.rs:35-61:
```rust
fn send_transfer_ack(world: &mut World, entity_id: EntityId, tick: u64, from_server: ServerId) {
    if let Some(&source_addr) = world.resource::<PeerAddresses>().0.get(&from_server.0) {
        let ack_msg = PeerMessage::TransferAck { entity_id, tick };
        if let Ok(bytes) = bincode::serialize(&ack_msg) {
            world.resource_mut::<OutboundTransfers>()
                .messages.push((source_addr, bytes));
        }
    }
}
```

No retry mechanism exists for the ACK. The `TransferAck` is queued once and sent once.

The source's timeout and retry logic in systems.rs:287-351 handles the case where the ACK is not received: it retries once (sending the transfer again), then aborts (removes TransferOutbound, keeps entity). But the target already has the entity as authoritative.

**The retry interaction makes things worse.** When the source retries the transfer (because the ACK was lost), the target receives a second `AuthorityTransfer` for an entity it already owns as authoritative. The target at systems.rs:136-145 handles this:
```rust
if world.get::<Authoritative>(bevy_entity).is_some() {
    tracing::warn!("Ignoring duplicate transfer for {} from {}", eid, transfer.from_server);
    // Still send ACK for duplicate -- source needs it even if we already have the entity
    send_transfer_ack(world, eid, current_tick, transfer.from_server);
    continue;
}
```

This is correct -- the target re-sends the ACK. But if this second ACK is also lost (double packet loss), the source aborts and the entity is duplicated.

## Impact
- **Probability**: On reliable networks (localhost), effectively zero. On WiFi between UNO Q boards, UDP packet loss rates of 1-5% are normal. The probability of losing both the ACK and the retry ACK is ~0.01-0.25%. Over thousands of transfers, this will happen.
- **Permanent duplication**: The entity exists on both servers as authoritative. Both simulate it, replicate it, and broadcast it. Peer servers see conflicting updates. The entity appears on two servers' entity counts. The coordinator's rebalancing sees the wrong entity distribution.
- **No self-healing**: There is no mechanism to detect or resolve the duplication afterward. The entity remains duplicated until one of the servers is restarted.

## Recommended Fix

**Option A (Simple):** Increase TRANSFER_MAX_RETRIES to 3 or 5. This reduces the probability of total ACK loss from ~0.01-0.25% to ~0.0001-0.00003%. Simple and effective for the embedded use case.

**Option B (Reliable ACK):** The target server continues sending TransferAck every tick for N ticks (e.g., 5) after accepting a transfer. This is a burst of ACKs rather than relying on a single packet. The source deduplicates (already handled -- process_transfer_acks ignores ACKs for entities without TransferOutbound).

**Option C (Coordinator-mediated):** After the two-phase transfer, both servers inform the coordinator of the authority change. The coordinator detects conflicts (entity claimed by two servers) and issues a resolution directive. This is the most robust but highest effort.

**Interim mitigation:** After the abort path (systems.rs:332-350), have the source check if it still receives shadow updates for the entity from the target server. If it does, the transfer actually succeeded despite the lost ACK, and the source can despawn its copy.

## Effort: small (Option A or B), large (Option C)

## Related Files
- crates/sim-server/src/systems.rs:35-61 (send_transfer_ack, one-shot)
- crates/sim-server/src/systems.rs:287-351 (process_transfer_timeouts, retry and abort)
- crates/sim-server/src/systems.rs:136-145 (duplicate transfer handling on target)
- crates/sim-server/src/systems.rs:24-27 (TRANSFER_TIMEOUT_TICKS=10, TRANSFER_MAX_RETRIES=1)
