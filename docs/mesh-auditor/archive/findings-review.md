# Audit Findings Review

**Date:** 2026-04-14 (updated)
**Scope:** All 44 findings across 4 audit cycles
**Status:** 23 resolved, 21 open (reviewed below)

---

## Resolution Summary

Of 44 total findings, **23 are resolved** (either fixed in code or obsoleted by other fixes):

| # | Title | How resolved |
|---|-------|-------------|
| 002 | Authority transfer fire-and-forget | Two-phase transfer protocol |
| 001 | Async-bridge data loss on overwrite | `std::mem::take` per field |
| 007 | Drain despawns during iteration | Two-phase collect-then-mutate |
| 015 | Outbound resources not cleared | `std::mem::take` each tick |
| 018 | Baseline drift on dropped frames | Baseline updated only in `Ok(())` arm of `try_send`; cleared to `None` on drop (`systems.rs:1218,1225,1250,1258,1269,1277`) |
| 021 | ACK infrastructure unused | `acked_tick` read in `prepare_client_snapshot` for stale-ACK detection; forces full snapshot after 40 ticks without ACK progress (`systems.rs:1195-1198`) |
| 022 | Change threshold per-component | `vec3_changed` uses `distance_squared()` instead of per-component `abs()` (`delta.rs:115`) |
| 023 | Ghost entities from baseline drift | Server: #018 fix prevents drift. Client: staleness sweep in `world-view.ts:278` removes entities not updated for >6s |
| 024 | Full snapshot cloned per client | `ClientFrame::Shared(Arc<Vec<u8>>)` wraps full snapshot once, `Arc::clone` per client (`resources.rs:17`, `systems.rs:1214,1247`) |
| 025 | ACK parsing no tick validation | Monotonicity check (`tick > prev`) in `network.rs:181-186`; server tick clamp (`.min(tick.0)`) at consumer in `systems.rs:1195` |
| 027 | NPC movement defeats delta | Delta fallback: when >80% entities changed, sends full snapshot instead (`systems.rs:1245`, `DELTA_FALLBACK_THRESHOLD = 0.80`) |
| 028 | Dual authority during transfer | `Without<TransferOutbound>` filters |
| 029 | Chunk reassembly memory leak | Global sweep every 100 iterations |
| 030 | Transfer retry uses stale state | Re-serialize from ECS components |
| 031 | Player input lost during transfer | Forward to target_addr |
| 033 | TransferAck retries too low | `TRANSFER_MAX_RETRIES` = 3 |
| 035 | serialized_msg memory overhead | Field removed from component |
| 040 | RemoveEntity orphans on target | Forward `ForwardedRemove` |
| 041 | Heartbeat counts frozen entities | `Without<TransferOutbound>` filter |
| 042 | Transfer entities invisible to clients | Included with target authority |
| 036 | Schedule ordering concern | Verified correct (documented) |
| 004 | EntityLookup desync risk | Mitigated by two-phase transfer |
| 006 | Replication clones entire entity Vec | Mitigated by chunking (clone per chunk, bounded size) |

---

## Open Findings — Prioritized Action Plan

| # | Title | Impact | Workload | Action |
|---|-------|--------|----------|--------|
| **003** | Tokio mutex in tick loop | **high** | low | Fix |
| **013** | UDP sends block game loop | **high** | med | Fix |
| **009** | `process::exit` in game loop | med | low | Fix |
| **010** | Exclusive world access prevents parallelism | med | high | Defer |
| **011** | No client input rate limiting | med | low | Fix |
| **012** | Rebalance directive last-writer-wins | med | low | Fix |
| **016** | Bincode serialization cost at scale | med | med | Defer |
| **019** | Per-client baseline memory unbounded | med | low | Fix |
| **020** | Delta computation O(N*C) per tick | med | med | Fix |
| **026** | Registration/snapshot race | med | med | Defer |
| **032** | Chunk total_chunks not validated | med | low | Fix |
| **037** | ENTITIES_PER_CHUNK not validated vs UDP | med | low | Fix |
| **038** | PlayerTransfer handlers are stubs | med | low | Defer |
| **039** | Drain retransfers aborted entities every tick | med | low | Fix |
| **005** | Client snapshot double iteration | low | low | Fix |
| **008** | Outbound Vecs never shrink | low | low | Fix |
| **014** | Tick counter not synchronized across servers | low | low | Not worth |
| **017** | Rebalance entity_ids field ignored | low | low | Fix |
| **034** | Transfer timeout off-by-one (50ms) | low | low | Fix |
| **043** | Global sweep may evict slow peers | low | low | Fix |
| **044** | Transfer retry uses unwrap | low | low | Fix |

---

## Detailed Assessment

### Tier 1: Fix Now — High Impact, Low/Med Workload

#### #003 — Tokio mutex in tick loop
**Impact: high | Workload: low**

Three `tokio::Mutex::lock().await` calls in the hot 50ms tick loop (`game_loop.rs:92,120,242`). Under contention (UDP burst, many clients), the game loop yields to the tokio scheduler, adding unpredictable latency to tick timing. The fix is straightforward: swap to `std::sync::Mutex` with `try_lock()` or use lock-free channels (`tokio::sync::mpsc`) to move data from async tasks to the game loop.

#### #013 — UDP sends block game loop
**Impact: high | Workload: med**

UDP replication sends happen inside the tick loop via `.await` (`game_loop.rs:177-179`). Each `send_to` yields. With 9 peers and chunked payloads, this is dozens of awaits per tick. Fix: serialize replication data in the ECS schedule, hand bytes to a dedicated sender task via a channel. The game loop should never await network I/O.

#### #018 — Baseline drift on dropped frames -- RESOLVED
**Impact: high | Workload: low**

~~The server advances its per-client baseline after serializing a delta frame, even if the frame is dropped (channel full).~~ Fixed: baseline updates only in `Ok(())` arm of `try_send`, cleared to `None` on `TrySendError::Full`. All three send paths (full, fallback-full, delta) follow this pattern. See `systems.rs` `prepare_client_snapshot`.

#### #023 — Ghost entities from baseline drift -- RESOLVED
**Impact: high | Workload: low**

~~Direct consequence of #018.~~ Fixed on both sides: server-side by #018 fix (baselines can't drift), client-side by staleness sweep in `world-view.ts:278` (removes entities not updated for >6s as defense-in-depth).

### Tier 2: Fix Soon — Medium Impact, Low Workload

#### #009 — `std::process::exit` in game loop
**Impact: med | Workload: low**

Drain-complete path calls `std::process::exit(0)` bypassing async runtime teardown and Drop impls. Currently harmless but will break once persistence flush requires graceful shutdown. Fix: break out of the game loop, let tokio runtime shut down naturally.

#### #011 — No client input rate limiting
**Impact: med | Workload: low**

A client can send unlimited `MoveInput` messages per tick. At 60 FPS, that's 60 messages/sec per client, each acquiring the inbound lock. Fix: process at most N inputs per client per tick, drop excess.

#### #012 — Rebalance directive last-writer-wins
**Impact: med | Workload: low**

`PendingTransfers.transfers` is an `Option` — a second directive before the first is processed silently replaces it. Fix: use a `Vec` to queue directives, or process immediately on receipt.

#### #019 — Per-client baseline memory unbounded
**Impact: med | Workload: low**

Each client's baseline HashMap grows with entity count and never shrinks. With entity churn (spawn/despawn cycles), dead entries accumulate. Fix: rebuild baseline from scratch on each full snapshot instead of patching incrementally.

#### #020 — Delta computation O(N*C) per tick
**Impact: med | Workload: med**

Each client gets an independent delta computation against its own baseline. Clients with identical baselines (common after a full snapshot) could share a single delta. Fix: track baseline generation numbers, compute once per unique baseline, share the result.

#### #021 — ACK infrastructure unused -- RESOLVED
**Impact: med | Workload: med**

~~Client tick ACKs are parsed and stored but never consulted.~~ Fixed: `prepare_client_snapshot` reads `acked_tick` for stale-ACK detection. When `last_sent_tick - acked > ACK_STALE_TICKS (40)`, forces a full snapshot. ACK monotonicity validated in `handle_client`, server tick clamped at consumer.

#### #022 — Change threshold per-component -- RESOLVED
**Impact: med | Workload: low**

~~Delta compression uses per-component abs() comparison.~~ Fixed: `vec3_changed` now uses `a.distance_squared(b) > CHANGE_THRESHOLD_SQ` for uniform directional sensitivity. `CHANGE_THRESHOLD_SQ = 0.0001`.

#### #024 — Full snapshot cloned per client -- RESOLVED
**Impact: med | Workload: low**

~~Full snapshot Vec<u8> cloned per client.~~ Fixed: `ClientFrame::Shared(Arc<Vec<u8>>)` wraps the snapshot once, each client receives `Arc::clone` (pointer copy). Only the backward-compat `ClientSnapshot.frame` does a full clone.

#### #027 — NPC movement defeats delta compression -- RESOLVED
**Impact: med | Workload: low**

~~NPCs move every tick, making delta frames as large as full snapshots.~~ Fixed: delta fallback heuristic in `prepare_client_snapshot`. When >80% entities changed (`DELTA_FALLBACK_THRESHOLD = 0.80`), sends full snapshot via shared `Arc` instead of encoding a delta.

#### #032 — Chunk total_chunks not validated
**Impact: med | Workload: low**

Receiver accepts chunks where `total_chunks` differs between chunks of the same (server, tick). A corrupted packet could cause silent misassembly. Fix: validate consistency on second+ chunk, discard on mismatch.

#### #037 — ENTITIES_PER_CHUNK not validated
**Impact: med | Workload: low**

`ENTITIES_PER_CHUNK = 100` was chosen as a round number, not derived from entity serialized size vs UDP limit. Fix: add a compile-time or startup assertion that `100 * sizeof(bincode EntityState) + envelope < 65507`.

#### #039 — Drain retransfers aborted entities every tick
**Impact: med | Workload: low**

When a drain transfer times out and aborts (TransferOutbound removed), the next `process_drain` tick picks the entity up again and re-initiates the transfer. If the target is dead, this loops forever until the drain timeout (30s). Fix: track entities that failed transfer during drain and skip them.

### Tier 3: Fix Eventually — Low Impact, Low Workload

#### #005 — Client snapshot double iteration
**Impact: low | Workload: low**

`prepare_client_snapshot` iterates auth entities, shadow entities, and transfer entities separately. Could be merged into fewer passes. Minor optimization.

#### #008 — Outbound Vecs never shrink
**Impact: low | Workload: low**

`OutboundTransfers.messages` and `OutboundForwards.messages` are `Vec`s that grow with peak usage and never release memory. Fix: periodically `shrink_to_fit()` or use a fixed-size ring buffer.

#### #017 — Rebalance entity_ids field ignored
**Impact: low | Workload: low**

`RebalanceDirective.entity_ids` exists in the protocol but is always sent empty and never read. Fix: either wire it up (coordinator specifies entities to transfer) or remove the field to reduce confusion.

#### #025 — ACK parsing no tick validation -- RESOLVED
**Impact: low | Workload: low**

~~Binary ACK parser accepts any u64 tick value.~~ Fixed: monotonicity check (`tick > 0 && tick > prev`) in `handle_client` rejects zero and non-advancing ACKs. Server tick clamping (`.min(tick.0)`) in `prepare_client_snapshot` prevents future-tick attacks. Malicious `u64::MAX` ACK is clamped to current tick.

#### #034 — Transfer timeout off-by-one
**Impact: low | Workload: low**

Uses `current_tick - sent_tick > TRANSFER_TIMEOUT_TICKS` (strict >), so timeout fires at 11 ticks (550ms) not 10 (500ms). Fix: use `>=`.

#### #043 — Global sweep may evict slow peers
**Impact: low | Workload: low**

Global max tick comparison may evict chunk reassembly entries from a server that just restarted (tick 0 vs peers at tick 5000). Fix: use per-server latest ticks instead of global max.

#### #044 — Transfer retry uses unwrap
**Impact: low | Workload: low**

Component access in `process_transfer_timeouts` uses `unwrap()` after query. Safe today but fragile. Fix: use `if let Some(...)`.

### Tier 4: Defer or Not Worth

#### #010 — Exclusive world access prevents parallelism
**Impact: med | Workload: high**

Most systems use `&mut World` (exclusive access), preventing Bevy from running them in parallel. Fixing this requires converting to proper system functions with query parameters. Large refactor, benefit is marginal at current server count (3). Defer until tick budget pressure requires it.

#### #016 — Bincode serialization cost at scale
**Impact: med | Workload: med**

Each tick serializes all auth entities once per peer via bincode. At scale, the CPU cost of serialization becomes measurable. Defer — the chunking fix already amortizes this better, and the real fix is #013 (move sends off the tick loop).

#### #026 — Registration/snapshot race
**Impact: med | Workload: med**

Client receives `PlayerAssigned` before its player entity appears in the first snapshot (1-tick race). Causes a brief flash in the client. Low priority — cosmetic only.

#### #038 — PlayerTransfer handlers are stubs
**Impact: med | Workload: low**

The protocol messages exist but handlers just log. This is intentional — persistence isn't implemented yet. Will be filled in when persistence is built. Defer.

#### #014 — Tick counter not synchronized
**Impact: low | Workload: low**

Tick counters on different servers drift. Not a problem — each server's tick is only used locally for ordering and staleness. Cross-server ordering uses the replication tick embedded in each message. **Not worth fixing.**

---

## Implementation Plan

### Phase 1: Data Integrity (baseline + delta fixes) -- COMPLETE
*Resolved: #018, #023, #021, #024, #022, #027, #025 — all delta compression issues*

All 7 findings in this phase are resolved. Baseline drift fixed, ACKs wired into stale detection, vector distance thresholds, delta fallback heuristic, Arc-shared snapshots, ACK validation.

### Phase 2: Tick Loop Health
*Fixes #003, #013, #009 — remove blocking operations from the game loop*

The tokio mutex and UDP sends are the main sources of tick timing jitter. Fixing them makes the game loop deterministic. #009 (process::exit) fits naturally in this pass since it's also a game loop lifecycle issue.

### Phase 3: Client Robustness
*Fixes #011, #019, #020, #005 — rate limiting, memory, and computation efficiency*

Input rate limiting prevents abuse, baseline memory fix prevents leaks, delta sharing reduces per-tick CPU. All client-facing.

### Phase 4: Transfer & Rebalance Polish
*Fixes #012, #039, #032, #037, #034, #017, #043, #044 — small fixes across transfer and rebalance systems*

Batch of small, independent fixes. Each is low workload. Do them all in one pass.

### Phase 5: Housekeeping
*Fixes #008 — Vec shrinking*

Lowest priority. Do whenever convenient.

### Deferred
- #010 (ECS parallelism) — large refactor, defer until tick budget requires it
- #016 (serialization cost) — addressed partially by chunking, defer
- #026 (registration race) — cosmetic, defer
- #038 (PlayerTransfer stubs) — waiting on persistence implementation
- #014 (tick sync) — not worth fixing
