# TransferOutbound Entities Invisible to Source Server's Clients

## Severity: medium

## Category: protocol

## Summary
The `Without<TransferOutbound>` filter on `prepare_client_snapshot`'s `auth_query` (systems.rs:1069) correctly prevents the source from replicating frozen entities to peers, but it also makes the entity immediately disappear from all clients connected to the source server. The target server only starts broadcasting the entity after it accepts the transfer and produces its next client snapshot (1-2 ticks later). During this gap, the entity is invisible to all clients everywhere -- neither the source nor the target broadcasts it. For player entities, this is a visible freeze/teleport.

## Evidence

The `prepare_client_snapshot` auth_query at systems.rs:1060-1070:
```rust
auth_query: Query<
    (...),
    (With<Authoritative>, Without<TransferOutbound>),
>,
```

The shadow_query does NOT have a TransferOutbound filter (line 1071-1081), but the entity has `Authoritative`, not `Shadow`, so it will not appear in the shadow_query either.

**Timeline**:
1. Tick N: Source adds `TransferOutbound`, sends UDP transfer. Entity disappears from source's client snapshot.
2. Tick N+0.5 (UDP transit): Transfer in flight.
3. Tick N+1: Target receives transfer in `drain_inbound_events`, promotes to `Authoritative`, sends ACK. Target's `prepare_client_snapshot` runs later in the same tick -- the entity appears in the target's client snapshot for the first time.
4. Tick N+1 or N+2: Source receives ACK, despawns.

**Minimum gap**: 1 tick (50ms) where the entity is in neither server's client snapshot. On embedded hardware with WiFi latency, this extends to 2-3 ticks (100-150ms).

**Client perception**: For clients connected to the source server, the entity disappears at tick N and never reappears (the source no longer has it). For clients connected to the target, the entity appears at tick N+1 with fresh state. For clients connected to a third server, they see the entity via shadow data from the source (which stops at tick N) and then from the target (which starts at tick N+1 or later).

## Impact
- **Visual pop**: Entities vanish for 50-150ms from the source server's client view. For NPCs, this is a minor visual glitch. For a player entity during drain, the player's own character disappears briefly.
- **Prediction disruption**: If the transferred entity is the client's own player, the prediction buffer has no server state to reconcile against for 1-3 ticks, potentially causing a visible snap when the target's first frame arrives.
- **Delta compression interaction**: The entity disappears from the source's client snapshot, causing a removal in the delta frame. When the entity reappears from the target as a shadow, it appears as a new entity in the next delta frame. The entity effectively "respawns" from the client's perspective.

## Recommended Fix

**Option A (Simple)**: Keep TransferOutbound entities in `prepare_client_snapshot` but mark them with their current `authority_server` already set to the target. This way the source's clients see the entity in its frozen position with the new authority server color, and the target's clients see it once the target starts broadcasting. The 1-tick gap where only the source broadcasts is filled.

```rust
// In prepare_client_snapshot, add a separate query for TransferOutbound entities
transfer_query: Query<
    (&NetworkId, &Position, &Velocity, &EntityKind, &TransferOutbound, Option<&LastInputSeq>),
    With<Authoritative>,
>,
```

Then include them in the entity list with `authority_server` set to `ServerId(outbound.target_server)`.

**Option B (Accept the gap)**: Document that a 1-tick visual gap is expected during authority transfers. This is the simplest approach and may be acceptable for NPCs. For players, Option A is better.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1056-1098 (prepare_client_snapshot, auth_query excludes TransferOutbound)
- crates/sim-server/src/systems.rs:1071-1081 (shadow_query, does not include TransferOutbound entities)
- crates/sim-server/src/systems.rs:120-193 (drain_inbound_events, target accepts transfer and becomes authoritative)
