# Heartbeat Entity Count Includes Frozen TransferOutbound Entities

## Severity: medium

## Category: operational

## Summary
The game loop's entity count for heartbeats (game_loop.rs:147-150) queries `With<Authoritative>` without excluding `TransferOutbound`. Entities mid-transfer are counted as "owned" by the source server, even though the source no longer simulates or replicates them. The target server also counts them (as newly authoritative). The coordinator sees an inflated total entity count and a skewed distribution, which distorts rebalance decisions. During bulk transfers or drain, this can cause the coordinator to trigger unnecessary or incorrect rebalance directives.

## Evidence

The heartbeat count in game_loop.rs:147-150:
```rust
let auth_count = world
    .query_filtered::<(), With<Authoritative>>()
    .iter(&world)
    .count();
```

This counts all entities with `Authoritative`, including those with `TransferOutbound`. The count is stored in `entity_count_atomic` (line 157) and sent to the coordinator via heartbeat.

The coordinator's rebalance logic in registry.rs:120-166 uses `entity_count` to determine overloaded/underloaded servers:
```rust
let overloaded = sim_servers.iter().max_by_key(|s| s.entity_count).unwrap();
let underloaded = sim_servers.iter().min_by_key(|s| s.entity_count).unwrap();
```

**Scenario**: Server 1 has 100 entities and transfers 10 to Server 2. During the ACK window:
- Server 1 reports 100 (still counts the 10 frozen entities)
- Server 2 reports 10 (accepted the 10)
- Coordinator sees total = 110, but real total is 100
- Coordinator sees Server 1 at 100 and Server 2 at 10, believes the imbalance is worse than it is, and may issue another rebalance directive

## Impact
- **Rebalance oscillation**: The coordinator sees the source as more loaded than it actually is (frozen entities do not consume tick budget since they are excluded from simulation). It may issue a second rebalance directive while the first is still completing, sending more entities than intended.
- **Double-counting duration**: The window where both servers count the same entity is typically 2-4 ticks (100-200ms) per transfer. During a bulk drain of 100+ entities, the double-counting is sustained for seconds.
- **Dashboard inaccuracy**: The coordinator dashboard shows inflated entity totals during transfers.

## Recommended Fix

Exclude `TransferOutbound` entities from the heartbeat count:
```rust
let auth_count = world
    .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
    .iter(&world)
    .count();
```

This reflects the actual "active" entity count that consumes tick budget on this server.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:147-150 (auth_count query, no TransferOutbound filter)
- crates/sim-server/src/game_loop.rs:157 (entity_count_atomic used for heartbeat)
- crates/coordinator/src/registry.rs:120-166 (rebalance logic using entity_count)
