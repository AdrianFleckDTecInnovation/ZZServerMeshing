# Churn-Resilience Audit — 2026-04-17

**Scope:** Bugs, pitfalls, and overcomplications measured against the north-star goal: a no-fuss peer-to-peer, server-authoritative mesh that dynamically adjusts to sim-servers dropping in and out via the coordinator.

**Method:** Read code paths around server join, drop, restart (design 013), coordinator outage, authority transfers, client connections, and replication under churn. Every finding cites `file:line`.

---

## Verdict

**The mesh is NOT ready to handle dynamic churn today.** Two findings are dispositive:

- **074 (client never re-assigns on reconnect)** — any client caught on a crashed/drained server is permanently stranded until browser reload. The mesh can survive churn internally, but from the user's view a crash equals a disconnect.
- **075 + 077 + 083 (graceful drain is entirely fiction)** — operator-initiated restarts have the same blast radius as crashes.

Design 013 (session_id, PROTOCOL_VERSION=2) is correctly implemented and matches its design doc. But 013 only fixes the receiver-side stuck-reassembly bug; it does not address the coordinator re-registration race (076), the drain flow being non-functional (075/077/083), or the client reconnection path (074).

Interior mesh machinery (UDP replication, shadow recovery after session change, transfer ACK retries, rebalance TTL sweeps) is reasonably robust. The **edges** — where clients touch the mesh, where operators touch the mesh, where peers rejoin after restart — are the weak points.

**Must-fix before claiming "dynamic churn ready":**

- **074** — client re-assigns via `/assign` after disconnect
- **075 + 077** — Ctrl-C actually triggers drain and sends `Draining` to coordinator
- **076** — coordinator prefers new registration over stale `reply_tx`
- **080 + 086** — players can actually migrate via `Reconnect`

Everything else is noise, bloat, or edge-case glitches rather than mesh-wide outages.

---

## Top 5 "delete this" overcomplications

1. **`ServerMessage::Reconnect` variant** (086) — Poster child for dead code masquerading as a feature. Delete it or wire it up (080 is the wire-up path). Current state misleads both developers and doc readers.
2. **Delta compression machinery** (092) — NPCs move every tick, so the 80% fallback fires every tick. Per-client baselines, compute_delta, ACK tracking, stale-ACK detection — none of it pays for itself in the current workload. Remove until a workload emerges that benefits.
3. **Three-branch bootstrap in `prepare_replication`** (087) — Collapsing three overlapping phase-filter code paths into one classifier reduces 100+ lines, eliminates one Vec allocation per peer per tick, and removes a class of branch-interaction bugs.
4. **Ctrl-C drain branch in `main.rs`** (075 + 077 + 083) — The entire graceful-shutdown ladder is dead code. Either implement it properly or delete the illusion.
5. **`DrainInfo.started_tick == 0` sentinel** (088) — Emblematic half-finished cleanup: complexity-audit 6.3 was resolved halfway. `Option<u64>` fix is ~10 lines.

---

## Findings by focus area

### Server join
- [076](#076-coordinator-rejects-re-register-for-15-s-after-crash) — coordinator rejects restart for 15 s
- [085](#085-initial_npcs-duplicates-on-restart) — `initial_npcs` duplicates on restart

### Server drop
- [077](#077-graceful-drain-never-broadcasts-coordclientmsgdraining) — `Draining` message never sent on graceful exit
- [075](#075-ctrl-c-graceful-drain-is-dead-code) — Ctrl-C drain is dead code
- [083](#083-drain-timeout-30-s-vs-ctrl-c-sleep-3-s) — 30 s drain timeout is really 3 s
- [084](#084-heartbeat-timeout-vs-re-register-race-destructively-promotes-shadows) — destructive recovery on restart race
- [090](#090-shadow-cleanup-runs-after-prepare_client_snapshot) — shadow cleanup ordering blip

### Server restart (design 013 / session_id)
- [078](#078-peer-only-straggler-packet-thrashes-peer_session_resets) — session straggler thrash
- [081](#081-session-reset-drops-peer_health-counters-mid-window) — peer_health counter reset timing
- [082](#082-session_of_peer-and-latest_tick-hashmaps-grow-unbounded) — unbounded receiver state maps
- [089](#089-random-session_id--0-collides-with-never-seen-sentinel) — session_id = 0 sentinel collision

### Coordinator outage
- 076 (above) — register rejection is invisible to sim-server
- [079](#079-coordinator-client-connects-with-plain-ws-even-in-tls-deployments) — coordinator connection never uses TLS
- 084 (above) — coordinator drives destructive recovery on restart race

### Authority transfers during churn
- [080](#080-rebalance-never-targets-player-entities) — players never rebalance
- 087 (below) — replication bootstrap overlaps with normal path
- [091](#091-rebalance-threshold-blocks-small-mesh-rebalancing) — rebalance threshold blocks small-mesh balancing
- 092 (below) — delta fallback runs constantly, wasted CPU during transfer tick

### Client connections during churn
- [074](#074-client-never-re-fetches-assign-on-reconnect-a-dead-server-url-loops-forever) — client never re-assigns (CRITICAL)
- [086](#086-servermessagereconnect-exists-is-tested-and-documented--but-is-never-sent) — Reconnect message exists but never sent (CRITICAL)
- 080 (above) — player transfer path needs Reconnect wired up

### Replication under churn
- 078 — session reset thrash
- 081 — peer_health mid-window reset
- 082 — receiver state maps grow
- [087](#087-bootstrap-replication-has-three-nearly-identical-phase-filtering-paths) — three-way branching
- [092](#092-delta-compression-machinery-fires-fallback-every-tick-because-npcs-always-move) — delta over-engineering

---

## Findings

### 074 — Client never re-fetches `/assign` on reconnect; a dead server URL loops forever
**Severity: CRITICAL · Category: [bug]**

**Files:** `client/src/network.ts:86-97, 286-305`; `client/src/main.ts:7-14`

`getServerUrl()` is called exactly once at page load in `main.ts`. `NetworkClient` stores that URL in `this.url`. On any disconnect, `onclose` schedules `this.connect()`, which reopens `new WebSocket(this.url)` using the cached URL. If that sim-server crashed, was drained, or the coordinator rebalanced load, the client hammers the dead address forever with exponential backoff capped at `RECONNECT_MAX_DELAY_MS` — never talking to the coordinator again.

The `ServerMessage::Reconnect { url }` handler at `network.ts:272-278` assumes the server instructs the client to a new URL, but `grep` shows no code path in the whole repo constructs/sends `ServerMessage::Reconnect` (only two matches: the enum definition and JSON roundtrip test). Client has zero dynamic server-switching.

**Why it matters for churn:** The single most visible churn failure. Every crashed server strands all its clients until the user reloads the tab.

**Fix:** In `scheduleReconnect()`, re-invoke the `/assign` HTTP flow (factor `getServerUrl` into a reusable method on `NetworkClient`, call it before `new WebSocket(...)`). Add a small retry budget before falling back to a hardcoded `/assign` lookup.

---

### 075 — Ctrl-C graceful drain is dead code
**Severity: CRITICAL · Category: [bug]**

**Files:** `crates/sim-server/src/main.rs:210-224`; `crates/sim-server/src/systems/drain.rs:14-18`

On SIGINT the sim-server pushes `drain_directives.push(vec![])` (main.rs:218) — an **empty** targets list. `process_coord_events` (coordinator.rs:48-61) turns that into `DrainState(Some(DrainInfo { targets: vec![], started_tick: 0 }))`. Then `process_drain` bails on the first guard:

```rust
let (targets, started_tick) = match &world.resource::<DrainState>().0 {
    Some(info) if !info.targets.is_empty() => (info.targets.clone(), info.started_tick),
    _ => return,
};
```

So no entities are transferred, `ShutdownRequested` is never set, and the 3-second sleep in main.rs:222 elapses while the tick loop keeps running. When the signal handler wakes, `tokio::select!` returns and the process exits abruptly — every entity vanishes from the mesh until shadow cleanup on peers, same as a crash. The comment at main.rs:215 ("the game loop will use whatever peers it knows about") is false.

Related: `CoordClientMsg::Draining` is **never sent** by sim-server. `grep -n "CoordClientMsg::Draining"` shows it only in `protocol.rs` and `coordinator/main.rs` (handler). So coordinator-side `drain_server()` is never invoked for Ctrl-C either.

**Why it matters:** The whole point of graceful drain is to avoid a crash-style shadow blackout on operator-initiated restarts. Today it's indistinguishable from a crash.

**Fix:**
1. In `main.rs:210-224`, collect current peer server IDs before pushing the drain directive, then `drain_directives.push(peers)`.
2. Push a `CoordClientMsg::Draining` through `coord_outbound` so the coordinator calls `drain_server()` and broadcasts consistent targets.
3. Block on `ShutdownRequested` instead of a fixed 3-second sleep so we wait for drain completion or timeout.

---

### 076 — Coordinator rejects re-register for 15 s after crash
**Severity: HIGH · Category: [bug]**

**Files:** `crates/coordinator/src/registry.rs:63-74`; `crates/coordinator/src/main.rs:138-247`

`Registry::register()` rejects a re-registration when `existing.reply_tx.is_closed()` is false. But `reply_tx` is an `mpsc::unbounded_channel::<String>()` whose receiver is held by the in-process `sink_task` (main.rs:149-155). That task only exits when `ws_sink.send(...)` returns `Err` — which only happens when the kernel detects the dead peer. On `kill -9` or power-off, TCP gives no FIN; without SO_KEEPALIVE (not set anywhere), the kernel may hold the half-open state for minutes to hours. The mpsc channel stays open because sink_task is waiting in `reply_rx.recv().await`.

Meanwhile the restarted sim-server connects with the SAME `server_id`. Coordinator calls `register()`, sees `existing.reply_tx` not closed, logs `"S{server_id} already registered with live connection — ignoring duplicate"` and returns early. **The new connection keeps running** but was never inserted; `heartbeat()` updates the OLD entry's `last_heartbeat` because the server_id matches, so heartbeat-based crash detection never fires.

The only escape is when the OLD TCP connection's mpsc sink finally fails — which requires the keepalive broadcast (registry.rs:529-535, every 5 s) AND the kernel surfacing a write error.

Mirror bug: the new connection thinks it succeeded (no reject signal is sent back), so it spams heartbeats to a ghosted entry indefinitely.

**Why it matters:** Operator restarts a sim-server. Other servers can't see it for up to 15 s (or longer). Client `/assign` routes to the OLD entry. New server is a silent zombie.

**Fix:**
1. Make `register()` prefer the NEW connection and aggressively close the old one. Matches session_id semantics (newest wins).
2. Add TCP SO_KEEPALIVE on the coordinator's accept path so half-open detection happens in seconds, not hours.

---

### 077 — Graceful-drained server never broadcasts `CoordClientMsg::Draining`
**Severity: HIGH · Category: [bug]**

**File:** `crates/sim-server/src/main.rs:210-224`

`CoordClientMsg::Draining { server_id }` tells the coordinator "I'm going away, please send me a DrainDirective with peer targets." The coordinator's dispatch at main.rs:201-203 routes it to `registry.drain_server(sid)`, which broadcasts `DrainDirective { target_servers }`. This is the *designed* drain flow.

But the sim-server never sends `Draining` — grep confirms no producer. Instead, main.rs pushes an empty drain directive directly to its local queue (dead end per 075). Even if 075 were fixed to locally synthesize targets, the coordinator would not know the server is leaving, would not stop assigning new clients via `/assign`, would not remove it from `PeerList`, and would not halt rebalance from/to it.

**Why it matters:** New clients connect to a draining server; peers keep forwarding inputs to it; rebalance directives still target it.

**Fix:** In `main.rs` ctrl_c branch, push a `CoordClientMsg::Draining { server_id }` into `coord_outbound` before sleeping. Then separately fix 075 so the drain system actually runs.

---

### 078 — Peer-only straggler packet thrashes `peer_session_resets`
**Severity: HIGH · Category: [bug]**

**File:** `crates/sim-server/src/receiver.rs:38-85`

`note_session` unconditionally trusts the latest-arriving `session_id` as the new current session (comment at lines 38-43 acknowledges this). Scenario:

1. S2 is on `session_a`. S1 has `session_of_peer[S2] = session_a`.
2. S2 restarts to `session_b`. S1 receives a chunked update with `session_b` → session_a dropped, all session_a reassembly entries cleared, shadow `last_peer_tick` zeroed.
3. Old-session-a straggler UDP arrives (delayed by kernel buffer, WiFi retransmit, etc). `note_session` detects `prev=session_b, incoming=session_a != session_b`, pushes another `PeerSessionReset`, flips `session_of_peer[S2]` back to `session_a`. **Every session_b shadow state just seeded gets zeroed.**
4. The next session_b packet arrives and flips again. Shadow cleanup at `shadow.rs:14-31` and `inbound.rs:73-94` have to re-bootstrap everything for the Nth time.

The session reset walks every shadow with `AuthorityServer == server_id` (inbound.rs:29-44) on every flip. With thousands of shadows and out-of-order packets, this is self-imposed DoS.

**Why it matters:** Rolling redeploy or UDP reorder on WiFi can flip sessions many times per second. Logs spam session-change warnings, shadows continuously re-bootstrap, `chunk_evictions` spike, metrics lie.

**Fix:** Add a "seen sessions" epoch set per server_id. Once a peer moves OFF session X, any future packet with session X is treated as a straggler and silently dropped. Use a small LRU (last 2-4 sessions per peer) to tolerate multiple rapid restarts without reviving ancient sessions.

---

### 079 — Coordinator client connects with plain `ws://` even in TLS deployments
**Severity: MEDIUM · Category: [bug]**

**File:** `crates/sim-server/src/coordinator_client.rs:22-33`

`run_coordinator_client` hard-codes `let url = format!("ws://{coordinator_addr}");`. There is no TLS code path for the coordinator control channel. On the UNO Q board deployment (`start-unoq.sh` uses TLS for WSS and HTTPS), the coordinator accepts HTTPS on 7871 but the server-to-coordinator control channel on 7870 is plain WebSocket. The sim-server's `--tls-cert`/`--tls-key` only affect its *client-facing* WS listener.

**Why it matters for churn:** Mixed TLS/plaintext deployments are fragile. If the operator moves the coordinator behind a TLS-only LB (natural hardening step), the sim-server loses all coordination — no PeerList, no RecoverEntities, no rebalance — and the mesh silently degrades.

**Fix:** Add `--coordinator-tls` flag. When true, use `tokio_tungstenite::connect_async` on a `wss://` URL. Pass the same `--tls-cert` as trust anchor for self-signed verification.

---

### 080 — Rebalance never targets Player entities
**Severity: HIGH · Category: [pitfall]**

**File:** `crates/sim-server/src/systems/transfers.rs:175-191`

`process_transfers` collects only NPC candidates: `if kind.0 == EntityType::Npc && world.get::<TransferOutbound>(entity).is_none()`. Players can never rebalance off a loaded server — the coordinator counts Player as weight 3 for rebalance decisions (game_loop.rs:325-337) but rebalance only moves NPCs. If one server accumulates all players (because `/assign` picked it at low load and everyone piled on), the coordinator keeps firing rebalance directives but the heavy server can't shed Players. Rebalance flaps forever: directive fires, a few NPCs move, weighted load barely shifts, next 5-second interval fires again.

Combined with 074 (client never re-assigns), a single busy server can't shed load at all after enough Players connect.

**Why it matters:** The whole "dynamic mesh" story rests on players being able to migrate. Today migration is NPCs-only, which defeats the purpose in any player-heavy workload.

**Fix:** Once client re-assignment (074) exists, extend `process_transfers` to include Players. Send the affected client a `ServerMessage::Reconnect { url: target_ws_url }` *before* sending the UDP `AuthorityTransfer`, so the client migrates transparently. Needs coordinator to look up target server's WS URL and include it in `RebalanceDirective`, or source to look it up from its own `PeerAddresses` + registered scheme.

---

### 081 — Session reset drops `peer_health` counters mid-window
**Severity: MEDIUM · Category: [bug]**

**File:** `crates/sim-server/src/receiver.rs:67-73`

`note_session` clears `ph.latest_tick`, `ph.packets_received`, `ph.chunk_evictions` on session change. The game-loop metrics drain at `game_loop.rs:621-627` also zeroes `packets_received` and `chunk_evictions` on the 5-second reporting window. When the two coincide, the external consumer sees zeros and can't distinguish "peer silent" from "peer just restarted, counters reset, next packet not yet arrived." The monitoring dashboard's `packets_received` rate jumps around because the reset happens asynchronously relative to the /metrics poll.

Design-013 side effect not captured in the design doc's failure-modes section.

**Why it matters:** Dashboards and automated alerting during churn events will be noisy and confusing.

**Fix:** Either leave the counters alone on session change (residue clears in ≤5 s anyway) or emit a `peer_health[sid].session_changed_at_tick` field so operators can distinguish "fresh session, no data" from "stale peer."

---

### 082 — `session_of_peer` and `latest_tick` HashMaps grow unbounded
**Severity: MEDIUM · Category: [bug]**

**File:** `crates/sim-server/src/receiver.rs:116-119, 141`

The receiver maintains two state maps (`session_of_peer: HashMap<u16, u64>`, `latest_tick: HashMap<(u16, u64), u64>`). Neither is pruned when a peer departs. The `latest_tick` map is especially leaky: every restarted peer creates a new `(server_id, session_id)` key that's never removed — even after the session has been fully superseded.

Finding #068 (archived) already flagged the accumulator version of this problem; this is the same bug but in receiver's private maps which are invisible to metrics, so harder to spot operationally.

**Why it matters for churn:** In a long-lived production mesh with regular restarts/rolling deploys, maps grow slowly forever and eventually the sweep iteration (receiver.rs:133-147) costs O(N) where N = lifetime restart count, not active peer count.

**Fix:** On session change, remove `latest_tick[(sid, old_session)]` (already done at receiver.rs:64). Additionally, on a PeerList update that drops a server from the known peer list, clear its entries from both maps. The receiver doesn't currently see PeerList updates — add a small control channel or a shared `HashSet<u16> active_peers` that the game loop updates after processing coord events.

---

### 083 — Drain timeout (30 s) vs Ctrl-C sleep (3 s)
**Severity: MEDIUM · Category: [bug]**

**Files:** `crates/sim-server/src/main.rs:220-223`; `crates/sim-server/src/systems/drain.rs:9, 108-121`

`DRAIN_TIMEOUT_TICKS = 600` (30 s). `tokio::time::sleep(Duration::from_secs(3))` on Ctrl-C (main.rs:222). Not coordinated. Even if 075 is fixed and drain starts properly, the process returns from `main` after 3 s and tears down the tokio runtime, killing the game loop mid-transfer. Any entity still in `TransferOutbound` state when the runtime shuts down is dropped — source never retries (process gone), target never receives the AuthorityTransfer UDP (sender task cancelled), entity vanishes.

Layered on top of archived finding #050 (sender task drain cancelled on runtime shutdown) — 083 makes it user-triggerable, not just theoretical.

**Why it matters:** Operator types Ctrl-C expecting graceful drain. `DRAIN_TIMEOUT_TICKS` advertises 30 s. Actual window is 3 s.

**Fix:** Replace `sleep(3s)` with a loop that polls `ShutdownRequested` (via shared mutex) or a `oneshot` set from inside the game loop when drain completes, with a 30-35 s ceiling to match `DRAIN_TIMEOUT_TICKS`.

---

### 084 — Heartbeat timeout vs re-register race destructively promotes shadows
**Severity: MEDIUM · Category: [pitfall]**

**Files:** `crates/coordinator/src/registry.rs:165-174`; `crates/sim-server/src/coordinator_client.rs:178-180`

Graceful restart sequence:
1. S2 exits (process gone, TCP half-open). Coordinator doesn't know yet.
2. Coordinator keeps sending keepalives to the dead reply_tx (every 5 s).
3. 15 s heartbeat silence → `detect_dead_servers` fires, `unregister(sid)` called, `broadcast_recovery(sid)` fires.
4. Meanwhile, restarted S2 reconnects every 2 s. If registration arrived during the 15 s window, 076 prevented it.
5. When the 15 s expires and the old entry is unregistered, the NEXT re-register attempt (2 s later) succeeds — but meanwhile the coordinator broadcast RecoverEntities. Survivors have already promoted S2's shadows to authoritative. When S2 reconnects, it has zero authority but may try to spawn initial_npcs → duplicates.
6. `broadcast_peer_list` adds S2 back. Peers open UDP. S2 replicates — but its entities don't exist anymore. Peers see S2 as a zombie with 0 authoritative entities; `/assign` sees it as least-loaded and routes new clients there.

**Why it matters:** Restart detection via heartbeat timeout is too coarse to distinguish "dead" from "restarting"; recovery path is destructive.

**Fix:** When a server reconnects with the SAME server_id before the heartbeat timeout fires, `Registry::register` should detect `existing.reply_tx != new_reply_tx` and prefer the new connection. On successful re-register: do NOT broadcast RecoverEntities. If survivors already adopted entities, send a PeerList with a flag signaling "late re-register, skip initial_npcs."

---

### 085 — `initial_npcs` duplicates on restart
**Severity: MEDIUM · Category: [bug]**

**File:** `crates/sim-server/src/game_loop.rs:186-201`

Every sim-server process boot spawns `initial_npcs` (default 20) at startup — fresh UUIDs, spatially scattered. If S2 crashes and survivors adopt its entities via RecoverEntities, those entities now have `AuthorityServer = survivor_id`. When S2 restarts, it spawns a fresh 20 NPCs unrelated to its old ones. The world grows by 20 NPCs per crash-recovery cycle.

On rolling redeploy, each server spawns 20 new NPCs on top of whatever it adopted — effectively doubling NPC count per rolling deploy.

**Why it matters:** Entity count grows unbounded under regular churn, crossing WiFi saturation thresholds much earlier than predicted.

**Fix:** Gate `initial_npcs` spawn on "first-ever run of this cluster" — only spawn if coordinator reports 0 servers on registration (operator-controlled bootstrap), or make it a CLI flag deployment tooling sets only on cold-start.

---

### 086 — `ServerMessage::Reconnect` exists, is tested, and documented — but is never sent
**Severity: HIGH · Category: [overcomplication]**

**Files:** `crates/shared/src/protocol.rs:16`; handler at `client/src/network.ts:272-278`; docs at `docs/true-mesh-architecture.md:38`, `.claude/rules/llm-directions.md:86`

The `Reconnect` variant is referenced in CLAUDE.md/docs as the flagship player-mobility primitive. `grep` shows NO code path sends it. Its handler exists in the client to close the WebSocket (which then fails to reconnect anyway — finding 074). `true-mesh-architecture.md:38` flatly says "Reconnect { url } message implemented... the client receives a reconnect instruction and migrates to the new server" — this is false.

Worst kind of overcomplication: code that claims to solve a real problem (player migration), has tests and docs, but actually doesn't. Load-bearing in the mental model yet bears no load.

**Fix:** Either (a) remove the variant, handler, test, and false docs claims, OR (b) wire it up (finding 080 is the wire-up path). Option (b) is the right move. Option (a) is acceptable as a stopgap to prevent reader confusion.

---

### 087 — Bootstrap replication has three nearly-identical phase-filtering paths
**Severity: MEDIUM · Category: [overcomplication]**

**File:** `crates/sim-server/src/systems/replication.rs:97-120, 203-236`

`prepare_replication` has THREE code paths that compute phase-filtered entity sets:
1. Lines 97-120: "no shadows at all" bootstrap
2. Lines 176-196: "peer has no players, use PeerBounds fallback"
3. Lines 203-236: "some peers have no shadows on this server" bootstrap

Paths 1 and 3 overlap. Under thrash (session reset dumping shadows per 078), the system oscillates between path 1 and path 3 tick-by-tick. The phase filter `(e.id.0.as_u128() % 4) as u64 == current_phase` appears three times in this function plus once in `game_loop.rs:219`. Each iteration allocates a new `Vec<EntityState>`.

**Why "delete this":** 200+ lines with three Vec allocations per tick and overlapping branches. A single pass that classifies each entity into `(peers_hot, peers_cold, peers_bootstrap)` per peer would be simpler, faster (one iteration), and eliminate bootstrap-vs-normal oscillation. The three paths are artifacts of bolt-on fixes for archived findings #058/#059/#064.

**Fix:** Rewrite `prepare_replication` as a single-pass classifier. Collapse ~100 lines to ~40, remove "overlapping early returns" bug class.

---

### 088 — `DrainInfo.started_tick == 0` sentinel collides with real tick 0
**Severity: MEDIUM · Category: [overcomplication]**

**Files:** `crates/sim-server/src/resources.rs:140-147`; `crates/sim-server/src/systems/drain.rs:14-30`

Complexity-audit 6.3 flagged `DrainState.draining + started_tick + targets` redundancy. The fix collapsed it into `DrainState(pub Option<DrainInfo>)` — but `DrainInfo` still carries `started_tick: u64` that is meaningful ONLY if `!= 0`, and the special value 0 means "just activated, not yet stamped." drain.rs:25-29 stamps it on first run.

If drain ever starts at tick 0 (SIGINT before first tick increments), the "already stamped" check at drain.rs:25 mis-fires and the timeout logic at drain.rs:109-120 can't distinguish "just started at tick 0" from "never started."

**Fix:** `pub struct DrainInfo { targets: Vec<ServerId>, started_tick: Option<u64> }`; update drain.rs:25-29 to use `None` → `Some(tick)`. ~10 line change.

---

### 089 — Random `session_id = 0` collides with "never seen" sentinel
**Severity: LOW · Category: [pitfall]**

**Files:** `crates/sim-server/src/main.rs:75`; `crates/sim-server/src/game_loop.rs:571`; `crates/sim-server/src/metrics.rs:122`

Probability ~1 in 2^64. If it happens, the peer will think our packets carry "session unknown" and treat us inconsistently. The `.unwrap_or(0)` at game_loop.rs:571 means the dashboard can't distinguish "peer has session 0" from "peer never sent a packet."

**Fix:** When generating, if `sid == 0` regenerate (trivial). Or use `NonZeroU64`.

---

### 090 — Shadow cleanup runs AFTER `prepare_client_snapshot`
**Severity: LOW · Category: [bug]**

**File:** `crates/sim-server/src/schedule.rs:28-29`

```rust
systems::prepare_client_snapshot.after(systems::prepare_replication),
systems::shadow_cleanup.after(systems::prepare_client_snapshot),
```

Shadow passes staleness check at tick N, snapshot includes it, client renders it. Tick N+1 shadow is despawned. Client gets delta with removal at tick N+1. Entity "dies" visibly one tick late. Minor on its own, but under churn (peer crashes, all shadows expire simultaneously 15 s later) there's a synchronized visual blip.

**Fix:** Move `shadow_cleanup` to run BEFORE `prepare_client_snapshot`. Test at schedule.rs:308-337 (`stale_shadow_included_in_snapshot_then_cleaned`) hard-codes the current ordering — update test to assert the opposite.

---

### 091 — Rebalance threshold blocks small-mesh rebalancing
**Severity: MEDIUM · Category: [pitfall]**

**File:** `crates/coordinator/src/registry.rs:401-406`

```rust
if diff < 10 || (diff as f32) < avg * 0.2 {
    return false;
}
```

At small scales, rebalance never fires. 2-server mesh with 3 players on S1 and 0 on S2: weighted diff = 9, threshold = 10, never triggers. A freshly started sim-server sits at 0 load indefinitely as long as the other servers are under 10 entities total — the exact state during startup. Combined with 080 (players never rebalance), the mesh is stuck.

**Fix:** Scale minimum threshold by average. For `avg < 50`, use `diff >= max(2, avg * 0.3)`.

---

### 092 — Delta compression machinery fires fallback every tick because NPCs always move
**Severity: LOW · Category: [overcomplication]**

**Files:** `crates/sim-server/src/systems/snapshot.rs:11-14`; archived finding #027

The delta framework has a fallback-to-full path specifically because "NPC movement every tick defeats delta compression." NPCs move at `NPC_SPEED = 2.0` with constant velocity — they produce diffs every tick by design. The fallback says "if >80% changed, just send full" — which is true every single tick in practice. The delta system does useless work every tick (compute baseline, compute diff, detect it's large, discard, rebuild full snapshot). Archived finding #049 partially fixed this with a pre-loop probe, but the over-engineering remains: **delta compression is off by default for the common workload, but all the delta machinery still runs**.

**Why "delete this":** Rip out the delta code path until a workload emerges that benefits (stationary inventory, dormant players). The per-client baselines (archived #019 memory, #020 CPU) and 80% probe complexity go away. If/when a workload benefits, add deltas back with the probe proving their value.

**Fix:** Either rip out delta, or add a hysteresis counter: if >90% of recent frames fell back to full, stop computing deltas for N seconds.

---

## Files referenced

- `client/src/network.ts` (074, 086)
- `client/src/main.ts` (074)
- `crates/sim-server/src/main.rs` (075, 077, 083, 089)
- `crates/sim-server/src/coordinator_client.rs` (079)
- `crates/sim-server/src/receiver.rs` (078, 081, 082, 089)
- `crates/sim-server/src/systems/drain.rs` (075, 083, 088)
- `crates/sim-server/src/systems/transfers.rs` (080)
- `crates/sim-server/src/systems/replication.rs` (087, 092)
- `crates/sim-server/src/systems/snapshot.rs` (092)
- `crates/sim-server/src/game_loop.rs` (081, 085, 089)
- `crates/sim-server/src/schedule.rs` (090)
- `crates/sim-server/src/resources.rs` (088)
- `crates/coordinator/src/registry.rs` (076, 084, 091)
- `crates/coordinator/src/main.rs` (076, 079)
- `crates/shared/src/protocol.rs` (086)
- `docs/true-mesh-architecture.md` (086 — false claim about Reconnect)
- `.claude/rules/llm-directions.md:86` (086 — same false claim)
