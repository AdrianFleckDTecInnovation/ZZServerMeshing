# Mesh Auditor Findings Index

## Audit 1: Bevy ECS Migration
Audited: 2026-04-12

17 findings across 7 categories. 1 critical, 4 high, 8 medium, 2 low.

## Audit 2: Delta Compression Feature
Audited: 2026-04-13

10 new findings (018-027). 2 high, 6 medium, 2 low.

The most urgent delta compression issue is baseline drift on dropped frames (#018), which causes entities to become invisible to clients for up to 5 seconds. The ghost entity problem (#023) is a direct consequence. The unused ACK infrastructure (#021) is the path to a proper fix.

## Audit 3: Two-Phase Transfer, UDP Chunking, Player Transfer Protocol
Audited: 2026-04-13

12 new findings (028-039). 4 high, 5 medium, 3 low.

The two-phase authority transfer resolves the critical fire-and-forget data loss from #002, but introduces new race conditions. The most urgent issue is dual authority during the transfer window (#028): both the source and target servers simulate, replicate, and accept inputs for the same entity during the ACK roundtrip. Combined with stale retry state (#030) and player input loss (#031), the transfer window is the highest-risk area for player-visible glitches.

The UDP chunking works correctly for the current entity size but has a memory leak risk (#029) from incomplete reassembly entries that are never cleaned when a peer crashes.

## Audit 4: Re-Audit After Fixes for #028, #029, #030, #031
Audited: 2026-04-13

**Fixes verified**: All four high-severity findings (#028, #029, #030, #031) have been correctly fixed. The implementations match the recommended approaches and resolve the originally identified issues.

**Resolution status of fixed findings**:
- **#028 (Dual Authority)**: RESOLVED. `Without<TransferOutbound>` correctly applied to `npc_behavior`, `physics_integration`, `clamp_positions`, `prepare_replication`, `prepare_client_snapshot` auth_query. Entity is frozen on source.
- **#029 (Chunk Reassembly Leak)**: RESOLVED. Global sweep every 100 iterations evicts entries >40 ticks behind global max. Crashed peer entries are now cleaned up.
- **#030 (Stale Retry State)**: RESOLVED. `process_transfer_timeouts` re-serializes from current ECS components. `serialized_msg` field removed from `TransferOutbound`.
- **#031 (Input Loss During Transfer)**: RESOLVED. Both `process_client_inputs` and `drain_inbound_events` now forward inputs for TransferOutbound entities to `target_addr`.

**Previously resolved findings now confirmed**:
- **#035 (serialized_msg memory overhead)**: RESOLVED by #030 fix -- `serialized_msg` removed from `TransferOutbound`.

**5 new findings (040-044)**: 1 high, 2 medium, 2 low. These are issues introduced or revealed by the fixes.

**Previously medium findings re-assessed**:
- **#033 (TransferAck fire-and-forget)**: Severity ELEVATED to high. See assessment below.
- **#039 (Drain retransfer loop)**: Remains medium. Unchanged by fixes.
- **#032 (Chunk total_chunks validation)**: Remains medium. Unchanged by fixes.

---

## All Findings by Severity

| # | Severity | Category | Title | Status | Effort |
|---|----------|----------|-------|--------|--------|
| [002](002-authority-transfer-fire-and-forget.md) | critical | data-loss | Authority transfer is fire-and-forget over UDP | resolved by two-phase (#028-#033) | medium |
| [001](001-async-bridge-data-loss-on-overwrite.md) | high | data-loss | Async-bridge swap overwrites unconsumed inbound data | open | small |
| [003](003-tokio-mutex-awaited-in-tick-loop.md) | high | bottleneck | Tokio mutex awaited inside the 50ms tick loop | open | small |
| [004](004-entity-lookup-desync-risk.md) | high | race-condition | EntityLookup can desync from ECS world | open | small |
| [007](007-drain-despawns-during-iteration.md) | high | race-condition | Drain system despawns entities while iterating over handles | open | small |
| [013](013-udp-send-blocks-game-loop.md) | high | bottleneck | Async UDP sends block the game loop tick | open | medium |
| [018](018-baseline-drift-on-dropped-frames.md) | high | data-loss | Server baseline advances even when client frame is dropped | open | small |
| [023](023-client-delta-removal-vs-snapshot-entity-set-race.md) | high | data-loss | Client delta path does not remove ghost entities from baseline drift | open | small |
| [028](028-dual-authority-during-transfer-window.md) | high | race-condition | Both servers simulate and replicate entity during two-phase ACK window | **resolved** | small |
| [029](029-chunk-reassembly-memory-leak.md) | high | scalability | Chunk reassembly entries leak memory when peer crashes or chunks are lost | **resolved** | small |
| [030](030-transfer-retry-uses-stale-state.md) | high | data-loss | Transfer retry re-sends stale entity state from initiation time | **resolved** | small |
| [031](031-input-loss-during-player-transfer.md) | high | data-loss | Player inputs accepted but lost during two-phase transfer ACK window | **resolved** | small |
| [033](033-transfer-ack-not-retransmitted.md) | **high** | data-loss | Transfer ACK is single UDP packet, loss causes permanent entity duplication | open (elevated from medium) | small |
| [040](040-remove-entity-during-transfer-despawns-outbound.md) | **high** | data-loss | RemoveEntity during transfer despawns outbound entity, orphaning it on target | **new** | small |
| [005](005-client-snapshot-double-iteration.md) | medium | bottleneck | Client snapshot system iterates queries three times | open | small |
| [006](006-replication-clones-entire-entity-vec.md) | medium | bottleneck | Replication clones entire entity Vec every tick | open | small |
| [008](008-outbound-vecs-never-shrink.md) | medium | scalability | Outbound Vec buffers grow but never shrink | open | small |
| [009](009-process-exit-in-game-loop.md) | medium | operational | Game loop calls std::process::exit during drain | open | small |
| [010](010-exclusive-system-access-prevents-parallelism.md) | medium | bottleneck | Exclusive world access prevents ECS system parallelism | open | large |
| [011](011-no-client-input-rate-limiting.md) | medium | operational | No rate limiting on client input | open | small |
| [012](012-rebalance-directive-last-writer-wins.md) | medium | data-loss | Multiple rebalance directives overwrite -- only last survives | open | small |
| [015](015-outbound-clears-missing-from-tick-loop.md) | medium | race-condition | Outbound resources not cleared between ticks | open | small |
| [016](016-bincode-serialization-cost-at-scale.md) | medium | scalability | Bincode serialization cost scales linearly per-peer per-tick | open | medium |
| [019](019-per-client-memory-unbounded-baselines.md) | medium | scalability | Per-client baseline HashMaps grow unbounded with entity count | open | small |
| [020](020-delta-computation-cost-per-client.md) | medium | bottleneck | Delta computation runs O(N*E) per tick across all clients | open | medium |
| [021](021-ack-not-used-for-baseline-confirmation.md) | medium | protocol | Client tick ACKs are stored but never consulted | open | medium |
| [022](022-change-threshold-suppresses-small-movements.md) | medium | protocol | 0.01 threshold uses per-component comparison, may suppress diagonal motion | open | small |
| [024](024-full-snapshot-clone-per-client.md) | medium | bottleneck | Full snapshot binary frame cloned N times for N clients | open | small |
| [026](026-registration-snapshot-race.md) | medium | race-condition | Client registration and first snapshot race across tick boundary | open | medium |
| [027](027-npc-simulation-changes-defeat-delta.md) | medium | bottleneck | NPC movement every tick defeats delta compression savings | open | small |
| [032](032-chunk-total-mismatch-no-validation.md) | medium | protocol | Chunk reassembly accepts mismatched total_chunks without validation | open | small |
| [035](035-serialized-msg-memory-overhead.md) | medium | scalability | TransferOutbound stores full serialized message per entity | **resolved** (by #030 fix) | small |
| [037](037-chunk-size-not-validated-against-udp-limit.md) | medium | scalability | ENTITIES_PER_CHUNK is a guess, not validated against UDP size limit | open | small |
| [038](038-player-transfer-protocol-unimplemented.md) | medium | protocol | PlayerTransfer protocol handlers are log-only stubs | open | small |
| [039](039-drain-retransfers-on-every-tick.md) | medium | bottleneck | Drain re-initiates transfers for aborted entities on every tick (infinite loop) | open | small |
| [041](041-heartbeat-counts-frozen-entities.md) | **medium** | operational | Heartbeat entity count includes frozen TransferOutbound entities | **new** | small |
| [042](042-transfer-outbound-entity-invisible-to-clients.md) | **medium** | protocol | TransferOutbound entities invisible to source server's clients for 1-3 ticks | **new** | small |
| [014](014-tick-counter-u64-not-synchronized.md) | low | protocol | Tick counter u64 not synchronized across servers | open | small |
| [017](017-behavioral-divergence-rebalance-ignores-entity-ids.md) | low | protocol | Rebalance directive entity_ids field ignored | open | small |
| [025](025-ack-parsing-no-validation.md) | low | protocol | Binary ACK parsing accepts any tick value without validation | open | small |
| [034](034-transfer-timeout-off-by-one.md) | low | protocol | Transfer timeout uses strict > causing 50ms off-by-one | open | small |
| [036](036-schedule-ordering-acks-before-transfers.md) | low | protocol | Schedule ordering verified correct, documents rationale | open | small |
| [043](043-global-sweep-stale-threshold-too-low.md) | **low** | scalability | Global reassembly sweep may evict entries from legitimately slow peers | **new** | small |
| [044](044-transfer-retry-component-access-after-query.md) | **low** | race-condition | Transfer retry uses unwrap on component access after query collection | **new** | small |

## Re-Audit Fix Verification Detail

### Fix #028: Dual Authority (RESOLVED)
The `Without<TransferOutbound>` filter is correctly applied to all five systems:
- `npc_behavior` (systems.rs:756)
- `physics_integration` (systems.rs:783)
- `clamp_positions` (systems.rs:798)
- `prepare_replication` (systems.rs:1030)
- `prepare_client_snapshot` auth_query (systems.rs:1069)

Entity is frozen on source: no simulation, no replication to peers, no inclusion in client snapshots (auth query). The shadow_query in `prepare_client_snapshot` does not pick it up either since it has `Authoritative`, not `Shadow`.

**New issue introduced**: #042 -- the entity vanishes from the source's client snapshot for 1-3 ticks before appearing in the target's client snapshot. This is an acceptable tradeoff vs dual authority, but worth documenting.

**New issue introduced**: #041 -- heartbeat count still includes frozen entities, distorting coordinator's rebalance decisions.

### Fix #031: Player Input Lost During Transfer (RESOLVED)
Both input paths now correctly forward to the transfer target:
- `process_client_inputs` MoveInput handler (systems.rs:514-535): checks `TransferOutbound`, forwards to `target_addr`
- `drain_inbound_events` forwarded input handler (systems.rs:200-221): checks `TransferOutbound`, re-forwards to `target_addr`

**Forward loop analysis**: No loop exists. The source forwards to the target. The target does not have `TransferOutbound` on the entity (it accepted the transfer and has it as plain `Authoritative`). So the target processes the input normally. The only risk would be if the entity were simultaneously being transferred FROM the target to a third server, but the coordinator does not issue overlapping transfers for the same entity.

**New issue introduced**: #040 -- `RemoveEntity` for a TransferOutbound entity despawns it without forwarding a remove to the target.

### Fix #030: Transfer Retry Uses Stale State (RESOLVED)
`process_transfer_timeouts` (systems.rs:337-378) now rebuilds the `AuthorityTransfer` message from current ECS components:
```rust
let nid = world.get::<NetworkId>(entity).unwrap();
let pos = world.get::<Position>(entity).unwrap();
// ... re-serializes fresh state
```

Since the entity is now frozen (fix #028), the "current" state is the same as the initial state -- the entity does not move during the timeout window. This means the re-serialization produces identical bytes to the original send, which is correct and consistent. The fix is still valuable for the case where the entity was NOT frozen (e.g., a regression in the #028 fix, or if the freeze behavior changes).

**`serialized_msg` field removed**: Confirmed. `TransferOutbound` in components.rs:44-49 has only 4 fields: `target_server`, `target_addr`, `sent_tick`, `retry_count`. This resolves finding #035 (serialized_msg memory overhead).

### Fix #029: Chunk Reassembly Memory Leak (RESOLVED)
The global sweep in receiver.rs:39-55 runs every `GLOBAL_SWEEP_INTERVAL = 100` loop iterations and evicts entries older than `GLOBAL_STALE_TICKS = 40` ticks behind the global max tick.

**Correctness**: The sweep correctly handles the crashed-peer scenario: if server 2 crashes, its entries stop advancing. The global max is driven by surviving servers. After 40 ticks (2 seconds at 20Hz), crashed server's entries are evicted.

**New issue identified**: #043 -- the global max comparison may evict entries from a legitimately slow peer (e.g., a server that just restarted at tick 0 while others are at tick 5000). This is a low-severity concern because non-chunked updates bypass reassembly entirely and only servers with 100+ entities use chunking.

## Severity Re-Assessment: #033 (Transfer ACK Fire-and-Forget)

**Previous severity**: medium.
**New severity**: high.

**Rationale**: With the #028 fix in place, the source server now freezes the entity during the transfer window. If the ACK is lost:
1. Source retries with fresh state (fix #030), but the entity is frozen, so the state is identical.
2. Target already has the entity. It sends a duplicate ACK (systems.rs:137-145).
3. If the duplicate ACK is also lost (double packet loss), the source aborts (removes `TransferOutbound`).
4. The entity resumes simulation on the source (unfrozen). The target also simulates it.
5. **Permanent entity duplication** -- the entity exists on both servers with no recovery mechanism.

The freeze fix makes the duplication MORE visible because the entity was invisible during the freeze and suddenly reappears on the source. Before the fix, both servers were simulating it simultaneously so the duplication was harder to notice but caused replication conflicts. Now, the entity clearly "comes back" on the source after being gone for 500ms+.

The fix did not change the probability of ACK loss, but the consequence is cleaner to reason about and more impactful: the entity goes from frozen-and-invisible to active-and-duplicated in one tick.

**Recommendation**: Increase `TRANSFER_MAX_RETRIES` from 1 to 3-5, or implement burst ACKs (target sends 3-5 ACK copies).

## Recommended Fix Order (New Findings from Audit 4)

1. **#040** (high) -- Add TransferOutbound check to RemoveEntity handler, forward remove to target (small effort)
2. **#033** (elevated to high) -- Increase TRANSFER_MAX_RETRIES or implement burst ACKs (small effort)
3. **#041** (medium) -- Exclude TransferOutbound from heartbeat entity count (small effort)
4. **#042** (medium) -- Include TransferOutbound entities in client snapshot with target authority (small effort)
5. **#043** (low) -- Use per-server latest ticks in global sweep instead of global max (small effort)
6. **#044** (low) -- Replace unwrap with checked access in retry path (small effort)

## Two-Phase Transfer Findings Detail

### Resolved (4)
- **#028** (high) Dual authority during transfer window -- RESOLVED by Without<TransferOutbound> filter
- **#029** (high) Chunk reassembly memory leak -- RESOLVED by global sweep
- **#030** (high) Transfer retry uses stale state -- RESOLVED by re-serialization from ECS
- **#031** (high) Player input lost during transfer -- RESOLVED by forwarding to target

### Open High (2)
- **#033** (elevated from medium) TransferAck fire-and-forget -- entity duplication on double packet loss
- **#040** (new) RemoveEntity during transfer despawns outbound entity, orphaning on target

### Open Medium (3)
- **#032** Chunk total_chunks validation missing
- **#039** Drain + timeout abort creates infinite retransfer loop
- **#041** (new) Heartbeat counts frozen entities, distorting rebalance

### Open Low (3)
- **#034** Transfer timeout off-by-one
- **#043** (new) Global sweep threshold may evict slow peers
- **#044** (new) Retry component access uses unwrap

### Resolved by Fixes (1)
- **#035** serialized_msg memory overhead -- RESOLVED (field removed by #030 fix)

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
