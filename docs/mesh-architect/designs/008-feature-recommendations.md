# 008 — Feature & Improvement Recommendations

**Date:** 2026-04-14
**Status:** Tier 1 & 2 implemented (see work-items/008-tier1-tier2-fixes.md)
**Scope:** Full codebase review + 55 auditor findings analysis

## Executive Summary

The mesh is in strong shape. Core replication, authority transfer, crash recovery, AoI filtering, and client prediction all work. The test suite is comprehensive (16 scenarios). The deployment story covers local dev, Docker, and embedded hardware.

The most impactful next steps are **fixing existing auditor findings** (not building new features), followed by a small number of targeted improvements. Most "big feature" ideas (coordinator HA, entity persistence, zone sharding) carry complexity far exceeding their value at the current prototype scale.

---

## Tier 1: Fix Before Building Anything New

These are existing bugs and risks flagged by the auditor that undermine the reliability of features already built. Fixing them improves the mesh more than any new feature would.

### 1.1 Delta Compression Is Silently Disabled (Audit #052)

**Value: HIGH | Complexity: LOW**

The force-full probe compares AoI-filtered baselines against the *unfiltered* entity list. In any world where entities exist outside a client's AoI (i.e., always), the change ratio exceeds 80% and delta compression falls back to full snapshots every tick. Delta compression is effectively dead code in production.

**Fix:** Move the probe inside the per-client loop; compare `client_entities` (AoI-filtered) against that client's baseline. This is a ~10 line change that could cut client bandwidth by 50-80%.

### 1.2 Baseline Drift on Dropped Frames (Audit #018, #023)

**Value: HIGH | Complexity: LOW**

The server updates the per-client baseline *before* confirming frame delivery. If the frame channel is full (client is slow), the baseline advances but the client never received it. Subsequent deltas are computed against a baseline the client doesn't have, producing incorrect removals and ghost entities until the next full snapshot.

**Fix:** Move baseline update to after `try_send` succeeds. ~5 line change.

### 1.3 Input Loss During Player Transfer (Audit #031)

**Value: HIGH | Complexity: MEDIUM**

During the 150-600ms transfer window, player inputs are processed on the source (which has the entity frozen) but never forwarded to the target. The player experiences a control blackout during every transfer.

**Fix:** Forward inputs arriving for TransferOutbound entities to the target server. The forwarding infrastructure already exists (ForwardedInput over UDP control channel). Needs ~30 lines in the input processing system.

### 1.4 Dual Authority During Transfer (Audit #028)

**Value: HIGH | Complexity: LOW**

Source continues simulating and replicating the entity during the ACK window. Both servers are authoritative simultaneously for 100-600ms. Clients connected to different servers see the entity at different positions.

**Fix:** TransferOutbound entities are already excluded from simulation (`Without<TransferOutbound>` on physics). Verify they're excluded from peer replication too. If not, add the filter. Small change.

### 1.5 Chunk Reassembly Memory Leak (Audit #029)

**Value: MEDIUM | Complexity: LOW**

Incomplete UDP chunk reassembly entries from crashed/unreachable peers grow without bound. The cleanup sweep uses a global tick max, which can evict entries from legitimately slow peers (#043).

**Fix:** Per-server latest tick tracking for cleanup. Cap max reassembly entries (e.g., 1000). ~20 lines.

### 1.6 RemoveEntity During Transfer (Audit #040)

**Value: HIGH | Complexity: LOW**

If a client sends RemoveEntity while the entity is mid-transfer, the source despawns it but the target already accepted it. The entity becomes an orphan on the target with no way to remove it.

**Fix:** Check for TransferOutbound before despawning. Forward ForwardedRemove to the target. ~15 lines.

---

## Tier 2: Targeted Improvements (Good ROI)

These are new capabilities that address real gaps without introducing architectural complexity.

### 2.1 Client Input Rate Limiting (Audit #011)

**Value: MEDIUM | Complexity: LOW**

Clients can flood the server with unlimited MoveInput and SpawnEntity messages. A single malicious client can saturate the input processing phase. There's also no ownership check on RemoveEntity — any client can remove any entity.

**Fix:** Per-client token bucket (e.g., 30 inputs/sec, 5 spawns/sec). Ownership map for RemoveEntity (client can only remove entities it spawned). ~50 lines in network.rs.

### 2.2 Coordinator Read Timeout Reduction (Audit #055)

**Value: MEDIUM | Complexity: TRIVIAL**

The 120-second WebSocket read timeout means a coordinator connection failure takes 2 minutes to detect. Combined with the 15-second heartbeat timeout on the coordinator side, there's a 2-minute window where the sim-server thinks it's connected but isn't.

**Fix:** Reduce to 30 seconds. Add periodic keepalive pings from the coordinator (which already has a Keepalive message type). One constant change + ~10 lines for ping scheduling.

### 2.3 Shared Snapshot Encoding (Audit #024, #020)

**Value: MEDIUM | Complexity: LOW**

When all clients see the same entities (common case), the server encodes an identical binary snapshot N times and clones it N times. For 10 clients, that's 10x the CPU and allocation.

**Fix:** Encode once into `Arc<Vec<u8>>` (or `bytes::Bytes`), send the same buffer to all clients sharing the same AoI viewport. Only encode separately for clients with different filtered entity sets. ~40 lines.

### 2.4 Transfer Retry with Fresh State (Audit #030)

**Value: MEDIUM | Complexity: LOW**

Transfer retries re-send the original serialized message. The entity's state has advanced 500ms+ since then. The target receives outdated position/velocity.

**Fix:** Re-serialize from current ECS state on retry instead of caching the original bytes. ~15 lines in the transfer timeout system.

### 2.5 Weighted Rebalance Algorithm

**Value: MEDIUM | Complexity: MEDIUM**

The rebalance algorithm uses raw entity count as the sole load metric. Player entities are significantly more expensive than NPCs (they receive inputs, trigger prediction, need forwarding). 50 players could cost more CPU than 500 NPCs.

**Fix:** Weighted entity count (e.g., Player = 3, NPC = 1). Include tick budget percentage in heartbeats. Rebalance based on weighted load + actual CPU pressure. ~60 lines across coordinator and sim-server.

### 2.6 Graceful Client Reconnection

**Value: MEDIUM | Complexity: MEDIUM**

When a client's server dies or transfers its player, the client gets a Reconnect message with the new server URL. But it reconnects fresh — loses prediction state, pending inputs, and interpolation warmup. The player sees a visible "snap" to a new position.

**Fix:** On reconnect, carry over pending inputs and last known position. Send RequestSnapshot immediately. Apply warmup interpolation from the carried-over position instead of snapping. ~80 lines across client network.ts and prediction.ts.

---

## Tier 3: Considered and Rejected (Complexity Too High)

These are commonly desired features that aren't worth their implementation and maintenance cost at the current scale.

### 3.1 Coordinator High Availability

**Problem:** Single coordinator is a SPOF.
**Why not:** The coordinator is stateless — sim-servers reconnect automatically when it restarts. It runs as a systemd service with auto-restart. Actual downtime is ~2 seconds on crash. Building Raft/Paxos consensus for a coordinator that can recover in 2 seconds adds hundreds of lines of distributed systems complexity for negligible availability improvement. The right move at scale is a hot standby with leader election (much simpler than consensus), but that's not needed yet.

### 3.2 Entity Persistence Layer

**Problem:** All entity state is in-memory. Coordinator restart loses registration state.
**Why not:** Sim-servers re-register on coordinator restart. Entity state lives in the sim-servers, not the coordinator. The only state lost on coordinator crash is the rebalance timer and current peer list, which rebuilds in <5 seconds. Adding SQLite/Redis persistence adds I/O latency to the tick loop and schema migration complexity for a benefit that only matters if ALL servers crash simultaneously — which the embedded hardware mesh has never experienced.

### 3.3 Zone-Based Spatial Sharding

**Problem:** As entity counts grow, each server processes entities across the entire world.
**Why not:** The current AoI filtering already limits which entities are replicated to which peers. Entity simulation is O(N) where N is the server's authoritative entity count, not total world entities. At the current scale (hundreds of entities per server), spatial sharding adds complexity (zone boundary handling, cross-zone interaction, hot spot migration) without measurable benefit. The right time for this is when a single server can't simulate its assigned entities at 20Hz — and we're far from that.

### 3.4 Full Network Compression (Quantization, Bitpacking)

**Problem:** Entities are 47 bytes each. Could be smaller with position quantization and bitpacking.
**Why not:** At 500 entities across 3 servers, total replication bandwidth is ~90KB/tick (47 × ~650 entities × chunk overhead). That's ~1.8MB/s — well within UDP capacity even on the UNO Q boards' WiFi. Quantization saves ~40% but adds encoding complexity, precision loss, and debugging difficulty. The bandwidth ceiling is the UDP chunking limit, not raw byte count. Optimize when bandwidth is actually the bottleneck.

### 3.5 Advanced Anti-Cheat

**Problem:** No input validation, speed checks, or server-side movement verification.
**Why not:** This is a prototype for server meshing architecture, not a shipping game. Anti-cheat is game-specific logic that depends on movement models, ability systems, and game rules that don't exist here. The basic rate limiting in Tier 2 prevents accidental flooding. Anything more is premature for a prototype.

### 3.6 Multi-Tier AoI / LOD System

**Problem:** Binary AoI (inside radius = full update, outside = nothing). No gradual degradation.
**Why not:** The current 500-unit AoI radius with 750-unit peer replication margin works well. Adding LOD tiers (e.g., full detail at 200u, position-only at 500u, heartbeat-only at 1000u) multiplies the encoding paths, baseline tracking, and client interpolation logic. The benefit is marginal when the AoI radius already limits entity counts to manageable levels. Useful for MMO scale (10,000+ entities in view), not prototype scale.

---

## Tier 4: Nice-to-Have (Low Priority)

Small improvements that are worth doing if you're already in the relevant code.

| Improvement | Audit # | Effort | Notes |
|-------------|---------|--------|-------|
| Vec clone → move in replication | #006 | 5 min | Change `.clone()` to owned move |
| Outbound vec capacity reuse | #008 | 10 min | `drain()` instead of `take()` |
| Remove dead backward-compat snapshot encoding | #053 | 15 min | Update tests to read from channel |
| Cache fallback position for no-AoI clients | #054 | 15 min | Avoid full clone on stale player |
| Reduce coordinator read timeout to 30s | #055 | 2 min | One constant change |
| Add `process::exit` → cooperative shutdown | #009 | 30 min | Let tokio runtime drain cleanly |
| Await UDP sender task on exit | #050 | 10 min | Prevent mid-drain cancellation |

---

## Recommended Order of Work

1. **Fix #052** (delta compression actually works) — Biggest single win
2. **Fix #018** (baseline drift) — Makes delta compression reliable
3. **Fix #031 + #028** (transfer input loss + dual authority) — Transfers become seamless
4. **Fix #040 + #029** (remove during transfer + reassembly leak) — Edge case safety
5. **Add input rate limiting** (#011) — Basic security
6. **Shared snapshot encoding** (#024) — Scales client count
7. **Weighted rebalance** — Better load distribution
8. **Graceful client reconnection** — Smoother player experience

Items 1-4 are bug fixes to existing features. Items 5-8 are new functionality. This order prioritizes making what exists work correctly before adding new capabilities.

---

## Conclusion

The mesh doesn't need more features — it needs the features it has to work correctly under edge cases. The auditor found 10 HIGH severity issues, most concentrated in delta compression and authority transfers. Fixing those (Tier 1) will improve the mesh more than any new feature. After that, input rate limiting, shared encoding, and weighted rebalancing (Tier 2) are the highest-ROI additions. Everything else is either premature optimization or complexity that isn't justified at the current scale.
