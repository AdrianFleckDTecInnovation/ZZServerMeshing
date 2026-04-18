# 009 — Feature Evaluation: What's Worth Building Next

**Date:** 2026-04-14
**Status:** Draft
**Scope:** Full codebase + coordinator + sim-server review, informed by 57 auditor findings

## Context

Doc 008 covers auditor-driven bug fixes and rightly concludes that fixing existing features matters more than adding new ones. This document looks at a different question: **beyond bug fixes, what capabilities would genuinely improve the mesh as an architecture prototype, and what's not worth the complexity?**

The bar is: does this make the mesh more demonstrable, more debuggable, or more resilient in ways that matter at the current scale (3 servers, hundreds of entities, embedded hardware)?

---

## Worth Building

### 1. Protocol Version Handshake

**Value: HIGH | Complexity: LOW (~30 lines)**

There is no version negotiation anywhere in the system. Sim-servers connect to the coordinator and start exchanging JSON. Peers exchange UDP bincode. Clients connect via WebSocket. None of these connections verify that both sides speak the same protocol version.

**Why it matters:** The binary protocol is 47 bytes per entity. Any field addition, reordering, or type change silently corrupts deserialization. On the UNO Q boards, a stale binary from a failed deploy will produce garbage entity state with no error — just wrong positions and phantom entities. This has likely already happened during development and been misdiagnosed as a "replication bug."

**Fix:**
- Add a `PROTOCOL_VERSION: u16` constant to `shared/` (increment on any wire format change)
- Coordinator WebSocket: include `protocol_version` in `Register`. Coordinator rejects mismatches with a clear error.
- UDP peer replication: first 2 bytes of every datagram are the protocol version. Receiver drops mismatched packets with a warning log.
- Client WebSocket: server sends version in `PlayerAssigned`. Client can display "version mismatch" instead of showing corrupted state.

This is the single cheapest change that prevents the most confusing class of bugs.

### 2. Coordinator Rebalance Feedback Loop

**Value: HIGH | Complexity: MEDIUM (~80 lines)**

The coordinator fires `RebalanceDirective` and never learns what happened. It has no visibility into whether the transfer succeeded, failed, was partially completed, or is still in progress. The 5-second cooldown is a blind timer — it can re-issue directives for entities that are mid-transfer, or wait 5 seconds when the previous transfer completed instantly.

**Current state:**
- `registry.rs:168-226`: `try_rebalance()` sends directive, returns `bool`
- No `RebalanceResult` or `TransferComplete` message exists
- Coordinator doesn't track in-flight transfers at all

**Fix:**
- Add `CoordClientMsg::TransferResult { entity_id, target_server, success: bool }` to the protocol
- Sim-server sends this after each authority transfer completes or fails
- Coordinator tracks in-flight transfer count per server
- Suppress new rebalance directives while transfers are in-flight
- Log transfer success rate as a metric

This transforms rebalancing from fire-and-forget to a closed-loop system. The coordinator can detect stuck transfers, retry intelligently, and avoid piling up directives on a server that's already struggling.

### 3. Structured Tracing with Mesh Context

**Value: MEDIUM | Complexity: LOW (~40 lines)**

The codebase uses `tracing` but fields are inconsistent. Some log lines include `server_id`, most don't. Entity IDs appear in some logs but not others. When debugging a cross-server issue (e.g., "entity disappeared during transfer"), you have to grep three server logs and mentally correlate them.

**Fix:**
- Add a `tracing::Span` at tick-loop level with `server_id` and `tick` as fields
- Add entity_id spans in transfer, replication, and input processing
- Use `tracing::instrument` on key functions in `systems.rs`
- No new dependencies — `tracing` is already used everywhere

This doesn't change behavior but dramatically improves debuggability, especially on the UNO Q boards where you're SSHing into three devices and tailing logs.

### 4. Server Join Grace Period

**Value: MEDIUM | Complexity: LOW (~20 lines)**

When a new sim-server registers with the coordinator, it immediately becomes eligible for rebalance targets. But it has zero shadows, zero context about the world, and hasn't yet received a full replication snapshot from peers. A rebalance directive arriving in its first second could transfer entities to a server that doesn't know where anything else is.

**Current state:**
- `registry.rs:99-111`: `heartbeat()` updates entity count and weighted load
- `registry.rs:168`: `try_rebalance()` considers all registered servers equally
- No concept of "this server just joined"

**Fix:**
- Add a `registered_at: Instant` field to `ServerEntry`
- Skip servers registered less than 10 seconds ago as rebalance *targets* (they can still be sources)
- Alternatively: wait until the server has sent at least 2 heartbeats (10 seconds at 5s interval)

Simple guard that prevents a race condition during server startup.

### 5. Entity Ownership Map

**Value: MEDIUM | Complexity: LOW (~40 lines)**

Any connected client can send `RemoveEntity` for any entity ID. There's no validation that the client owns or spawned the entity. Combined with the lack of authentication, this means one browser tab can delete entities spawned by another.

**Current state (network.rs):** Rate limiting exists for inputs and spawns (added recently), but `RemoveEntity` has no ownership check.

**Fix:**
- Track `spawned_by: ClientId` on each entity (or a `HashMap<EntityId, ClientId>` in `ClientRegistry`)
- `RemoveEntity` handler checks ownership before despawning
- Forwarded removes from peers bypass the check (server-to-server is trusted)

This is distinct from the rate limiting already in place. Rate limiting prevents flooding; ownership prevents one client from affecting another's entities.

---

## Probably Not Worth It

### 6. Runtime Configuration (Hot-Reload Constants)

**Value: MEDIUM | Complexity: MEDIUM-HIGH**

Constants like `AOI_RADIUS`, `TICK_RATE`, `SHADOW_STALE_TICKS`, rebalance thresholds, and heartbeat timeouts are compile-time. Changing them requires recompilation, which on the UNO Q boards means a full cross-compile + deploy cycle.

**Why not (yet):** The constants are stable. They've been tuned over multiple iterations and don't change frequently. Adding runtime config means either a config file (need to deploy it to boards), env vars (need to update systemd units), or a control API (more coordinator complexity). The cross-compile cycle is ~60 seconds — not painful enough to justify the plumbing. If the project reaches a phase where rapid tuning matters (e.g., profiling under real player load), this becomes worth it. Right now it's premature.

### 7. Prometheus/OpenTelemetry Metrics

**Value: MEDIUM | Complexity: MEDIUM**

The metrics server (`metrics_server.rs`) returns custom JSON with per-phase p50/p95/p99 timing. This works for the coordinator dashboard but isn't queryable by standard monitoring tools.

**Why not:** The custom JSON format is consumed by exactly one thing: the coordinator dashboard. Adding Prometheus format means a new dependency (`prometheus` or `metrics` crate), metric registration boilerplate, and a second export path. The dashboard already shows everything needed. If the mesh were running in production with an SRE team watching Grafana, Prometheus would be essential. For a 3-board prototype, the coordinator dashboard is sufficient.

### 8. Peer-to-Peer Liveness Detection

**Value: MEDIUM | Complexity: MEDIUM-HIGH**

Peers currently learn about each other only through the coordinator's `PeerList`. If UDP between two peers silently fails (firewall change, WiFi interference on the boards), neither peer knows. Shadows go stale after 15 ticks (750ms), then get cleaned up. The entities are "lost" until the coordinator triggers crash recovery — but the peer isn't actually dead, just unreachable from one specific server.

**Why not:** This is a real problem but the fix is complex. You need bidirectional UDP ping/pong between all peer pairs, timeout detection, and then a decision: tell the coordinator? Try alternate routes? Promote shadows to authoritative? Each option has edge cases. The current system handles it acceptably: stale shadows get cleaned up, the coordinator eventually detects the heartbeat gap and triggers recovery. The failure mode is temporary entity invisibility, not data loss. Worth revisiting if WiFi reliability on the boards becomes a persistent issue.

### 9. Multi-Entity-Type Support (Items, Projectiles, Structures)

**Value: LOW (for the prototype) | Complexity: HIGH**

Currently only `Player` and `Npc` entity types exist. Real games need items, projectiles, area effects, terrain modifications. Each type has different replication requirements (projectiles need high frequency, structures need reliability, items need ownership).

**Why not:** The mesh prototype demonstrates entity-level authority distribution, not game design. Adding entity types means new components, new simulation systems, new client rendering, new replication priority logic. Each type is a mini-feature. The current Player/NPC split already demonstrates the key architectural property: different entity types can have different weights and different replication behaviors. Adding more types is game development, not mesh development.

### 10. Coordinator High Availability (Leader Election)

**Value: LOW (at current scale) | Complexity: VERY HIGH**

Covered in 008 and rejected. Restating here for completeness: the coordinator is stateless, auto-restarts via systemd, and recovers in ~2 seconds. Actual downtime is negligible. Hot standby with leader election is hundreds of lines of distributed consensus code for a problem that causes 2 seconds of inconvenience.

### 11. Client-Side Interpolation Improvements

**Value: LOW | Complexity: MEDIUM**

The client uses basic tick-based interpolation between `previousPosition` and `currentPosition`. More sophisticated approaches (hermite curves, velocity-based extrapolation, jitter buffering) would make motion smoother.

**Why not:** The current interpolation is visually adequate at 20Hz. The entities are simple spheres and boxes. Smoother interpolation matters for AAA character animation, not for a mesh architecture demo. The prediction system (`prediction.ts`) with server reconciliation handles the player's own entity, which is the one that matters most for perceived responsiveness.

---

## Already Handled by Doc 008

For completeness, these are addressed in the existing recommendation doc and its work items:

| Feature | 008 Reference | Status |
|---------|--------------|--------|
| Delta compression fix (#052) | Tier 1.1 | Needs implementation |
| Baseline drift fix (#018) | Tier 1.2 | Already fixed in code |
| Transfer input forwarding (#031) | Tier 1.3 | Needs implementation |
| Dual authority during transfer (#028) | Tier 1.4 | Needs verification |
| Chunk reassembly cleanup (#029) | Tier 1.5 | WI-080 |
| RemoveEntity during transfer (#040) | Tier 1.6 | Needs implementation |
| Client input rate limiting (#011) | Tier 2.1 | WI-081, partially in code |
| Coordinator timeout (#055) | Tier 2.2 | WI-082 |
| Shared snapshot encoding (#024) | Tier 2.3 | WI-084 |
| Transfer retry fresh state (#030) | Tier 2.4 | Needs implementation |
| Weighted rebalance (#2.5) | Tier 2.5 | WI-083, partially in code |
| Client reconnection smoothing | Tier 2.6 | Needs implementation |

---

## Recommended Priority (New Items Only)

1. **Protocol version handshake** — Cheapest, prevents the most confusing class of bugs
2. **Entity ownership map** — Natural companion to the rate limiting already added
3. **Server join grace period** — 20 lines, prevents a real race condition
4. **Rebalance feedback loop** — Makes the coordinator actually aware of transfer outcomes
5. **Structured tracing** — Pays for itself on the first cross-server debugging session

Items 1-3 are each under 40 lines and could be done in a single session. Item 4 is the most impactful architectural improvement. Item 5 is quality-of-life that compounds over time.

---

## Conclusion

The mesh is architecturally complete for what it set out to demonstrate: entity-level authority distribution across multiple servers with automatic rebalancing and crash recovery. The 008 bug fixes should come first. After those, the five recommendations above are the highest-value additions — they improve robustness and debuggability without changing the architecture.

Everything else (runtime config, Prometheus, peer liveness, new entity types, coordinator HA, interpolation) is either premature for the prototype scale, solvable with simpler workarounds, or game development rather than mesh development. If nothing from the "Worth Building" list seems compelling, that's a valid signal: the prototype may be feature-complete for its purpose, and the remaining work is polish and bug fixing.
