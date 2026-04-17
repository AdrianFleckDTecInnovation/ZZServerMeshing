# TransferOutbound Entities Replicated to Peers Despite Being Frozen

## Severity: low

## Category: bottleneck

## Summary

The `prepare_replication` auth_query at `systems.rs:1118` includes entities with `TransferOutbound` component (no `Without<TransferOutbound>` filter). These frozen entities are replicated to all peers at the normal tiered rate (hot at 20Hz, cold at 5Hz) even though they are not being simulated and their state never changes. This wastes bandwidth on duplicate data -- the target server already has the entity from the transfer message, and other peers get identical state every tick.

## Evidence

The auth_query in `prepare_replication` at `systems.rs:1118-1128`:
```rust
auth_query: Query<
    (
        &NetworkId,
        &Position,
        &Velocity,
        &EntityKind,
        &AuthorityServer,
        Option<&LastInputSeq>,
    ),
    With<Authoritative>,
>,
```

There is no `Without<TransferOutbound>` filter. The system comment at lines 1103-1107 explicitly documents this:
```
/// Includes TransferOutbound entities so peers maintain up-to-date shadows for crash
/// recovery. The target server will have both its own authoritative copy AND a shadow
/// from the source -- that's fine, drain_inbound_events skips peer updates for entities
/// it already owns.
```

This design choice was intentional for crash recovery: if the transfer fails, other peers still have a recent shadow. However, the entity is frozen (fix #028 removes it from simulation), so the "up-to-date" shadow is identical every tick. Sending the same frozen state at 20Hz (hot) provides no more crash recovery value than sending it once.

## Impact

Minimal in practice. Transfer windows are short (500ms timeout, 3 retries = 2 seconds max). During that window, one frozen entity is replicated at the normal rate instead of being skipped. With typical transfer rates of 1-5 entities at a time, the wasted bandwidth is ~50 bytes * 20Hz * 2 seconds = ~2KB per transfer.

This only matters at scale with mass transfers (e.g., drain of 500 entities). During a drain, all 500 frozen entities are replicated to all peers at 20Hz for up to 2 seconds each, adding ~500KB of unnecessary traffic.

## Recommended Fix

No immediate fix needed due to low impact. If drain bandwidth becomes a concern, add `Without<TransferOutbound>` to the auth_query in `prepare_replication`, accepting the slightly reduced crash recovery coverage during the transfer window (the target already has the entity, and the source will retry if the transfer fails).

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1103-1107 (design comment)
- crates/sim-server/src/systems.rs:1118-1128 (auth_query without TransferOutbound filter)
- crates/sim-server/src/components.rs (TransferOutbound component)
