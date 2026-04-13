# Mesh Auditor Findings Index

## Audit 1: Bevy ECS Migration
Audited: 2026-04-12

17 findings across 7 categories. 1 critical, 4 high, 8 medium, 2 low.

## Audit 2: Delta Compression Feature
Audited: 2026-04-13

10 new findings (018-027). 2 high, 6 medium, 2 low.

The most urgent delta compression issue is baseline drift on dropped frames (#018), which causes entities to become invisible to clients for up to 5 seconds. The ghost entity problem (#023) is a direct consequence. The unused ACK infrastructure (#021) is the path to a proper fix.

---

## All Findings by Severity

| # | Severity | Category | Title | Effort |
|---|----------|----------|-------|--------|
| [002](002-authority-transfer-fire-and-forget.md) | critical | data-loss | Authority transfer is fire-and-forget over UDP | medium |
| [001](001-async-bridge-data-loss-on-overwrite.md) | high | data-loss | Async-bridge swap overwrites unconsumed inbound data | small |
| [003](003-tokio-mutex-awaited-in-tick-loop.md) | high | bottleneck | Tokio mutex awaited inside the 50ms tick loop | small |
| [004](004-entity-lookup-desync-risk.md) | high | race-condition | EntityLookup can desync from ECS world | small |
| [007](007-drain-despawns-during-iteration.md) | high | race-condition | Drain system despawns entities while iterating over handles | small |
| [013](013-udp-send-blocks-game-loop.md) | high | bottleneck | Async UDP sends block the game loop tick | medium |
| [018](018-baseline-drift-on-dropped-frames.md) | high | data-loss | Server baseline advances even when client frame is dropped | small |
| [023](023-client-delta-removal-vs-snapshot-entity-set-race.md) | high | data-loss | Client delta path does not remove ghost entities from baseline drift | small |
| [005](005-client-snapshot-double-iteration.md) | medium | bottleneck | Client snapshot system iterates queries three times | small |
| [006](006-replication-clones-entire-entity-vec.md) | medium | bottleneck | Replication clones entire entity Vec every tick | small |
| [008](008-outbound-vecs-never-shrink.md) | medium | scalability | Outbound Vec buffers grow but never shrink | small |
| [009](009-process-exit-in-game-loop.md) | medium | operational | Game loop calls std::process::exit during drain | small |
| [010](010-exclusive-system-access-prevents-parallelism.md) | medium | bottleneck | Exclusive world access prevents ECS system parallelism | large |
| [011](011-no-client-input-rate-limiting.md) | medium | operational | No rate limiting on client input | small |
| [012](012-rebalance-directive-last-writer-wins.md) | medium | data-loss | Multiple rebalance directives overwrite -- only last survives | small |
| [015](015-outbound-clears-missing-from-tick-loop.md) | medium | race-condition | Outbound resources not cleared between ticks | small |
| [016](016-bincode-serialization-cost-at-scale.md) | medium | scalability | Bincode serialization cost scales linearly per-peer per-tick | medium |
| [019](019-per-client-memory-unbounded-baselines.md) | medium | scalability | Per-client baseline HashMaps grow unbounded with entity count | small |
| [020](020-delta-computation-cost-per-client.md) | medium | bottleneck | Delta computation runs O(N*E) per tick across all clients | medium |
| [021](021-ack-not-used-for-baseline-confirmation.md) | medium | protocol | Client tick ACKs are stored but never consulted | medium |
| [022](022-change-threshold-suppresses-small-movements.md) | medium | protocol | 0.01 threshold uses per-component comparison, may suppress diagonal motion | small |
| [024](024-full-snapshot-clone-per-client.md) | medium | bottleneck | Full snapshot binary frame cloned N times for N clients | small |
| [026](026-registration-snapshot-race.md) | medium | race-condition | Client registration and first snapshot race across tick boundary | medium |
| [027](027-npc-simulation-changes-defeat-delta.md) | medium | bottleneck | NPC movement every tick defeats delta compression savings | small |
| [014](014-tick-counter-u64-not-synchronized.md) | low | protocol | Tick counter u64 not synchronized across servers | small |
| [017](017-behavioral-divergence-rebalance-ignores-entity-ids.md) | low | protocol | Rebalance directive entity_ids field ignored | small |
| [025](025-ack-parsing-no-validation.md) | low | protocol | Binary ACK parsing accepts any tick value without validation | small |

## Delta Compression Findings Detail

### Data Loss (2)
- **#018** (high) Baseline drift on dropped frames -- server thinks client has entities it never received
- **#023** (high) Ghost entities on client from baseline/removal mismatch (consequence of #018)

### Bottleneck (3)
- **#020** (medium) O(N*E) delta computation duplicated across clients with identical views
- **#024** (medium) Full snapshot Vec<u8> cloned per client instead of shared
- **#027** (medium) All NPCs move every tick, making every delta as large as a full snapshot

### Scalability (1)
- **#019** (medium) Per-client baseline HashMaps: O(N*E) memory, never shrink

### Protocol (3)
- **#021** (medium) ACK infrastructure built but unused -- baseline advances optimistically
- **#022** (medium) Change threshold per-component instead of per-vector
- **#025** (low) ACK parsing has no tick validation

### Race Condition (1)
- **#026** (medium) Client sees PlayerAssigned before player entity exists in first snapshot

## Recommended Fix Order (Delta Compression)

1. **#018** -- Move baseline update to after successful try_send (prevents data loss, small effort)
2. **#023** -- Client-side staleness check as defense-in-depth (small effort, pairs with #018)
3. **#024** -- Use Arc<Vec<u8>> for shared full snapshot frames (small effort, reduces allocations)
4. **#021** -- Wire ACKs into baseline management OR remove dead code (medium effort, completes the protocol)
5. **#020** -- Share delta computation across clients with identical views (medium effort)
6. **#027** -- Add heuristic to skip delta when >80% entities changed (small effort)
7. **#022** -- Switch to vector distance comparison (small effort)
8. **#019** -- Reuse baseline HashMap on full snapshot (small effort)
9. **#025** -- Add ACK tick validation (small effort, only matters after #021)
10. **#026** -- Accept one-tick gap or defer PlayerAssigned (medium effort, low priority)
