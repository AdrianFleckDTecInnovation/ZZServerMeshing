# Mesh Auditor Findings Index

Audit of the Bevy ECS migration (sim-server HashMap -> bevy_ecs 0.18).
Audited: 2026-04-12

## Summary

17 findings across 7 categories. 1 critical, 4 high, 8 medium, 2 low.

The most urgent issue is fire-and-forget authority transfers (#002), which can permanently destroy entities on UDP packet loss. The async-to-ECS bridge (#001, #003) and the despawn-during-iteration pattern (#007) are high-severity issues that should be addressed before production deployment.

## Findings by Severity

| # | Severity | Category | Title | Effort |
|---|----------|----------|-------|--------|
| [002](002-authority-transfer-fire-and-forget.md) | critical | data-loss | Authority transfer is fire-and-forget over UDP | medium |
| [001](001-async-bridge-data-loss-on-overwrite.md) | high | data-loss | Async-bridge swap overwrites unconsumed inbound data | small |
| [003](003-tokio-mutex-awaited-in-tick-loop.md) | high | bottleneck | Tokio mutex awaited inside the 50ms tick loop | small |
| [004](004-entity-lookup-desync-risk.md) | high | race-condition | EntityLookup can desync from ECS world | small |
| [007](007-drain-despawns-during-iteration.md) | high | race-condition | Drain system despawns entities while iterating over handles | small |
| [013](013-udp-send-blocks-game-loop.md) | high | bottleneck | Async UDP sends block the game loop tick | medium |
| [005](005-client-snapshot-double-iteration.md) | medium | bottleneck | Client snapshot system iterates queries three times | small |
| [006](006-replication-clones-entire-entity-vec.md) | medium | bottleneck | Replication clones entire entity Vec every tick | small |
| [008](008-outbound-vecs-never-shrink.md) | medium | scalability | Outbound Vec buffers grow but never shrink | small |
| [009](009-process-exit-in-game-loop.md) | medium | operational | Game loop calls std::process::exit during drain | small |
| [010](010-exclusive-system-access-prevents-parallelism.md) | medium | bottleneck | Exclusive world access prevents ECS system parallelism | large |
| [011](011-no-client-input-rate-limiting.md) | medium | operational | No rate limiting on client input | small |
| [012](012-rebalance-directive-last-writer-wins.md) | medium | data-loss | Multiple rebalance directives overwrite -- only last survives | small |
| [015](015-outbound-clears-missing-from-tick-loop.md) | medium | race-condition | Outbound resources not cleared between ticks | small |
| [016](016-bincode-serialization-cost-at-scale.md) | medium | scalability | Bincode serialization cost scales linearly per-peer per-tick | medium |
| [014](014-tick-counter-u64-not-synchronized.md) | low | protocol | Tick counter u64 not synchronized across servers | small |
| [017](017-behavioral-divergence-rebalance-ignores-entity-ids.md) | low | protocol | Rebalance directive entity_ids field ignored | small |

## Findings by Category

### Data Loss (3)
- **#002** (critical) Authority transfer fire-and-forget -- entities permanently lost on UDP drop
- **#001** (high) Async bridge overwrites unconsumed data
- **#012** (medium) Rebalance directives overwrite each other

### Bottleneck (5)
- **#003** (high) Tokio mutex in tick loop
- **#013** (high) UDP sends block game loop
- **#005** (medium) Triple query iteration in client snapshot
- **#006** (medium) Unnecessary Vec clone in replication
- **#010** (medium) Exclusive systems prevent parallelism

### Race Condition (3)
- **#004** (high) EntityLookup desync risk
- **#007** (high) Despawn during iteration
- **#015** (medium) Outbound resources not cleared between ticks

### Scalability (2)
- **#008** (medium) Vec buffers never shrink
- **#016** (medium) Bincode serialization cost at scale (replication stops at ~1200 entities)

### Operational (2)
- **#009** (medium) process::exit prevents graceful shutdown
- **#011** (medium) No client input rate limiting

### Protocol (2)
- **#014** (low) Tick counter drift across servers
- **#017** (low) entity_ids field ignored in rebalance

## Recommended Fix Order

1. **#002** -- Authority transfer ack protocol (prevents permanent entity loss)
2. **#001** -- Change swap to extend (one-line fix, prevents data loss)
3. **#003** -- Replace tokio::sync::Mutex with std::sync::Mutex (reduces lock contention)
4. **#007** -- Separate collect/despawn phases (prevents potential panics)
5. **#011** -- Add client input rate limiting (prevents abuse)
6. **#006** -- Remove unnecessary clone (one-line fix)
7. **#005** -- Remove double iteration and HashSet (simple refactor)
8. **#009** -- Replace process::exit with break (simple fix)
9. **#013** -- Move UDP sends to separate task (medium effort, significant benefit)
10. Everything else (low urgency)
