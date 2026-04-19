# Work Items 021: Peer-Restart Edges

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 5).
Audit findings: `docs/mesh-auditor/074-churn-audit-summary.md`
(078 + 081 + 082 + 084 + 085 + 089).

This batch cleans up the remaining sharp edges around peer restart and
rolling redeploys. None of these block user-visible churn behavior today
(Batches 1-4 already cover that); they show up as log noise, slow leaks,
or rare zombies. Land them after the main correctness work to avoid
masking regressions.

**Open-question resolutions:**

- **Q4 (how receiver learns about PeerList updates for pruning, finding 082):**
  use a shared `Arc<RwLock<HashSet<u16>>>` called `active_peers`. Game
  loop writes on each PeerList event. Receiver reads on its periodic
  sweep to prune entries for departed peers. `RwLock` over `Mutex`
  because reads are frequent (each sweep) and writes are rare (each
  PeerList event).

- **Q5 (`initial_npcs` gating, finding 085):** **CLI flag
  `--cold-start`**. The coordinator is stateless by design per
  CLAUDE.md; adding a "recently-departed server_ids" map would violate
  that principle. Deployment tooling (`start-unoq.sh`, `start.sh`) is
  the natural place to know "is this a fresh bootstrap or a service
  restart". Default to `false` — services that want initial NPCs must
  opt in. `start.sh` / `start-unoq.sh` set the flag on cluster
  bootstrap.

Implement in order. Items are independent enough to re-order under
scope pressure, except WI-213 which depends on WI-212's `active_peers`
plumbing.

---

### WI-210: `session_id = 0` collision fix (finding 089)

**Size:** trivial
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/main.rs:75`

**Description:**

Today `let session_id = rand::random::<u64>();`. 1-in-2^64 probability
of returning 0, which collides with the "never seen" sentinel at
`game_loop.rs:571` and `metrics.rs:122` where `.unwrap_or(0)` means "no
data yet."

Fix: regenerate until nonzero:
```rust
let session_id = loop {
    let s = rand::random::<u64>();
    if s != 0 { break s; }
};
```

**Tests:** none needed (the probability is too low to test meaningfully;
a unit test would either flake at 1/2^64 or be a tautology).

**Done when:** `cargo build -p sim-server` succeeds.

---

### WI-211: Session straggler LRU (finding 078)

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/receiver.rs`

**Description:**

Today `note_session` trusts the latest-arriving `session_id` as the
new current session even when it's an OLD session (a delayed UDP packet
from before a restart). That triggers a `PeerSessionReset` which
destroys fresh shadow state. Under WiFi reorder during a rolling
deploy, the same peer can flip sessions multiple times per second.

Fix: add a small LRU of recently-seen sessions per peer. If an
incoming session is in the LRU but is NOT the current session, it's a
straggler — silently drop the packet rather than trigger a reset.

1. Add a new receiver-local state alongside `session_of_peer`:
   ```rust
   // Per-peer LRU of recently-seen session_ids. Capacity 4. Front is
   // newest; back is oldest. Does NOT include the current session
   // (which lives in session_of_peer).
   let mut session_history: HashMap<u16, VecDeque<u64>> = HashMap::new();
   const SESSION_HISTORY_CAPACITY: usize = 4;
   ```

2. In `note_session`:
   - When a packet arrives with a session_id that differs from the
     current `session_of_peer[server_id]`:
     - Check `session_history[server_id]`. If the incoming session is
       in there, it's a straggler from a fully-superseded session.
       Return early WITHOUT flipping the session. Increment a new
       counter `peer_health.stragglers_dropped[server_id]` for
       observability. Drop the packet (caller returns, packet not
       processed).
     - If NOT in history, this is a genuine session change. Push the
       CURRENT session (the one being replaced) to the front of
       `session_history[server_id]`, trim to capacity, then proceed
       with the existing reset logic.

3. `note_session`'s signature currently returns nothing and implicitly
   performs a session change. Return a `bool` indicating "straggler,
   drop packet" vs "proceed." Callers check the bool.

   Alternative (smaller diff): Return `Option<()>` where `None` means
   "drop this packet." Callers `.?`-check.

4. Add `peer_health.stragglers_dropped: HashMap<u16, u64>` counter.
   Surface via `/metrics` (same treatment as `packets_received`).

**Tests:**
- `session_straggler_in_history_is_dropped`: Seed history with session
  A (current), then bump to session B (current now; A in history),
  then send a packet with session A. Assert no `PeerSessionReset`
  event is pushed and the receiver's current session stays B.
- `session_new_change_overwrites_history`: Seed A, move to B, move to C.
  Assert history has [B, A] (in that order).
- `session_history_capacity_enforced`: Cycle 5 sessions; assert history
  has only 4 entries.
- `session_stragglers_counter_increments`: On each dropped straggler,
  assert `peer_health.stragglers_dropped[sid]` increments.

**Done when:** tests pass; `cargo test -p sim-server receiver` green.

---

### WI-212: Preserve `peer_health` counters across session change (finding 081)

**Size:** small
**Depends on:** nothing (independent of WI-211, but they touch adjacent
receiver code so order WI-211 before WI-212 to keep diffs tidy)
**Files:**
- `crates/sim-server/src/receiver.rs` (`note_session`)
- `crates/sim-server/src/game_loop.rs` (`PeerHealthAccumulator` struct)

**Description:**

Today `note_session` at `receiver.rs:67-73` clears
`peer_health.latest_tick[sid]`, `packets_received[sid]`,
`chunk_evictions[sid]` on each session change. This makes `/metrics`
zero out at restart windows — operators can't distinguish "peer silent"
from "peer just restarted, counters reset, next packet pending."

Fix:

1. Remove the three `.remove(&server_id)` calls at `receiver.rs:69-71`.

2. Add a new field `session_changed_at_tick: HashMap<u16, u64>` to
   `PeerHealthAccumulator`. Populated by `note_session` on each session
   change. Value is the LOCAL current tick (read from wherever the
   receiver has access to it — the receiver doesn't hold the tick
   counter today; the easiest fix is to use
   `std::time::Instant::now().elapsed()` relative to a stored start
   time, or accept "approximate tick" by querying a shared
   `AtomicU64` written by the game loop each tick).

   **Simplest implementation:** add `Arc<AtomicU64> local_tick` shared
   between game loop (writer, once per tick) and receiver (reader, on
   session change). `note_session` reads it and stores in
   `peer_health.session_changed_at_tick[sid]`.

3. Plumb `local_tick: Arc<AtomicU64>` from `main.rs` through
   `run_udp_receiver` and `run_game_loop_ecs`. Game loop writes it in
   the phase-7 vicinity or at the top of each tick.

4. `/metrics` JSON already serializes `peer_health` maps. Add the new
   field to the serialization shape.

**Tests:**
- `session_change_does_not_zero_packets_received`: seed
  `peer_health.packets_received[sid] = 100`, change session, assert
  still 100 (no reset).
- `session_change_records_at_tick`: seed local_tick = 42, change
  session, assert `peer_health.session_changed_at_tick[sid] == 42`.

**Done when:** tests pass.

---

### WI-213: Prune receiver state on departed peers (finding 082)

**Size:** small-medium
**Depends on:** nothing strict (but benefits from the `local_tick`
plumbing pattern in WI-212)
**Files:**
- `crates/sim-server/src/main.rs` (construct `active_peers`)
- `crates/sim-server/src/receiver.rs` (use `active_peers` in sweep)
- `crates/sim-server/src/systems/coordinator.rs` (update `active_peers`
  on PeerList)

**Description:**

Today `session_of_peer` and the receiver's `latest_tick` map grow
unbounded as peers come and go. On each restart cycle, new
`(server_id, session_id)` keys accumulate in `latest_tick`. In a
long-lived production mesh, these maps eventually cost O(N) sweep time
where N = lifetime restart count.

Fix:

1. **Shared resource.** Add an `Arc<std::sync::RwLock<HashSet<u16>>>`
   called `active_peers` in `main.rs`. Pass to `run_udp_receiver` and
   to `run_game_loop_ecs`.

2. **Game loop writes on PeerList.** In
   `systems/coordinator.rs`, inside the `peer_list_updates` loop (lines
   18-39): after updating `PeerAddresses`, also acquire a write lock on
   `active_peers` and set it to the set of `peer.server_id.0` values.
   Clear and repopulate on each PeerList:
   ```rust
   let mut ap = active_peers.write().unwrap_or_else(|e| e.into_inner());
   ap.clear();
   ap.extend(peers.iter().map(|p| p.server_id.0));
   ```
   This requires passing `active_peers` as a resource or closure
   capture. Simplest: add it as a Bevy ECS resource `ActivePeers(pub
   Arc<RwLock<HashSet<u16>>>)` inserted at game-loop construction and
   read by `process_coord_events`.

3. **Receiver prunes on sweep.** In the existing
   `GLOBAL_SWEEP_INTERVAL` branch in `run_udp_receiver`, after the
   reassembly retain, take a read lock on `active_peers`, then:
   - `session_of_peer.retain(|k, _| active_peers.contains(k))`
   - `latest_tick.retain(|(sid, _), _| active_peers.contains(sid))`
   - `session_history.retain(|k, _| active_peers.contains(k))` (from
     WI-211)
   Log the pruned count if nonzero.

4. **Edge case: no PeerList received yet.** On a fresh sim-server
   before the first PeerList arrives, `active_peers` is empty and
   pruning would wipe everything. Gate: only prune if
   `active_peers.len() > 0`. Before first PeerList, sweep is a
   no-op.

**Tests:**
- `receiver_prunes_state_for_departed_peer`: seed `session_of_peer`
  with sids {1, 2, 3}, set `active_peers` to {1, 2}, trigger a sweep
  (can factor the sweep logic into a helper fn for unit testing).
  Assert sid 3 is gone from all three maps.
- `receiver_does_not_prune_before_first_peer_list`: seed
  `session_of_peer` with {1, 2}, `active_peers` empty. Sweep. Assert
  maps unchanged.

**Done when:** tests pass.

---

### WI-214: Suppress `RecoverEntities` on timely re-register (finding 084)

**Size:** small
**Depends on:** Batch 1 (WI-170 already in) — newest-wins register
**Files:**
- `crates/coordinator/src/registry.rs`
- `crates/coordinator/src/main.rs` (decide where to call what)

**Description:**

Today `Registry::register()` (after Batch 1) correctly replaces old
entries with new ones. BUT the 15-second heartbeat-timeout path can
fire `broadcast_recovery` BEFORE the restarted server re-registers,
causing survivors to destructively promote shadows. Sequence:

1. S2 dies. Coordinator doesn't know yet.
2. 15s heartbeat silence → `detect_dead_servers` returns [2].
3. Coordinator calls `unregister(2)` + `broadcast_recovery(2)`.
4. S2 finishes restart + reconnects, re-registers.
5. Survivors already promoted S2's shadows to authoritative; restarted
   S2 has zero authoritative entities + spawns `initial_npcs` (finding
   085 — handled in WI-215).

Fix: when a re-register arrives for a recently-removed server_id
*within a short window* (e.g. 20 s), DO NOT broadcast RecoverEntities
when the heartbeat timeout finally fires.

Two approaches:

**Approach A (simpler, chosen):** Track "recently registered" on
`Registry`. When `register()` replaces or inserts, record
`last_register_at: HashMap<u16, Instant>`. When `detect_dead_servers`
runs, it returns candidates. Before `broadcast_recovery` fires in
main.rs, gate: if the dead server_id was registered within the last
20s (i.e., there's a recent re-register that beat the timeout), SKIP
`broadcast_recovery` and SKIP `unregister`. This is the desired
behavior when the coordinator's heartbeat view was stale but the
server actually did restart.

Actually wait — the heartbeat timeout fires on the OLD entry's
`last_heartbeat`. After Batch 1's newest-wins register, the OLD entry
is replaced (not kept), so `last_heartbeat` was reset to `now` on the
new register. `detect_dead_servers` then doesn't see it as dead.

Let me re-read the scenario... OK so after Batch 1, the specific
scenario in 084 is mostly resolved:
1. S2 dies. Coordinator doesn't know.
2. 15s silence fires on the OLD entry.
3. BUT if S2 reconnected BEFORE 15s, the new register replaced the
   OLD entry with a fresh `last_heartbeat = now`, so `detect_dead_servers`
   would NOT return [2]. No `broadcast_recovery` fires.
4. Survivors continue seeing S2 as live; shadows not promoted; S2
   duplicating initial_npcs is handled by WI-215.

So Batch 1 already fixed the destructive recovery race IF the
re-register happens before 15s. The remaining concern: what if
the re-register comes AFTER 15s (e.g. restart takes 20s)? Then:
1. 15s silence fires on old entry → `broadcast_recovery` → survivors
   promote shadows.
2. S2 reconnects at 20s → fresh register succeeds (newest-wins).
3. S2 starts spawning `initial_npcs` (handled by WI-215).
4. No way to undo the shadow promotion on survivors.

This is less destructive than before Batch 1 (at least S2 joins
cleanly), but still results in some duplicate entities. The full fix
here requires the restarted server to recognize "the cluster adopted
my previous entities" and NOT spawn initial_npcs — which is exactly
what WI-215 does.

**Decision:** close finding 084 as "resolved by Batch 1 + WI-215." The
15s-gated race is inherent to the heartbeat-timeout approach; making it
shorter is out of scope. Tests in Batch 1 and WI-215 cover the cases.
Document the resolution in the worklog.

**Tests:**
- Add `recovery_does_not_fire_if_re_register_beats_timeout`: simulate
  a timed re-register before 15s and assert no RecoverEntities was
  broadcast. Use fake-clock or deliberate short-timeout test helper.

**Done when:** the test passes; worklog notes the resolution.

---

### WI-215: Gate `initial_npcs` on `--cold-start` CLI flag (finding 085)

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/main.rs` (CLI parsing)
- `crates/sim-server/src/game_loop.rs` (initial_npcs spawn)
- `start.sh`, `start-unoq.sh` (pass `--cold-start` on bootstrap)

**Description:**

Today `initial_npcs` (default 20) is spawned unconditionally on every
sim-server boot. On crash-restart or rolling redeploy, survivors have
already adopted the entities → the restarting server duplicates them →
world grows by 20 per cycle.

Fix: add a `--cold-start` boolean CLI flag (default false). Only spawn
`initial_npcs` when true.

1. **CLI flag.** In `main.rs`, add to the `Args` struct:
   ```rust
   /// Spawn initial NPCs on boot. Only set on fresh cluster bootstrap,
   /// not on service restarts — avoids duplicating NPCs already adopted
   /// by surviving peers.
   #[arg(long, default_value_t = false)]
   cold_start: bool,
   ```

2. **Conditional spawn.** At `game_loop.rs:186-201` (the
   `initial_npcs` spawn block), gate on the flag. The `cold_start` value
   flows via `Args` → `run_game_loop_ecs` signature → ECS resource
   `ColdStart(pub bool)` read at spawn time. Simpler: pass `cold_start:
   bool` directly as a parameter to `run_game_loop_ecs`; game loop uses
   it once at init.

   If `cold_start == false`, skip the spawn entirely. Log at info:
   `"cold_start=false, skipping initial NPC spawn"`.

3. **Update `start.sh` and `start-unoq.sh`** to pass `--cold-start`
   when they do the initial cluster launch. Systemd-restart paths (if
   any exist) do NOT pass the flag. For `start-unoq.sh`, the
   `POST /api/mesh/start` path of the board dashboard also needs a
   conditional — check `crates/board-dash` code for this.

4. **Default at `20`** still applies when `cold_start=true`:
   `--initial-npcs 20 --cold-start` spawns 20. Without `--cold-start`,
   `--initial-npcs 20` is ignored (logged as "ignored, cold_start=false").

**Tests:**
- `cold_start_false_skips_initial_npc_spawn`: construct a game loop
  with cold_start=false and initial_npcs=20, tick a few times, assert
  zero NPCs authoritatively present.
- `cold_start_true_spawns_initial_npcs`: cold_start=true,
  initial_npcs=5, assert 5 NPCs exist after first tick.

**Done when:**
- Tests pass.
- `./start.sh` passes `--cold-start` on first launch (updated shell script).
- `./start-unoq.sh` passes `--cold-start` on first launch (updated shell script).
- Board dashboard's mesh-start path either also passes `--cold-start`
  (if "start" means "bootstrap") or omits it (if "restart"). Decide
  based on semantics of the button.

---

### WI-216: Board verification

**Size:** small
**Depends on:** WI-210 through WI-215
**Files:** none (verification only)

**Description:**

Deploy via `./start-unoq.sh`. Execute the following checks:

1. **Rolling redeploy session thrash eliminated (WI-211):**
   - `./start-unoq.sh` from clean slate (all three boards restart).
   - Watch `~/mesh/sim*.log` on zzttpi for `peer session changed`
     lines.
   - **Expected:** each peer pair emits AT MOST one session-change
     log per deploy — no thrash from straggler packets.

2. **Peer_health counter continuity (WI-212):**
   - During a rolling redeploy, curl `/metrics` on each sim-server
     every second. Track `peer_health[sid].packets_received`.
   - **Expected:** the counter does not zero on restart — it either
     stays flat (peer silent) or continues incrementing from its
     pre-restart value. `session_changed_at_tick` advances on restart.

3. **Receiver state pruning (WI-213):**
   - Start a 3-server cluster. Shut down S3 entirely (not restart —
     just stop).
   - Wait for the coordinator heartbeat timeout + one sweep interval.
   - Dump receiver state via a debug log line or new `/metrics` field:
     `receiver.session_of_peer` should no longer contain server_id 3
     once the PeerList broadcast drops it.
   - **Expected:** map sizes bounded by ACTIVE peer count, not
     lifetime restart count.

4. **initial_npcs no longer duplicates on restart (WI-215):**
   - Fresh cluster bootstrap via `./start-unoq.sh` (with
     `--cold-start`). Note initial total NPC count (e.g. 60 = 3*20).
   - `ssh unoq` → kill sim-server → restart (via systemd or
     board-dash `POST /api/mesh/start`, ensuring `--cold-start` is
     NOT passed on restart).
   - **Expected:** total NPC count stays at 60 (or whatever was there
     before the kill minus drain losses). NOT 60 + 20 = 80.

5. **Regression: Batches 1-4 still work:**
   - Smoke test the full player-migration flow from Batch 4.
   - Confirm no regressions from the new plumbing.

**Done when:**
- All five scenarios pass. Evidence in `worklogs/mesh-dev/`.

---

## Out of scope

- Making `DRAIN_TIMEOUT_TICKS` or `heartbeat timeout` tunable. Fixed
  values are adequate for current scale.
- Adding a "graceful peer departure" message that lets a departing
  server signal `active_peers.remove(sid)` synchronously. Today's
  approach (relying on the next PeerList broadcast) adds a ~1s lag
  before pruning. Acceptable.
- Merging `peer_health.latest_tick` and `session_of_peer` into one map.
  Tempting but they have different lifecycles (session_of_peer updates
  eagerly in the receiver; latest_tick is partially snapshotted by the
  game loop for metrics).
