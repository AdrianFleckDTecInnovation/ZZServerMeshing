# Design 016 — Churn-Resilience Plan (Outline)

**Status:** Draft outline. Produces the shape of the fix plan; individual
batches will be fleshed out into full designs + work items once the user
approves the ordering.

**Input:** `docs/mesh-auditor/074-churn-audit-summary.md` (19 findings).

**Scope:** Turn those 19 findings into an ordered, batched delivery plan that
gets the mesh to "no-fuss dynamic churn ready" without re-introducing risk.

---

## 1. Goal statement

After this plan lands, the mesh survives sim-servers dropping in and out —
from crashes, from operator Ctrl-C, from rolling redeploys — **without a human
touching a browser tab**. A crashed or drained sim-server's clients migrate to
a healthy peer automatically. Operator restarts are indistinguishable from a
no-op from the client's point of view. Load imbalance caused by churn self-
corrects within one rebalance interval, including imbalance involving player
entities. The coordinator always prefers the newest live connection from a
given `server_id`, so restarts never produce zombie entries. Observability
through churn is clean: session changes are visible, but counters don't lie
during the transition.

Non-goal: making the mesh infinitely scalable, adding new gameplay features,
or introducing new protocol concepts beyond what's already designed (e.g. the
existing `ServerMessage::Reconnect { url }` gets wired up; we don't introduce
a coordinator-issued migration token).

---

## 2. Ordering principles

The batches are ordered by **unblocking dependency**, not by severity:

1. **Coordinator correctness first (076).** Until `register()` prefers the
   new connection, *every* restart story is broken — 084's destructive-
   recovery race is a direct symptom of 076, and 077's `Draining` message
   can't be trusted because the coordinator might still ghost the old entry.
   Fix the registry first; the server-side drain work becomes a reliable
   collaborator rather than a race with the coordinator.

2. **Client reconnection next (074).** This is the single most visible
   churn failure and it also *blocks* 080 (player migration): there is no
   point wiring `Reconnect { url }` if the client can't fetch a fresh URL on
   its own anyway. 074 is also the natural place to validate 076 end-to-end
   — "crash a server, see the client migrate" is the smallest full-stack
   churn test we can run.

3. **Graceful drain as a self-contained track (075 + 077 + 083 + 088).**
   Operator-initiated shutdown shares the message plumbing (`Draining`,
   `DrainDirective`, `DrainState`, `ShutdownRequested`) and is isolated
   from the client and rebalance paths. This is a coherent, independently
   testable batch.

4. **Player migration (080 + 086 + 091).** Depends on 074 (client
   reconnection) AND 076 (coordinator correctness). 091 (small-mesh
   rebalance threshold) is in this batch because without it, 080's player-
   transfer code path doesn't fire on small meshes — the feature would
   appear broken in testing even though it works.

5. **Peer-restart edges (078 + 081 + 082 + 084 + 085 + 089).** After the
   three core correctness issues are fixed, these are remaining sharp edges
   around rolling redeploys and peer restart. They share receiver state
   (`session_of_peer`, `latest_tick`, `peer_health`) or coordinator state
   (`initial_npcs` duplication, recovery races). None of them block user-
   visible churn behavior; they show up as noise, leaks, or rare zombies.

6. **Overcomplications last (087 + 092 + 090 + 079).** These are the
   "delete this" findings. They should come after correctness because
   collapsing the three-branch bootstrap (087) or removing delta compression
   (092) without first locking in the session-reset and client-reconnect
   semantics risks masking a regression. 079 (coordinator TLS) is in this
   batch because it's hardening, not a churn fix. 090 (shadow cleanup
   ordering) is also here because it's cosmetic.

Each batch produces observable behavior change a human can verify from a
browser or a log. No batch is merged on internal-only changes.

---

## 3. Batches

### Batch 1 — Coordinator: prefer the new connection

**Resolves:** 076
**Size:** S-M

**Approach.** `Registry::register` today silently rejects a re-registration
when `existing.reply_tx.is_closed()` is false — but the mpsc channel stays
open forever under `kill -9` or power-off. Flip the policy: when a
`Register` arrives for a `server_id` that already exists, **treat the new
connection as the live one**. Drop the old `ServerEntry`, close the old
`reply_tx` (so its sink task exits and the stale TCP socket closes), then
insert the new entry. This matches the session-id "newest wins" semantics
already in place for UDP peers and eliminates the 15 s zombie window.

Also add SO_KEEPALIVE on the coordinator's accept path so the kernel
surfaces half-open TCP in seconds rather than minutes. Keepalive is
insurance; the "newest wins" change is the primary fix.

**Dependencies.** None — strictly coordinator-local.

**Acceptance criteria.**
- Kill -9 a sim-server and restart it within 2 s. Coordinator logs a
  "replacing entry, old connection closed" message; new entry is
  visible in `/servers` within one heartbeat (1 s) of the new
  connection. No 15 s zombie window.
- `broadcast_recovery` does NOT fire when a same-`server_id` reconnect
  happens before the heartbeat timeout expires (this is a side effect
  of 084 being fixed too, since the new connection arrives before
  `detect_dead_servers` ever runs).
- Unit test in `registry.rs` that constructs two reply_tx channels for
  the same server_id and asserts the newer one wins.

---

### Batch 2 — Client: re-assign on reconnect

**Resolves:** 074
**Size:** S

**Approach.** Extract the current one-shot `getServerUrl()` in
`client/src/main.ts` into a reusable method on `NetworkClient` that hits
`/assign` each time it is called. In `scheduleReconnect()`, invoke the
fresh-assign call *before* `new WebSocket(...)`. Keep exponential backoff,
but on each backoff iteration refetch the URL — the coordinator decides
which server the client lands on, not the client's stale state. Add a
fallback: if `/assign` fails (coordinator unreachable), retry the last
known URL as a last resort so the client isn't stuck when only the
coordinator blinks.

**Dependencies.** Batch 1 — with 076 fixed, `/assign` always returns a
live server within ~2 s of a crash, so the client's refetch actually finds
a healthy peer. Without Batch 1, the client can re-fetch `/assign` and
still be routed to the ghost entry.

**Acceptance criteria.**
- Crash a sim-server the client is connected to; the client migrates to a
  surviving server within `RECONNECT_DELAY_MS + 1 heartbeat` with no user
  action. Scene test verifies this end-to-end.
- Kill all sim-servers, wait 5 s, bring one back up; client eventually
  connects (exponential backoff + /assign retry).
- Client no longer hammers a dead URL forever.

---

### Batch 3 — Drain works end-to-end

**Resolves:** 075 + 077 + 083 + 088
**Size:** M

**Approach.** Today the drain branch in `main.rs` pushes `vec![]` onto
`drain_directives`, the drain system early-returns on `targets.is_empty()`,
and the process falls out of `tokio::select!` after a hardcoded 3 s sleep.
The fix has three coordinated moves:

1. **Send `CoordClientMsg::Draining` to the coordinator** on SIGINT
   (finding 077). The coordinator's existing handler calls
   `drain_server()` which broadcasts the correct `DrainDirective` back.
   Remove the "push empty targets locally" hack — the coordinator is the
   source of truth for drain targets.
2. **Replace the 3 s sleep with a poll on `ShutdownRequested`** bounded
   by a ceiling that matches `DRAIN_TIMEOUT_TICKS` (30 s). The game loop
   sets `ShutdownRequested` when drain completes or times out; `main.rs`
   waits for it.
3. **Fix the `started_tick == 0` sentinel** (finding 088) by changing
   `DrainInfo.started_tick: Option<u64>` and using `None` → `Some(tick)`
   in the drain system. This is a ~10 line cleanup but it removes a
   latent bug where a SIGINT on tick 0 re-stamps the drain start every
   tick and the timeout never fires.

**Dependencies.** Soft dependency on Batch 1 — the coordinator side of
`Draining` dispatch works today, but the drain *broadcast* to peers
depends on the coordinator not ghosting entries. With Batch 1 done,
drain directives reach the right server the first time.

**Acceptance criteria.**
- Ctrl-C a sim-server with authoritative entities. Scene test observes:
  (a) coordinator logs `Draining` received, (b) coordinator sends
  `DrainDirective` with real targets, (c) source sends `AuthorityTransfer`
  for every authoritative entity, (d) peers ACK, (e) source sets
  `ShutdownRequested` and exits within ~5 s (sub-timeout happy path).
- Peer shadow counts of the drained server's entities do NOT drop to zero
  at drain time — they convert to authoritative on the targets.
- Drain timeout path still works: kill target peer UDP; drain times out
  at ~30 s, source exits, remaining entities vanish (acceptable because
  the entity was marooned).

---

### Batch 4 — Player migration on rebalance

**Resolves:** 080 + 086 + 091
**Size:** M-L

**Approach.** With Batches 1–3 landed, the mesh can actually handle
migrations. Wire up the `ServerMessage::Reconnect { url }` path that has
existed (and been tested, and been documented as working) for months:

1. **Rebalance includes Player entities** (finding 080). Extend
   `process_transfers` in `systems/transfers.rs` so Players are
   candidates, not just NPCs. Sort order can still prefer NPCs first
   (they're cheaper to migrate) but allow Player migration when NPC-only
   transfers can't close the load gap.
2. **Send `Reconnect { url }` to the affected client BEFORE the UDP
   `AuthorityTransfer`** (finding 086). The source server holds the WS
   connection to the client; it constructs the target's ws/wss URL from
   `PeerAddresses` (or coordinator-provided URL on `RebalanceDirective`
   — see open question below), sends `Reconnect`, then sends the
   `AuthorityTransfer` to the target. The client's existing handler
   closes the WS; with Batch 2 in place, the client then refetches
   `/assign` (which at this point will return the target because the
   target just gained weighted load).
3. **Scale the rebalance threshold by mesh size** (finding 091). At
   small scales (avg < 50 weighted units) the current `diff < 10`
   floor blocks any rebalance from firing. Replace with
   `diff >= max(2, avg * 0.3)` for small meshes. Without this, player
   migration doesn't fire on a 2-server 6-player mesh even though the
   code is correct.

**Dependencies.**
- Batch 2 (client re-assign) — without it, `Reconnect` closes the WS
  but the client loops on the old URL. Hard dependency.
- Batch 1 (coordinator correctness) — ensures the client's fresh
  `/assign` returns a real server, not a ghost.

**Open question on scope:** Does the source server construct the target
URL locally (from `PeerAddresses` + known scheme), or does the
coordinator include the URL in `RebalanceDirective`? See Section 5.

**Acceptance criteria.**
- 2-server mesh, S1 has 6 players and S2 has 0. Coordinator issues a
  rebalance directive within 5 s (Batch 4's threshold fix enables this).
  Source sends `Reconnect` to 2–3 players. Clients migrate transparently
  — no user reload. Scene test asserts players appear on S2's
  authoritative set within 2 s of directive.
- Weighted load converges to within the rebalance threshold after one
  full rebalance interval.
- If Batch 4.3's threshold isn't right, the scene test fails visibly
  (no rebalance fires) and we know.

---

### Batch 5 — Peer-restart edges

**Resolves:** 078 + 081 + 082 + 084 + 085 + 089
**Size:** M

**Approach.** Six findings clustered around peer restart / session reset.
All are real but small. They share receiver state and the coordinator
recovery path:

- **078 (session straggler thrash):** Add a small LRU of "recently seen
  sessions per peer" (size 2–4). A UDP packet whose session_id is in the
  LRU but isn't current is silently dropped instead of triggering
  `PeerSessionReset`. Prevents oscillation under WiFi reorder.
- **081 (peer_health counter reset):** Instead of zeroing the counters on
  session change, add `peer_health[sid].session_changed_at_tick` so the
  dashboard can distinguish "stale peer" from "fresh session." Preserves
  counter continuity during churn.
- **082 (unbounded receiver state maps):** On a PeerList update that
  drops a server, prune its entries from `session_of_peer` and
  `latest_tick`. Requires a small plumbing change so the receiver can see
  PeerList updates (shared `HashSet<u16> active_peers` or a control
  channel).
- **084 (destructive recovery race):** With Batch 1 done, same-server_id
  re-registration before heartbeat timeout already preserves the new
  connection. Add the missing half: suppress `RecoverEntities` broadcast
  when the re-register happens within the timeout window, OR attach a
  "late re-register" flag to the subsequent PeerList so the returning
  server knows survivors already adopted its entities and it should NOT
  spawn `initial_npcs`.
- **085 (`initial_npcs` duplicates):** Gate `initial_npcs` on "coordinator
  reports no prior presence of this server_id" — simplest shape is a
  `Registered { cold_start: bool }` reply from coordinator to the first
  `Register` message, set to true only when there was no prior record of
  this server_id in the registry within the last N minutes. Alternative:
  operator-controlled CLI flag set only on first boot.
- **089 (`session_id = 0` collision):** Regenerate on the 1-in-2^64 chance
  it happens. Or use `NonZeroU64`. Trivial.

**Dependencies.** Batches 1–4 should all be in first; these are
polishing the edges after the main correctness work is done.

**Acceptance criteria.**
- Rolling redeploy across 3 servers produces at most 1 session reset per
  peer per deploy (today it can thrash multiple times).
- `peer_health[sid].packets_received` does NOT zero during a session
  change window — `session_changed_at_tick` advances instead.
- Coordinator logs confirm `initial_npcs` does NOT spawn on re-register.
  Scene test kills S2, waits for survivors to adopt, restarts S2,
  asserts total entity count is unchanged.
- Receiver `session_of_peer.len()` bounded by current active peer count
  (not lifetime restart count).

---

### Batch 6 — Simplifications and hardening

**Resolves:** 087 + 092 + 090 + 079
**Size:** M

**Approach.** Pure cleanup and hardening, no new user-visible behavior.

- **087 (three-branch bootstrap):** Rewrite `prepare_replication` as a
  single-pass classifier: for each entity, classify it into a
  `(per_peer_hot, per_peer_cold, per_peer_bootstrap)` tuple in one
  iteration. Collapses ~200 lines to ~60, removes three Vec allocations,
  eliminates the oscillation bug class between path 1 and path 3.
- **092 (delta compression removal):** NPCs move every tick; the 80 %
  fallback fires every tick; the delta pipeline is dead weight. Rip out
  the delta path and the per-client baseline tracking, archive the
  delta-frame type behind a version flag so the client still decodes
  full frames. If a workload emerges that benefits (dormant inventory,
  stationary players), re-introduce with the probe proving its value.
- **090 (shadow cleanup ordering):** Move `shadow_cleanup` to run BEFORE
  `prepare_client_snapshot` in the schedule so stale shadows don't
  appear in snapshots. Update the test that currently asserts the
  (wrong) ordering.
- **079 (coordinator TLS):** Add `--coordinator-tls` flag on sim-server
  and route coord client to `wss://` when set. Pass `--tls-cert` as the
  trust anchor for self-signed verification. No behavior change for
  the Mac dev loop; enables TLS-behind-LB deployment without silent
  degradation.

**Dependencies.** Batches 1–5 locked in. These are refactors and
hardening that should land on a known-good base.

**Acceptance criteria.**
- `cargo bench`-style before/after on `prepare_replication` shows
  ≤ fewer allocations per tick (exact delta quantified during design).
- No regressions in the full scene-test suite, specifically
  `peer-replication`, `multi-client`, `authority-transfer`,
  `rebalance-storm`.
- Dashboard `delta_ratio` metric either goes to 0 or the metric is
  removed alongside the delta path.
- TLS coordinator connection verified on the UNO Q boards.

---

## 4. Out-of-scope / deferred

- **089 session_id = 0 collision** is included in Batch 5 for
  completeness but is a 1-in-2^64 event. Acceptable to punt if Batch 5
  becomes too large.
- **Player persistence** (`docs/player-persistence-design.md`): out of
  scope. `PlayerTransfer` handling in the coordinator is already a
  logged-only placeholder. Player migration in Batch 4 is transparent
  (client just reconnects to a new server) — persistence is the
  *session-spanning* story, which is a separate design.
- **Shadow extrapolation, interest management v2, delta compression re-
  introduction**: not needed for churn. Defer to the roadmap.
- **Horizontal scaling beyond ~10 servers**: out of scope. Current
  designs are known to work at 3–5 servers; the coordinator registry is
  a `HashMap` and all broadcasts are O(peers). Fine for the target.
- **090 shadow cleanup ordering**: included in Batch 6 because it's
  cheap, but strictly speaking it's a cosmetic one-tick visual blip,
  not a churn fix. Could be deferred to the next roadmap iteration
  without blocking churn readiness.

---

## 5. Open design questions

These must be resolved before Batch 4 (at the latest) can be turned into
work items. Other batches don't block on them.

1. **Where does the target URL come from in `Reconnect { url }`?**
   Options:
   - (a) Source server constructs it from `PeerAddresses` + the
     scheme stored when peers were registered (needs the WS port and
     TLS flag to propagate through `PeerInfo` → `PeerList`). Local,
     fast, no coordinator round-trip.
   - (b) Coordinator includes `target_ws_url` in `RebalanceDirective`.
     Cleaner separation, but adds a field to an existing message and
     couples rebalance timing to URL resolution.
   - Recommended: (a), with a fallback — if `PeerAddresses` doesn't
     have a WS URL for the target, skip the player (rebalance only
     moves NPCs in that case, same as today). Coordinator already
     knows each peer's `ws_port`, `host`, and `tls`; we can plumb
     those into `PeerInfo` without breaking the protocol.

2. **Does Batch 3's `ShutdownRequested` poll use a mutex, an atomic bool,
   or a tokio oneshot?** `ShutdownRequested` is already an ECS resource;
   the cleanest plumbing is probably a watch channel (`tokio::sync::
   watch::<bool>`) set from inside the game loop when drain completes.
   User should confirm: is an extra dependency acceptable, or should we
   use an `AtomicBool` polled from `main.rs`?

3. **Batch 4 — does Player migration require client-side state
   preservation?** Today the client reconstructs its view purely from
   server snapshots — migration is transparent if the new server has
   the player's entity. But local-only state (input seq counter,
   camera, pending inputs) needs to survive the WS reconnect. Worth
   confirming the client's `PredictionManager` handles a reconnect
   gracefully, or spec'ing what it does on reconnect.

4. **Batch 5 — how does the receiver learn about PeerList updates for
   state pruning?** Options: shared `Arc<RwLock<HashSet<u16>>>` that
   the game loop writes on each PeerList event; or a small
   `mpsc::channel<PeerListUpdate>` from coord-processor → receiver.
   The game loop already has the PeerList info — plumbing is a
   mechanical choice.

5. **Batch 5 — `initial_npcs` gating: does the coordinator respond to
   `Register` with a `cold_start` flag, or do we add a `--cold-start`
   CLI flag that deployment tooling sets?** The coordinator approach
   is more robust (works for any start path), the CLI approach is
   simpler but pushes the responsibility to whatever wrapper invokes
   the binary. User should pick.

6. **Batch 6 — delta compression: delete outright, or move behind a
   feature flag?** Outright delete simplifies the most. Feature flag
   preserves the option value of re-introducing it later without
   archaeology. The audit (092) recommends outright delete; worth
   confirming we're OK losing the code entirely vs. keeping an
   `cfg(feature = "delta")` skeleton.

---

## 6. Suggested Batch 1 first step

If the user approves this outline and wants to start immediately, the
smallest shippable change that moves the needle is:

> **Change `Registry::register()` in `crates/coordinator/src/registry.rs`
> to prefer the new connection.** When a `Register` arrives for an
> existing `server_id`, drop the old entry (closing its `reply_tx`) and
> insert the new one unconditionally. Keep the TCP SO_KEEPALIVE change
> as a separate follow-up if the scope needs to stay tiny.

That's ~15 lines of Rust plus a unit test asserting the new reply_tx
wins. It fixes 076 (the audit's highest-priority non-client finding),
un-blocks everything else on the server side, and can be shipped and
deployed to the boards in one round trip. It also makes manual testing
of the rest of the plan much easier because `kill -9` + restart stops
being a mystery.

---

## 7. Batch summary table

| Batch | Findings | Size | Blocks | Outcome |
|-------|----------|------|--------|---------|
| 1 | 076 | S-M | 2, 3, 4 | Coordinator never ghosts entries |
| 2 | 074 | S | 4 | Clients survive crashes without reload |
| 3 | 075, 077, 083, 088 | M | — | Operator Ctrl-C drains gracefully |
| 4 | 080, 086, 091 | M-L | — | Players migrate on rebalance |
| 5 | 078, 081, 082, 084, 085, 089 | M | — | Peer-restart edges cleaned up |
| 6 | 087, 092, 090, 079 | M | — | Code simplified, TLS hardening |

Deferred: none of the 19 findings are dropped. 089 and 090 are
low-priority polish items inside their batches and can be split off if
scope pressure demands it.

---

## Next step

User reviews this outline. On approval, each batch gets a full design
doc and ordered work items under `docs/mesh-architect/designs/` and
`docs/mesh-architect/work-items/` respectively, named
`017-coordinator-newest-wins.md`, `018-client-reassign.md`,
`019-graceful-drain.md`, etc. Until then, no work-item files exist and
the plan is advisory.
