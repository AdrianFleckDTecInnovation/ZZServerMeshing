# Design 014 — Rebalance In-Flight Transfer TTL

**Status:** Proposed
**Author:** mesh-architect
**Date:** 2026-04-17
**Scope:** coordinator only (no sim-server / wire-protocol change)
**Evidence:** `worklogs/cluster-ops/worklog-20260418-010926.md`
**Target branch:** `dev`

---

## 1. Problem

`Registry::try_rebalance()` in `crates/coordinator/src/registry.rs` excludes a
server from the overloaded-candidate set whenever its
`in_flight_transfers: u32` counter is nonzero:

```rust
// registry.rs L240–243
let overloaded = snaps.iter()
    .filter(|s| s.in_flight == 0)
    .max_by_key(|s| s.weighted_load);
```

The counter is incremented every time a `RebalanceDirective` is emitted
(registry.rs L289–292) and is decremented by exactly one code path:
`record_transfer_result()` (registry.rs L177–194), which runs only when the
source sim-server sends a `CoordClientMsg::TransferResult` back over its
WebSocket.

There is **no timeout, no cleanup on unregister, no cleanup on re-register, no
cleanup on peer-death recovery broadcast**. If any single transfer never
produces a `TransferResult`, the counter is permanently stuck ≥ 1 and that
server becomes invisible to the rebalance algorithm until the coordinator
process restarts.

### Observed failure (UNO Q cluster, 2026-04-17)

`peer-restart-reconvergence` and `peer-restart-twice` scene tests
`SIGKILL`'d S2 repeatedly between 18:00:39 and 18:01:55. During that window
S3 was the most-loaded survivor and the coordinator issued 8
`RebalanceDirective{S3 → S2}` messages. Entity counts show 7 transfers
completed (S2 grew 0 → 7, S3 shrank 18 → 11). At least one of the 8 never
produced a `TransferResult` — either S2 crashed after receiving the
`AuthorityTransfer` UDP packet but before replying, the packet itself dropped,
or the WebSocket `TransferResult` was in flight when S2 was killed and its
reply_tx got flushed.

After 18:01:55 the rebalance stream stopped cold for ~20 minutes while the
cluster sat at S1:28 / S2:8 / S3:10. Thresholds (`diff ≥ 10 &&
diff ≥ avg·0.2`) were well met (diff=20, avg=15.3, 20%·avg=3.07). The only
explanation consistent with the code is that `S3.in_flight_transfers`
(and almost certainly `S1.in_flight_transfers` from earlier untracked rebalances)
were stuck > 0. Operator recovery was a manual `systemctl --user restart
mesh-coordinator` on zzttpi; after the restart, in-memory counters reset to
0 and balancing resumed on the next 5 s tick.

### Why it matters

- **Silent degradation.** The mesh keeps serving clients but stops
  self-balancing. Operators have no signal short of staring at `/servers`.
- **Compounds with churn.** Every future crash or restart cycle adds more
  stuck counters across more servers. The steady state trends toward "no
  server is ever eligible to be the overloaded side" and rebalancing ceases
  entirely.
- **Blocks scene tests.** The very tests that exercise crash recovery are
  what triggered the bug, so CI cannot currently catch it.

---

## 2. Non-goals

- **Not** changing the UDP wire protocol (`PROTOCOL_VERSION = 2` per design
  013 stays pinned). Sim-servers already send `TransferResult`; we only fix
  coordinator-side bookkeeping.
- **Not** redesigning the rebalance algorithm. Thresholds, cadence, and
  entity-selection stay as-is.
- **Not** adding a retry loop for failed transfers. A TTL fire is logged and
  the stuck slot is freed — the next rebalance tick decides fresh based on
  current load.

---

## 3. Design

### 3.1 Data structure

Replace `in_flight_transfers: u32` on `ServerEntry` with

```rust
/// Timestamps of RebalanceDirectives this server has received for which no
/// TransferResult has yet been recorded. Pushed on directive send,
/// removed on TransferResult, and pruned by TTL in prune_stale_transfers().
pub in_flight_transfers: VecDeque<std::time::Instant>,
```

Rationale (vs. the alternatives in the prompt):

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| `Vec<Instant>` | simplest | prune is O(n) re-compact | reject |
| **`VecDeque<Instant>`** | push_back O(1), front-pop O(1), TTL prune is one `while let Some(t) = front() { if t < cutoff { pop } else break }` | slightly more code than Vec | **accept** |
| `HashMap<TransferId, Instant>` | can correlate to specific transfer | `TransferId` doesn't exist in the protocol — `TransferResult` carries `entity_id`+`target_server`, and `RebalanceDirective` is sent with `entity_ids: Vec::new()` (server picks), so correlation is impossible without a protocol change | reject (would require wire change) |

`VecDeque` is ordered by push time (monotonic within a single process), so TTL
pruning is strictly front-pop until the head is fresh — no scanning. `len()`
replaces the old `u32` for the filter check.

**Sizing.** At most one directive per 5 s rebalance interval per server in
steady state; worst-case under a storm, a single server never sees more than
a handful of concurrent outbound transfers (each transfer is one entity). A
capacity of 16 per server is plenty; no pre-allocation needed.

### 3.2 TTL duration

**TTL = 15 seconds.** Justification:

- Rebalance interval is 5 s (`main.rs` L253).
- Typical transfer settles in < 1 s (UDP `AuthorityTransfer` + target ack
  via `TransferResult` on WebSocket — both one hop).
- 99th-percentile tick budget on the Pi Zero W2 is ~30 ms, on UNO Q ~5–10
  ms. Even a two-second GC pause on the source wouldn't stretch a
  successful transfer past a few seconds.
- 15 s is 3× the rebalance interval, so a stuck slot costs at most three
  skipped rebalance cycles before it's reclaimed. In the observed outage,
  the cluster was idle for 20 minutes — a 15 s TTL would have recovered in
  one cycle.
- 15 s matches the existing heartbeat-death timeout (`detect_dead_servers`,
  registry.rs L127), which is a nice coincidence: the same duration we
  consider a server "probably crashed" is the same duration we declare a
  transfer "probably lost". If the source dies, the heartbeat path tears it
  down anyway; if the target dies, the TTL is the only recovery.

**Implementation.** TTL pruning runs opportunistically at the start of
`try_rebalance()` (no new timer needed; rebalance already ticks every 5 s).

### 3.3 Topology cleanup

Three cleanup hooks, all in `Registry`:

1. **`register()` replacing a dead connection** (registry.rs L49–58): clear
   the new entry's `in_flight_transfers`. (Already happens naturally because
   we `insert` a fresh `ServerEntry` with an empty deque — document this
   explicitly.)
2. **`unregister()` / `unregister_if_sender_matches()`**: when server X
   departs, iterate *all other* servers and drain any in-flight entries that
   were destined for X. **Problem**: the current `in_flight_transfers`
   model stores only timestamps — we don't know which target each entry was
   for.

   **Solution**: change the entry type to `(Instant, target: ServerId)`:

   ```rust
   pub in_flight_transfers: VecDeque<(std::time::Instant, ServerId)>,
   ```

   On directive send (registry.rs L290–292), push `(now, ServerId(underloaded_id))`.
   On `TransferResult { target_server, .. }`, pop the oldest entry whose
   target matches (O(n) scan, n ≤ 16 — trivial). On unregister(X), drop
   all entries with `target == X` across all servers' deques.

   **Question:** should we also clear entries with `source == X`? Source
   (the `ServerEntry` owning the deque) is the server that just unregistered,
   so its deque disappears with the entry. N/A.

3. **`broadcast_recovery(dead_server_id)`** (registry.rs L158–173): this
   already runs on crash detection. Piggyback on it — call the same
   "drain entries targeting X" sweep here. Redundant with (2) in the
   normal `Disconnected` → `unregister_if_sender_matches` path, but
   defensive for the `detect_dead_servers` → `unregister` path
   (main.rs L305–310) where `unregister()` is called directly. We can make
   `unregister()` itself invoke the sweep so both paths benefit.

**Answer to the prompt's question:** clear entries whose **target** was the
departed server (its `TransferResult` cannot arrive). Source-side is
automatic because the source's own deque gets dropped with its
`ServerEntry`.

### 3.4 Observability

Three changes, all coordinator-local (no protocol change):

1. **Elevate rebalance skip reasons to `info!`.** Currently the per-tick
   `Rebalance check:` summary is `debug!` (registry.rs L235), invisible on
   the Pi with no `RUST_LOG`. Keep the summary at `debug!` (it fires every
   5 s and would be noisy), but add a *conditional* `info!` line whenever
   a rebalance **would** have fired but was suppressed by the
   `in_flight == 0` filter. Example:

   ```
   INFO Rebalance skipped: S1 overloaded (w28) but has 2 in-flight transfers; next eligible source diff below threshold
   ```

   This gives operators exactly the signal that was missing in the
   worklog — "the algorithm ran and decided not to act, and here's why."

2. **Log TTL fires at `warn!`.** When `prune_stale_transfers()` removes an
   entry:

   ```
   WARN In-flight transfer TTL: S3 -> S2 (age=17.2s) — TransferResult never arrived, freeing slot
   ```

   This is the monitoring hook. If operators see steady TTL fires, it
   signals an underlying transfer-completion bug (not just a one-off crash).

3. **Expose counters on `/servers`.** Extend `get_server_list()`
   (registry.rs L318–336) with `in_flight_transfers` (deque length) per
   server. Add a new top-level key `ttl_prunes_total: u64` (process-lifetime
   counter on `Registry`) to the `/servers` JSON response in `main.rs`
   `serve_servers_list` at L411–414:

   ```json
   {
     "servers": [ { "server_id": 1, ..., "in_flight_transfers": 0 } ],
     "ttl_prunes_total": 3
   }
   ```

   This is strictly additive and is what future diagnosis will lean on. The
   dashboard can surface it in a later pass; for now `curl | jq` is fine.

### 3.5 Crash-safety

All state is in-memory. On coordinator restart the `Registry` is fresh, every
counter is 0, every sim-server re-registers and reports `entity_count` via
heartbeat. Fix is coherent with that: TTL state is also in-memory, lost on
restart (which is the correct behavior — restart is the nuclear reset).

---

## 4. Wire & API surface

- **`PeerMessage` / UDP protocol:** unchanged (`PROTOCOL_VERSION = 2`
  stays).
- **`CoordClientMsg::TransferResult`:** unchanged (same shape:
  `{ entity_id, target_server, success }`).
- **`CoordServerMsg::RebalanceDirective`:** unchanged.
- **Coordinator `/servers` HTTP response:** **additive** — adds
  `in_flight_transfers` per server and `ttl_prunes_total` at the top level.
  Existing clients (dashboard, `/assign` consumers, scene tests) ignore
  unknown fields, so no migration needed.

---

## 5. Failure modes (after fix)

| Event | Pre-fix behavior | Post-fix behavior |
|---|---|---|
| Source sends directive, target crashes before ack | Source stuck forever | TTL fires at 15s, `warn!` log, slot freed; next rebalance tick picks next-best source |
| Target sends `TransferResult` | Source decrements | Source pops matching target entry |
| Source crashes with in-flight transfers | Source entry dropped; other servers' deques unaffected (correct, those were *their* transfers to other targets) | Same |
| Coordinator restart | All counters = 0 | All counters = 0 (same) |
| Stale `TransferResult` arrives after TTL already fired | `saturating_sub(1)` underflows to 0 harmlessly | Pop-by-target finds no match; no-op. Log at `debug!` for visibility |
| Directive sent, target unregisters before ack | Stuck forever | `unregister(target)` sweeps target's entries out of all sources immediately — no TTL wait |

---

## 6. Alternatives considered

1. **Reset to 0 on every rebalance tick.** Simplest possible fix: zero out
   all `in_flight_transfers` at the top of `try_rebalance()`. Rejected —
   defeats the whole point of the counter, which is to avoid stacking a
   second directive on a source that hasn't finished the first. A
   fast-tick sequence of overloads could pile 4–5 directives onto one source
   in one tick.

2. **Wire-level correlation ID.** Add `transfer_id: u64` to
   `RebalanceDirective` and `TransferResult`, store
   `HashMap<transfer_id, Instant>`. Rejected — requires wire-protocol bump,
   the whole point of this design is to stay on protocol v2. Can be
   revisited if a deeper rebalance redesign ever needs it.

3. **Make sim-server periodically broadcast `in_flight` state.** Let the
   coordinator reconstruct ground truth from the sim-servers. Rejected —
   requires wire change and wastes bandwidth on steady-state heartbeats,
   for a problem that's fully solvable coordinator-side.

4. **Exponential back-off instead of hard TTL.** E.g. allow one new
   directive per source regardless of in-flight, but double the cooldown
   per outstanding directive. Rejected — more complex, still needs cleanup
   logic for the actually-stuck case, and the simple TTL already meets all
   requirements.

---

## 7. Testing plan

### 7.1 Unit tests (in `registry.rs` `mod tests`)

- `in_flight_increments_on_directive` — trigger rebalance, assert
  `in_flight_transfers.len() == 1` on the source.
- `in_flight_decrements_on_transfer_result` — call `record_transfer_result`,
  assert deque shrinks.
- `in_flight_ttl_prunes_after_cutoff` — manually backdate a deque entry,
  call `prune_stale_transfers`, assert entry gone and `ttl_prunes_total` = 1.
- `in_flight_preserves_fresh_under_ttl` — half-fresh, half-stale;
  prune leaves fresh alone.
- `unregister_clears_entries_targeting_departed_server` — S1 has an
  in-flight directive to S2; `unregister(2)` empties S1's deque.
- `record_transfer_result_matches_by_target` — push `(now, S2)` and
  `(now, S3)`, record S3 success, assert only S3's entry removed.
- `rebalance_reclaims_after_ttl` — simulate the worklog scenario: fire
  directive, never ack, advance time past TTL, next `try_rebalance`
  succeeds.

### 7.2 Scene test

**New:** `tools/test-suite/scenarios/rebalance-stuck-recovery.ts`

Pattern follows `peer-restart-reconvergence.ts`:

1. Start cluster (local or board), spawn 300 NPCs round-robin.
2. Poll `/servers` until the first `RebalanceDirective` has been issued
   (detect by S3 shrinking or by checking `in_flight_transfers > 0`
   on `/servers` — the new field makes this observable).
3. `SIGKILL` the target server precisely during that window
   (after directive sent, before ack could return).
4. Restart the target.
5. Assert within **25 s** (15 s TTL + 5 s rebalance + 5 s headroom):
   - Source server's `in_flight_transfers` on `/servers` is 0.
   - `ttl_prunes_total` ≥ 1 (the fix fired).
   - Load imbalance resolves (max-min entity diff ≤ 20% of avg).

Done-when: scene test passes locally and on the UNO Q cluster via
`./test-scene.sh`.

### 7.3 Operational verification on UNO Q

Run the existing `peer-restart-reconvergence` + `peer-restart-twice` pair
twice in a row. Before the fix, the second run hangs the rebalancer as in
the worklog. After the fix, both runs complete and `ttl_prunes_total` on
`/servers` may be > 0 (expected — that's the fix doing its job on the tail
of each kill).

---

## 8. Migration

- Single coordinator process, single rolling restart. No sim-server change.
- During the coordinator restart window (~1 s), sim-servers hold their
  registrations open but the assignment HTTP endpoint returns nothing for
  new clients. Existing clients are unaffected.
- After restart, sim-servers re-register as normal. Fresh registry state,
  fix active.

---

## 9. Deliverables & sequencing

See `docs/mesh-architect/work-items/014-rebalance-in-flight-ttl.md`.

Three work items (WI-141 — WI-143), all coordinator-only:

1. **WI-141** — data-structure + TTL + pruning + unregister cleanup (core fix).
2. **WI-142** — logging + `/servers` counter exposure (observability).
3. **WI-143** — scene test `rebalance-stuck-recovery.ts` (regression guard).

WI-141 is the bug fix. WI-142 and WI-143 are independent and can land in
either order after 141.
