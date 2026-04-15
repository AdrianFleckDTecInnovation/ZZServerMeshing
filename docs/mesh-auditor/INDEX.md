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

## Audit 5: Data Integrity Fix Verification (#021, #022, #025, #027)
Audited: 2026-04-13

5 new findings (045-049). 1 medium, 4 low.

**Fixes verified**: All four data integrity fixes are correctly implemented.

- **#022 fix (vec3 distance threshold)**: CORRECT. `distance_squared()` is equivalent to per-component for single-axis movement and strictly more permissive for diagonal movement, which is the intended fix. Boundary behavior is identical: 0.01 single-axis movement triggers neither old nor new code. See #045.
- **#027 fix (delta fallback heuristic)**: CORRECT. Full snapshot sent when >80% entities changed. Baseline is correctly rebuilt via `build_baseline(&entities)` on success. `ticks_since_full` reset to 1. Next tick produces delta. Division-by-zero guarded for 0 entities. See #047.
- **#021 fix (stale-ACK detection)**: CORRECT. `ACK_STALE_TICKS = 40` (2 seconds). Reads `acked_tick` with Relaxed ordering. Benign race with concurrent ACK writes -- worst case is one extra full snapshot. Recovery tested and verified. See #046.
- **#025 fix (ACK tick validation)**: CORRECT. Monotonicity in `handle_client` prevents regression. Server tick clamp in `prepare_client_snapshot` prevents future-tick bypass. Neither layer can reject legitimate ACKs. See #048.

**One actionable finding**: #049 (medium) -- `compute_delta` runs before the fallback check and is discarded when >80% entities changed. Wastes O(E) per client per tick in the common all-NPCs-moving case.

## Audit 6: Tick Loop Health Fixes Verification (#003, #009, #013)
Audited: 2026-04-13

2 new findings (050-051). Both low severity.

**Fixes verified**: All three tick loop health fixes are correctly implemented.

- **#003 fix (std::sync::Mutex swap)**: CORRECT. All `tokio::sync::Mutex` replaced with `std::sync::Mutex` across 6 files. The `.lock().unwrap_or_else(|e| e.into_inner())` pattern correctly recovers from mutex poisoning. No lock is held across an `.await` point -- each lock acquisition is followed by synchronous `push()` or `mem::take()` operations and the guard is dropped before the next `.await`. With the multi-threaded tokio runtime (`#[tokio::main]`), a `std::sync::Mutex` blocked thread cannot deadlock against itself because receiver and game loop tasks run on different worker threads. Critical sections are sub-microsecond (pointer swaps only). This is the correct mutex type for this pattern.
- **#009 fix (break instead of process::exit)**: CORRECT. `ShutdownRequested` check at `game_loop.rs:377` uses `break` to exit the tick loop. The function returns normally, the `tokio::spawn` JoinHandle resolves, and `tokio::select!` in `main.rs:181` catches the completion. All destructors run. The tokio runtime shuts down gracefully. The one caveat is that other spawned tasks (sender, receiver, WS server) are not explicitly joined -- they are cancelled by runtime shutdown. See #050.
- **#013 fix (UDP sender task)**: CORRECT. Two separate `tokio::sync::mpsc` channels: replication (capacity 256, best-effort drops) and control (capacity 64, latency-critical). `run_udp_sender` uses `biased` select to drain control messages with priority over replication. The game loop uses `try_send` (non-blocking) for both channels, never blocking the tick loop. Separate drop counters log warnings every 5 seconds. The `biased` select does NOT starve replication under normal conditions because control messages are low-frequency (authority transfers and forwarded inputs happen infrequently relative to per-tick replication). Even under burst conditions (e.g., 50 transfers in one tick), the control channel holds all of them as a single `Batch` message, consuming one slot. The `UdpSocket` is safely shared between the sender task and receiver task via `Arc` -- tokio's `UdpSocket` supports concurrent `send_to` and `recv_from` from different tasks.

**Heartbeat fix verified**: The entity count for heartbeats (`game_loop.rs:238-240`) now uses `Without<TransferOutbound>`, resolving finding #041.

### Deadlock analysis for std::sync::Mutex in async context

The concern: if task A holds the `std::sync::Mutex` and gets descheduled by the tokio runtime, then task B (running on the same worker thread) tries to acquire the same mutex, the worker thread blocks and cannot make progress on either task.

Why this cannot happen in practice:
1. The `#[tokio::main]` macro creates a multi-threaded runtime (default: one worker per CPU core).
2. Each lock acquisition is immediately followed by a synchronous operation (`push`, `mem::take`) that completes in nanoseconds. The tokio runtime does not preempt synchronous code -- it only reschedules at `.await` points.
3. Since the lock is never held across an `.await`, the holder cannot be descheduled while holding the lock. The lock-hold duration equals the synchronous critical section duration.
4. Even on a single-threaded runtime, this would be safe because the lock is released synchronously before any yield point.

The one theoretical risk would be a panic inside the critical section, which poisons the mutex. The `unwrap_or_else(|e| e.into_inner())` pattern handles this by extracting the data from the poisoned mutex, allowing the system to continue operating.

---

## Audit 7: Area of Interest (AoI) Filtering v2
Audited: 2026-04-14

6 new findings (052-057). 1 high, 3 medium, 1 low. Plus verification of the 3 previous-attempt failure modes.

**Implementation verification**: The v2 AoI implementation correctly avoids all three failure modes from the previous attempt:
1. **No tiered filtering** -- CORRECT. The filter is binary in/out at 500 units. No variable update rates, no partial updates. The `in_aoi()` function returns a simple bool. No tiered logic exists anywhere in the codebase.
2. **prepare_client_snapshot is still a regular Bevy system** -- CORRECT. The system signature at `systems.rs:1107` uses standard `Res`, `ResMut`, and `Query` parameters. It is not an exclusive system. It does not take `&mut World`.
3. **No EntityLookup HashMap clone** -- CORRECT. Player positions are looked up by scanning the already-built `entities` Vec (O(C*E) worst case with C clients). The `EntityLookup` resource is not used by this system.

**One high-severity issue**: #052 -- the pre-loop `force_full` probe compares the full unfiltered entity list against AoI-filtered baselines, causing delta compression to be permanently disabled whenever a client's AoI covers less than 20% of total entities. This defeats the bandwidth savings that AoI is supposed to provide.

**AoI + delta interaction**: The delta compression interaction works correctly in principle (entities entering/exiting AoI produce clean add/remove deltas), but the force_full probe bug (#052) means deltas are never used in practice for AoI clients in a large world.

**Test coverage**: 7 new unit tests cover AoI filtering, boundary behavior, delta interaction (enter/leave/no-flicker), and forced full snapshot filtering. The tests are well-designed and pass. However, they use small entity counts (2-3 entities) where the force_full bug does not trigger because all entities are within or very close to the 80% threshold.

---

## Audit 8: Tiered AoI-Aware Shadow Replication (Design 010)
Audited: 2026-04-15

10 new findings (058-067). 2 high, 4 medium, 2 low, plus 2 verification-level findings.

**Implementation overview**: Peer replication is now split into hot (20Hz, within 750 units of peer Player shadows) and cold (5Hz, all other entities sent every 4th tick) tiers. Full snapshots are disabled by default (behind `--full-snapshot` flag). The tiered classification logic in `prepare_replication` is correct, well-tested (15+ unit tests), and the distance-check math is sound.

**Two high-severity issues**:

1. **#058 -- Backpressure skips cold ticks with no recovery path**: When UDP backpressure is active, ALL per-peer replication is blocked -- including cold ticks. With full snapshots disabled by default, there is no fallback to keep shadows alive during backpressure. After 15 seconds (SHADOW_STALE_TICKS=300), all shadows from the overloaded server are cleaned up on every peer. This can also create a self-reinforcing deadlock: no shadows means `prepare_replication` returns early, which means no replication, which means no shadows.

2. **#059 -- No-shadows early return prevents replication bootstrapping**: When a peer has zero shadow entities on this server (new peer, post-backpressure recovery, shadow cleanup), `prepare_replication` returns early with empty data. This creates a chicken-and-egg deadlock: Server A cannot replicate to Server B because it has no shadows from B, and Server B cannot replicate to A for the same reason. Currently masked by startup timing but becomes likely with simultaneous server restarts or post-backpressure recovery.

**These two findings interact**: #058 causes shadow loss under backpressure, and #059 prevents recovery after backpressure clears. Together they create a permanent replication blackout that requires server restarts to resolve.

**Stale comment found**: The backpressure comment at `game_loop.rs:417-418` says "rely on the 1/sec full snapshot for shadow recovery" -- but the 1/sec full snapshot no longer exists by default.

**Design doc error found**: #062 -- The design doc claims SHADOW_STALE_TICKS is 100 (5 seconds) but the code has 300 (15 seconds). The safety analysis in the design doc is based on incorrect values.

**Scalability assessment**: The O(E_auth * P_players_per_peer * N_peers) distance check (#060) is negligible at current scale (~0.1ms with 500 entities) but approaches tick budget concerns at 5000+ entities with 5+ players per peer. The cold-tick burst pattern (#066) is more immediately concerning: at 2500+ entities with 3 peers, every cold tick overflows the 256-slot replication channel, triggering the backpressure cascade from #058.

**Metrics correctness**: hot_count and cold_count are internally consistent and tested (#063). The semantics are: hot_count = entities within PEER_AOI_RADIUS, cold_count = entities outside that were included because cold tick. This is correct but may be slightly confusing on dashboards.

**Test coverage**: 15+ unit tests cover all major paths (hot-only ticks, cold ticks, full cycle, per-peer splits, edge cases including tick 0, no shadows, no players, PeerBounds fallback, boundary precision). The tests are thorough. One gap: no test for the backpressure interaction (#058) or the bootstrapping deadlock (#059), because those require the game_loop integration which unit tests do not exercise.

---

## All Findings by Severity

| # | Severity | Category | Title | Status | Effort |
|---|----------|----------|-------|--------|--------|
| [002](002-authority-transfer-fire-and-forget.md) | critical | data-loss | Authority transfer is fire-and-forget over UDP | resolved by two-phase (#028-#033) | medium |
| [001](001-async-bridge-data-loss-on-overwrite.md) | high | data-loss | Async-bridge swap overwrites unconsumed inbound data | open | small |
| [003](003-tokio-mutex-awaited-in-tick-loop.md) | high | bottleneck | Tokio mutex awaited inside the 50ms tick loop | **resolved** | small |
| [004](004-entity-lookup-desync-risk.md) | high | race-condition | EntityLookup can desync from ECS world | open | small |
| [007](007-drain-despawns-during-iteration.md) | high | race-condition | Drain system despawns entities while iterating over handles | open | small |
| [013](013-udp-send-blocks-game-loop.md) | high | bottleneck | Async UDP sends block the game loop tick | **resolved** | medium |
| [018](018-baseline-drift-on-dropped-frames.md) | high | data-loss | Server baseline advances even when client frame is dropped | **resolved** | small |
| [023](023-client-delta-removal-vs-snapshot-entity-set-race.md) | high | data-loss | Client delta path does not remove ghost entities from baseline drift | **resolved** | small |
| [028](028-dual-authority-during-transfer-window.md) | high | race-condition | Both servers simulate and replicate entity during two-phase ACK window | **resolved** | small |
| [029](029-chunk-reassembly-memory-leak.md) | high | scalability | Chunk reassembly entries leak memory when peer crashes or chunks are lost | **resolved** | small |
| [030](030-transfer-retry-uses-stale-state.md) | high | data-loss | Transfer retry re-sends stale entity state from initiation time | **resolved** | small |
| [031](031-input-loss-during-player-transfer.md) | high | data-loss | Player inputs accepted but lost during two-phase transfer ACK window | **resolved** | small |
| [033](033-transfer-ack-not-retransmitted.md) | **high** | data-loss | Transfer ACK is single UDP packet, loss causes permanent entity duplication | open (elevated from medium) | small |
| [040](040-remove-entity-during-transfer-despawns-outbound.md) | **high** | data-loss | RemoveEntity during transfer despawns outbound entity, orphaning it on target | open | small |
| [052](052-force-full-probe-uses-unfiltered-entities.md) | **high** | bottleneck | force_full probe compares unfiltered entities against AoI-filtered baselines, permanently disabling delta compression | open | small |
| [058](058-backpressure-skips-cold-ticks-no-recovery.md) | **high** | data-loss | Backpressure blocks all per-peer replication including cold ticks, no recovery path without full snapshots | **new** | small |
| [059](059-no-shadows-skips-replication-bootstrapping-failure.md) | **high** | data-loss | No-shadows early return prevents replication bootstrapping, creates permanent deadlock with #058 | **new** | small |
| [005](005-client-snapshot-double-iteration.md) | medium | bottleneck | Client snapshot system iterates queries three times | open | small |
| [006](006-replication-clones-entire-entity-vec.md) | medium | bottleneck | Replication clones entire entity Vec every tick | open | small |
| [008](008-outbound-vecs-never-shrink.md) | medium | scalability | Outbound Vec buffers grow but never shrink | open | small |
| [009](009-process-exit-in-game-loop.md) | medium | operational | Game loop calls std::process::exit during drain | **resolved** | small |
| [010](010-exclusive-system-access-prevents-parallelism.md) | medium | bottleneck | Exclusive world access prevents ECS system parallelism | open | large |
| [011](011-no-client-input-rate-limiting.md) | medium | operational | No rate limiting on client input | open | small |
| [012](012-rebalance-directive-last-writer-wins.md) | medium | data-loss | Multiple rebalance directives overwrite -- only last survives | open | small |
| [015](015-outbound-clears-missing-from-tick-loop.md) | medium | race-condition | Outbound resources not cleared between ticks | open | small |
| [016](016-bincode-serialization-cost-at-scale.md) | medium | scalability | Bincode serialization cost scales linearly per-peer per-tick | open | medium |
| [019](019-per-client-memory-unbounded-baselines.md) | medium | scalability | Per-client baseline HashMaps grow unbounded with entity count | open | small |
| [020](020-delta-computation-cost-per-client.md) | medium | bottleneck | Delta computation runs O(N*E) per tick across all clients | open | medium |
| [021](021-ack-not-used-for-baseline-confirmation.md) | medium | protocol | Client tick ACKs are stored but never consulted | **resolved** | medium |
| [022](022-change-threshold-suppresses-small-movements.md) | medium | protocol | 0.01 threshold uses per-component comparison, may suppress diagonal motion | **resolved** | small |
| [024](024-full-snapshot-clone-per-client.md) | medium | bottleneck | Full snapshot binary frame cloned N times for N clients | **resolved** | small |
| [026](026-registration-snapshot-race.md) | medium | race-condition | Client registration and first snapshot race across tick boundary | open | medium |
| [027](027-npc-simulation-changes-defeat-delta.md) | medium | bottleneck | NPC movement every tick defeats delta compression savings | **resolved** | small |
| [032](032-chunk-total-mismatch-no-validation.md) | medium | protocol | Chunk reassembly accepts mismatched total_chunks without validation | open | small |
| [035](035-serialized-msg-memory-overhead.md) | medium | scalability | TransferOutbound stores full serialized message per entity | **resolved** (by #030 fix) | small |
| [037](037-chunk-size-not-validated-against-udp-limit.md) | medium | scalability | ENTITIES_PER_CHUNK is a guess, not validated against UDP size limit | open | small |
| [038](038-player-transfer-protocol-unimplemented.md) | medium | protocol | PlayerTransfer protocol handlers are log-only stubs | open | small |
| [039](039-drain-retransfers-on-every-tick.md) | medium | bottleneck | Drain re-initiates transfers for aborted entities on every tick (infinite loop) | open | small |
| [041](041-heartbeat-counts-frozen-entities.md) | medium | operational | Heartbeat entity count includes frozen TransferOutbound entities | **resolved** | small |
| [042](042-transfer-outbound-entity-invisible-to-clients.md) | medium | protocol | TransferOutbound entities invisible to source server's clients for 1-3 ticks | open | small |
| [049](049-compute-delta-before-fallback-wastes-cpu.md) | medium | bottleneck | compute_delta runs before fallback check, wasted when >80% changed | **resolved** | small |
| [053](053-backward-compat-full-frame-wasted-every-tick.md) | medium | bottleneck | Unfiltered full snapshot encoded every tick for unused ClientSnapshot resource | open | small |
| [054](054-entities-clone-for-no-aoi-clients.md) | medium | bottleneck | Full entity Vec cloned every tick for clients without AoI position | open | small |
| [055](055-coordinator-read-timeout-120s-too-long.md) | medium | operational | 120s coordinator read timeout delays crash detection and reconnection | open | small |
| [056](056-npc-scatter-defeats-aoi-benefit.md) | medium | scalability | NPC spawn range scales with WORLD_EXTENT, scattering entities beyond AoI | open | small |
| [060](060-cold-tier-quadratic-per-entity-per-player.md) | **medium** | scalability | O(E_auth * P_players * N_peers) distance checks per tick, concerning at 5000+ entities | **new** | small |
| [061](061-cold-tick-entities-clone-per-peer.md) | **medium** | bottleneck | Cold tick clones full entity Vec per peer, ~288KB at 2000 entities with 3 peers | **new** | small |
| [062](062-design-doc-shadow-stale-ticks-mismatch.md) | **medium** | operational | Design doc claims SHADOW_STALE_TICKS=100 but code uses 300, safety analysis based on wrong values | **new** | small |
| [066](066-cold-tick-burst-amplifies-udp-chunking.md) | **medium** | bottleneck | Cold tick sends 4-10x more UDP chunks than hot ticks, overflows replication channel at 2500+ entities | **new** | medium |
| [014](014-tick-counter-u64-not-synchronized.md) | low | protocol | Tick counter u64 not synchronized across servers | open | small |
| [017](017-behavioral-divergence-rebalance-ignores-entity-ids.md) | low | protocol | Rebalance directive entity_ids field ignored | open | small |
| [025](025-ack-parsing-no-validation.md) | low | protocol | Binary ACK parsing accepts any tick value without validation | **resolved** | small |
| [034](034-transfer-timeout-off-by-one.md) | low | protocol | Transfer timeout uses strict > causing 50ms off-by-one | open | small |
| [036](036-schedule-ordering-acks-before-transfers.md) | low | protocol | Schedule ordering verified correct, documents rationale | open | small |
| [043](043-global-sweep-stale-threshold-too-low.md) | low | scalability | Global reassembly sweep may evict entries from legitimately slow peers | open | small |
| [044](044-transfer-retry-component-access-after-query.md) | low | race-condition | Transfer retry uses unwrap on component access after query collection | open | small |
| [045](045-vec3-threshold-more-permissive-on-single-axis.md) | low | protocol | vec3 distance_squared equivalent for axis-aligned, more permissive for diagonal | verification | n/a |
| [046](046-stale-ack-race-with-concurrent-ack.md) | low | race-condition | Stale-ACK check has benign race with concurrent client ACK writes | verification | n/a |
| [047](047-delta-fallback-baseline-correctly-rebuilt.md) | low | protocol | Delta fallback full snapshot correctly rebuilds baseline | verification | n/a |
| [048](048-ack-clamp-rejects-legitimate-reordered-acks.md) | low | protocol | ACK validation cannot reject legitimate ACKs; tick wraparound not a concern | verification | n/a |
| [050](050-sender-drain-cancelled-on-game-loop-exit.md) | low | data-loss | UDP sender task drain cancelled by runtime shutdown on game loop exit | open | small |
| [051](051-inbound-swap-holds-mutex-during-ecs-write.md) | low | bottleneck | Inbound swap holds shared mutex during ECS resource write (unnecessary) | open | small |
| [057](057-per-client-aoi-filter-clone-cost.md) | low | bottleneck | AoI filter clones EntityState for every in-range entity per client per tick | accepted tradeoff | medium |
| [063](063-metrics-hot-cold-double-counted-on-cold-ticks.md) | **low** | operational | hot_count metric semantics may confuse dashboard readers on cold ticks | **new** | small |
| [064](064-fallback-peerbounds-no-cold-tier.md) | **medium** | scalability | PeerBounds fallback sends at 20Hz with no cold-tier distinction | **new** | small |
| [065](065-shadow-positions-one-tick-stale-for-hot-classification.md) | **low** | race-condition | Shadow player positions 1 tick stale for hot classification, absorbed by 250-unit margin | **new** (accepted) | small |
| [067](067-transfer-outbound-entities-replicated-at-full-rate.md) | **low** | bottleneck | Frozen TransferOutbound entities replicated at full rate despite unchanged state | **new** | small |

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
3. **#041** (medium) -- Exclude TransferOutbound from heartbeat entity count (small effort) -- **RESOLVED** in #013 fix batch
4. **#042** (medium) -- Include TransferOutbound entities in client snapshot with target authority (small effort)
5. **#043** (low) -- Use per-server latest ticks in global sweep instead of global max (small effort)
6. **#044** (low) -- Replace unwrap with checked access in retry path (small effort)

## Recommended Fix Order (Tiered Replication)

1. **#058** (high, small effort) -- Cold ticks bypass backpressure gate. One-line change: `(!repl_backpressure || is_cold_tick)`. **Must fix to prevent shadow cascade under load.**
2. **#059** (high, small effort) -- Bootstrap replication to peers with no shadows. Send all entities to shadowless peers on cold ticks. Prevents permanent replication deadlock.
3. **#066** (medium, medium effort) -- Spread cold entities across phases to prevent burst overflow. Reduces peak channel utilization by 4x.
4. **#064** (medium, small effort) -- Apply cold-tier rate to PeerBounds fallback path (skip non-cold ticks for peers without players).
5. **#062** (medium, small effort) -- Fix design doc SHADOW_STALE_TICKS value and delta.rs comment.
6. **#061** (medium, small effort) -- Skip filter on cold ticks (all entities pass), avoid per-peer clone when possible.
7. **#060** (medium, small effort) -- Skip distance checks on cold ticks where all entities are included anyway.
8. **#067** (low, small effort) -- Consider excluding TransferOutbound from replication (optional, low impact).
9. **#065** (low, n/a) -- Document margin rationale in code comment (accepted design tradeoff).
10. **#063** (low, small effort) -- Add total_sent_count metric or document hot/cold semantics.

## Two-Phase Transfer Findings Detail

### Resolved (4)
- **#028** (high) Dual authority during transfer window -- RESOLVED by Without<TransferOutbound> filter
- **#029** (high) Chunk reassembly memory leak -- RESOLVED by global sweep
- **#030** (high) Transfer retry uses stale state -- RESOLVED by re-serialization from ECS
- **#031** (high) Player input lost during transfer -- RESOLVED by forwarding to target

### Open High (2)
- **#033** (elevated from medium) TransferAck fire-and-forget -- entity duplication on double packet loss
- **#040** (new) RemoveEntity during transfer despawns outbound entity, orphaning on target

### Open Medium (2)
- **#032** Chunk total_chunks validation missing
- **#039** Drain + timeout abort creates infinite retransfer loop

### Resolved Medium (1)
- **#041** Heartbeat counts frozen entities -- **RESOLVED** by Without<TransferOutbound> in game_loop.rs:238-240

### Open Low (3)
- **#034** Transfer timeout off-by-one
- **#043** (new) Global sweep threshold may evict slow peers
- **#044** (new) Retry component access uses unwrap

### Resolved by Fixes (1)
- **#035** serialized_msg memory overhead -- RESOLVED (field removed by #030 fix)

## Delta Compression Findings Detail

### Data Loss (2) -- BOTH RESOLVED
- **#018** (high) Baseline drift on dropped frames -- **RESOLVED**: baseline updated only on successful `try_send`
- **#023** (high) Ghost entities on client -- **RESOLVED**: server-side by #018 fix, client-side staleness sweep at 6s

### Bottleneck (3) -- 2 RESOLVED, 1 OPEN + 1 NEW
- **#020** (medium) O(N*E) delta computation duplicated across clients with identical views -- open
- **#024** (medium) Full snapshot cloned per client -- **RESOLVED**: `ClientFrame::Shared(Arc<Vec<u8>>)`
- **#027** (medium) NPC movement defeats delta -- **RESOLVED**: fallback to full snapshot when >80% changed
- **#049** (medium) compute_delta wasted before fallback check -- **resolved**: pre-loop `change_ratio` probe skips all per-client `compute_delta` when >80% changed

### Scalability (1)
- **#019** (medium) Per-client baseline HashMaps: O(N*E) memory, never shrink -- open

### Protocol (3) -- ALL RESOLVED
- **#021** (medium) ACK infrastructure unused -- **RESOLVED**: stale-ACK detection with `ACK_STALE_TICKS = 40`
- **#022** (medium) Change threshold per-component -- **RESOLVED**: `distance_squared()` comparison
- **#025** (low) ACK parsing no validation -- **RESOLVED**: monotonicity check + server tick clamp

### Race Condition (1)
- **#026** (medium) Client sees PlayerAssigned before player entity exists in first snapshot -- open

## Tick Loop Health Findings Detail

### Resolved (3)
- **#003** (high) Tokio mutex in tick loop -- **RESOLVED**: replaced with std::sync::Mutex, no deadlock risk
- **#009** (medium) process::exit in game loop -- **RESOLVED**: replaced with `break`, graceful return
- **#013** (high) UDP send blocks game loop -- **RESOLVED**: dedicated sender task with dual channels

### New (2)
- **#050** (low) Sender task drain cancelled on runtime shutdown -- edge case, no impact on coordinator-initiated drain
- **#051** (low) Inbound swap holds mutex during ECS write -- sub-microsecond waste, cleanup only

## AoI Filtering Findings Detail

### High (1)
- **#052** force_full probe uses unfiltered entities against AoI-filtered baselines -- delta compression permanently disabled for AoI clients

### Medium (4)
- **#053** Backward-compat full snapshot encoded every tick for unused resource -- wasted CPU/memory
- **#054** Full entity Vec cloned for clients without player position -- triggered during authority transfer
- **#055** 120s coordinator read timeout -- delays crash detection by 2 minutes
- **#056** NPC spawn range scales with WORLD_EXTENT -- entities scatter beyond AoI, world appears empty

### Low (1)
- **#057** Per-client AoI filter clones EntityState per entity -- accepted tradeoff, quantified for future reference

## Recommended Fix Order (AoI Filtering)

1. **#052** (high, small effort) -- Move force_full probe inside per-client loop, compare against filtered `client_entities`. **Must fix before AoI provides any bandwidth benefit.**
2. **#053** (medium, small effort) -- Remove backward-compat `ClientSnapshot.frame` encoding, update tests to use per-client channels
3. **#056** (medium, small effort) -- Adjust NPC spawn range to cluster near origin or use `AOI_RADIUS` instead of `WORLD_EXTENT`
4. **#054** (medium, small effort) -- Cache last known AoI center on `ClientHandle`, use as fallback when player position unavailable
5. **#055** (medium, small effort) -- Reduce coordinator read timeout to 30-60s, optionally add coordinator-side keepalive
6. **#057** (low, future) -- Index-based AoI filtering to avoid per-entity clones (only if profiling shows clone is significant)

## Recommended Fix Order (Delta Compression)

Phase 1 (data integrity) is complete. Remaining items:

1. ~~**#049** -- Skip compute_delta when fallback is likely~~ **resolved**
2. **#020** -- Share delta computation across clients with identical views (medium effort)
3. **#019** -- Reuse baseline HashMap on full snapshot (small effort)
4. **#026** -- Accept one-tick gap or defer PlayerAssigned (medium effort, low priority)
