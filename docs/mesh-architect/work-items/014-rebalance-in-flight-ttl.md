# Work Items 014 — Rebalance In-Flight Transfer TTL

**Design:** `docs/mesh-architect/designs/014-rebalance-in-flight-ttl.md`
**Evidence:** `worklogs/cluster-ops/worklog-20260418-010926.md`
**Scope:** coordinator only — no sim-server / wire-protocol changes.
**Target branch:** `dev`

All line numbers reference `crates/coordinator/src/registry.rs` as of commit
`3ee536c` unless noted otherwise.

---

### WI-141: TTL + pruning + topology cleanup for in-flight transfers
**Size:** medium
**Depends on:** none
**Files:**
- `crates/coordinator/src/registry.rs` (primary)

**Description:**

Replace `ServerEntry::in_flight_transfers: u32` (registry.rs L22) with

```rust
pub in_flight_transfers: std::collections::VecDeque<(std::time::Instant, ServerId)>,
```

where each entry is `(issued_at, target_server)`. Update every read/write
site accordingly:

1. **Constructor (L73):** initialize to `VecDeque::new()` in the
   `register()` path. No capacity hint; worst-case len ≤ ~16.

2. **Increment on directive send (L289–292):**
   ```rust
   if let Some(entry) = self.servers.get_mut(&overloaded_id) {
       entry.in_flight_transfers.push_back(
           (std::time::Instant::now(), ServerId(underloaded_id))
       );
   }
   ```

3. **Rebalance filter (L240–243):** change `.filter(|s| s.in_flight == 0)`
   to `.filter(|s| s.in_flight_len == 0)` where `in_flight_len` is the
   new `usize` field on the local `ServerSnap` struct (L205–211). Populate
   via `in_flight: s.in_flight_transfers.len()` at L218 — rename the
   `ServerSnap` field if desired for clarity, but a straight `len()`
   substitution is sufficient.

4. **Decrement on TransferResult (`record_transfer_result`, L177–194):**
   replace `entry.in_flight_transfers = entry.in_flight_transfers.saturating_sub(1);`
   with a target-matched pop:
   ```rust
   if let Some(entry) = self.servers.get_mut(&source_server) {
       let target = ServerId(target_server);
       if let Some(pos) = entry.in_flight_transfers.iter().position(|(_, t)| *t == target) {
           entry.in_flight_transfers.remove(pos);
       } else {
           tracing::debug!(
               source = source_server, target = target_server,
               "TransferResult for unknown in-flight slot (likely TTL-reaped already)"
           );
       }
   }
   ```
   `remove(pos)` on a VecDeque is O(n) but n ≤ 16 — trivial.

5. **Add TTL constant and pruning method** on `Registry`:
   ```rust
   /// How long an in-flight transfer may sit before being reaped.
   /// Matches heartbeat death timeout — the same duration we consider a
   /// server "probably crashed" is the same duration we consider an
   /// unacked transfer "probably lost".
   const IN_FLIGHT_TTL: std::time::Duration = std::time::Duration::from_secs(15);

   /// Drop in-flight transfer slots older than IN_FLIGHT_TTL.
   /// Returns the number of slots pruned this call.
   pub fn prune_stale_transfers(&mut self) -> u32 {
       let cutoff = std::time::Instant::now() - Self::IN_FLIGHT_TTL;
       let mut pruned = 0u32;
       for entry in self.servers.values_mut() {
           // VecDeque is push-back ordered, so stale entries cluster at front.
           while let Some((t, target)) = entry.in_flight_transfers.front().copied() {
               if t < cutoff {
                   entry.in_flight_transfers.pop_front();
                   tracing::warn!(
                       source = entry.server_id,
                       target = target.0,
                       age_secs = t.elapsed().as_secs(),
                       "In-flight transfer TTL: TransferResult never arrived, freeing slot"
                   );
                   pruned += 1;
               } else {
                   break;
               }
           }
       }
       self.ttl_prunes_total = self.ttl_prunes_total.saturating_add(pruned as u64);
       pruned
   }
   ```

   Add a process-lifetime counter on `Registry`:
   ```rust
   pub struct Registry {
       servers: HashMap<u16, ServerEntry>,
       ttl_prunes_total: u64,
   }
   ```
   Initialize to 0 in `Registry::new()`.

6. **Call pruning from `try_rebalance()` (registry.rs L201, first line of
   the method):**
   ```rust
   pub fn try_rebalance(&mut self) -> bool {
       self.prune_stale_transfers();
       let now = std::time::Instant::now();
       // ... existing body ...
   }
   ```
   This runs every 5 s (the rebalance interval), which is within a 3× safety
   margin of the 15 s TTL. No new timer required.

7. **Topology cleanup on unregister.** Add a helper:
   ```rust
   /// Drop in-flight entries that targeted the departed server, across
   /// every other source's deque.
   fn clear_in_flight_targeting(&mut self, departed: u16) {
       let target = ServerId(departed);
       for entry in self.servers.values_mut() {
           entry.in_flight_transfers.retain(|(_, t)| *t != target);
       }
   }
   ```
   Call it at the top of `unregister()` (L78) **before** the
   `self.servers.remove(&server_id)` line — otherwise the departed server's
   own entry is still in the map and its own deque gets scanned
   unnecessarily (harmless but wasteful). Also call it at the start of the
   removal branch of `unregister_if_sender_matches()` (L94–99) before
   `self.servers.remove(&server_id);`.

   **Do not** clear entries where `source == departed` — those entries
   live on the departed `ServerEntry` itself and are dropped with it.

**Tests (add to `mod tests` at the bottom of registry.rs):**

```rust
#[test]
fn in_flight_increments_on_rebalance_directive() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    age_all_servers(&mut reg);
    reg.heartbeat(1, 100, 0);
    reg.heartbeat(2, 10, 0);
    assert!(reg.try_rebalance());
    assert_eq!(reg.servers.get(&1).unwrap().in_flight_transfers.len(), 1);
    let (_, target) = reg.servers.get(&1).unwrap().in_flight_transfers.front().unwrap();
    assert_eq!(*target, ServerId(2));
}

#[test]
fn in_flight_decrements_on_matching_transfer_result() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    age_all_servers(&mut reg);
    reg.heartbeat(1, 100, 0);
    reg.heartbeat(2, 10, 0);
    reg.try_rebalance();
    reg.record_transfer_result(1, 2, true);
    assert!(reg.servers.get(&1).unwrap().in_flight_transfers.is_empty());
}

#[test]
fn in_flight_ttl_prunes_stale_entries() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    // Manually inject a stale entry
    reg.servers.get_mut(&1).unwrap().in_flight_transfers.push_back(
        (std::time::Instant::now() - std::time::Duration::from_secs(20), ServerId(2))
    );
    let pruned = reg.prune_stale_transfers();
    assert_eq!(pruned, 1);
    assert!(reg.servers.get(&1).unwrap().in_flight_transfers.is_empty());
    assert_eq!(reg.ttl_prunes_total(), 1);
}

#[test]
fn in_flight_ttl_preserves_fresh_entries() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    // One stale, one fresh
    let deque = &mut reg.servers.get_mut(&1).unwrap().in_flight_transfers;
    deque.push_back((std::time::Instant::now() - std::time::Duration::from_secs(20), ServerId(2)));
    deque.push_back((std::time::Instant::now(), ServerId(2)));
    assert_eq!(reg.prune_stale_transfers(), 1);
    assert_eq!(reg.servers.get(&1).unwrap().in_flight_transfers.len(), 1);
}

#[test]
fn unregister_clears_entries_targeting_departed_server() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    let _rx3 = register_server(&mut reg, 3);
    age_all_servers(&mut reg);
    reg.servers.get_mut(&1).unwrap().in_flight_transfers
        .push_back((std::time::Instant::now(), ServerId(2)));
    reg.servers.get_mut(&3).unwrap().in_flight_transfers
        .push_back((std::time::Instant::now(), ServerId(2)));
    reg.unregister(2);
    assert!(reg.servers.get(&1).unwrap().in_flight_transfers.is_empty());
    assert!(reg.servers.get(&3).unwrap().in_flight_transfers.is_empty());
}

#[test]
fn record_transfer_result_matches_by_target() {
    let mut reg = Registry::new();
    let _rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    let _rx3 = register_server(&mut reg, 3);
    let deque = &mut reg.servers.get_mut(&1).unwrap().in_flight_transfers;
    deque.push_back((std::time::Instant::now(), ServerId(2)));
    deque.push_back((std::time::Instant::now(), ServerId(3)));
    reg.record_transfer_result(1, 3, true); // ack the S3 one
    let remaining = &reg.servers.get(&1).unwrap().in_flight_transfers;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining.front().unwrap().1, ServerId(2));
}

#[test]
fn rebalance_reclaims_eligibility_after_ttl() {
    // Reproduces the worklog scenario: directive fires, never acked,
    // next rebalance tick (post-TTL) makes the source eligible again.
    let mut reg = Registry::new();
    let mut rx1 = register_server(&mut reg, 1);
    let _rx2 = register_server(&mut reg, 2);
    age_all_servers(&mut reg);
    reg.heartbeat(1, 100, 0);
    reg.heartbeat(2, 10, 0);
    assert!(reg.try_rebalance());
    let _ = rx1.try_recv(); // drain the directive

    // No TransferResult ever comes back — normally stuck forever.
    // Backdate the entry past TTL.
    for (t, _) in reg.servers.get_mut(&1).unwrap().in_flight_transfers.iter_mut() {
        *t = *t - std::time::Duration::from_secs(20);
    }

    // Keep imbalance the same; rebalance should now fire again.
    reg.heartbeat(1, 100, 0);
    reg.heartbeat(2, 10, 0);
    assert!(reg.try_rebalance(), "rebalance should be reclaimed after TTL");
    assert!(reg.ttl_prunes_total() >= 1);
}
```

Helper: add a `#[cfg(test)] pub fn ttl_prunes_total(&self) -> u64 { self.ttl_prunes_total }`
accessor on `Registry` so tests can read the counter without making the
field public. (Public getter is added in WI-142 regardless.)

**Done when:**
- `cargo test -p coordinator` passes with all new tests green.
- `cargo clippy -p coordinator -- -D warnings` clean.
- Existing `rebalance_*` tests still pass unchanged.
- `./start.sh && ./test.sh` passes all scenarios locally.
- Deployed to UNO Q boards via `./start-unoq.sh` and `./test-scene.sh`
  clean run.

---

### WI-142: Observable in-flight state — logging + /servers exposure
**Size:** small
**Depends on:** WI-141
**Files:**
- `crates/coordinator/src/registry.rs`
- `crates/coordinator/src/main.rs`

**Description:**

1. **Expose `ttl_prunes_total` as a public getter** on `Registry`:
   ```rust
   pub fn ttl_prunes_total(&self) -> u64 { self.ttl_prunes_total }
   ```

2. **Add `in_flight_transfers` to `get_server_list()` output**
   (registry.rs L318–336): within the `json!({...})` for each server,
   add:
   ```rust
   "in_flight_transfers": s.in_flight_transfers.len(),
   ```

3. **Extend `/servers` HTTP response** in `main.rs` around L411–414:
   ```rust
   let r = registry.read().await;
   let list = r.get_server_list();
   let ttl_prunes = r.ttl_prunes_total();
   drop(r);
   let body = serde_json::json!({
       "servers": list,
       "ttl_prunes_total": ttl_prunes,
   }).to_string();
   ```
   This is strictly additive — existing consumers (dashboard, `/assign`
   callers, scene tests) ignore unknown fields.

4. **Elevate skip-reason logging.** In `try_rebalance()`, just before the
   early returns that discard a rebalance opportunity, add an `info!`
   summary that names the reason. Specifically, after computing
   `overloaded`/`underloaded` and before returning false due to threshold
   or equal-id checks, if the *unfiltered* max-loaded server has
   `in_flight_transfers > 0` AND a rebalance *would* otherwise have fired,
   emit:

   ```rust
   tracing::info!(
       "Rebalance skipped: S{busy} (w{load}) has {n} in-flight transfer(s), deferring",
       busy = busiest.id,
       load = busiest.weighted_load,
       n = busiest.in_flight_len,
   );
   ```

   Implementation sketch: compute `busiest_unfiltered = snaps.iter()
   .max_by_key(|s| s.weighted_load)` before the filter. If the filter
   eliminated it AND the (busiest_unfiltered, underloaded) pair would have
   crossed both thresholds, log the skip. Limit to once per call — no
   per-server logging noise.

   This is the signal whose absence made the worklog diagnosis so painful:
   "the algorithm ran and decided not to act because N transfers are stuck
   on Sx."

5. **Keep the existing `Rebalance check:` line at `debug!`** (L235) —
   it's the noisy per-tick audit and stays off by default. The new
   `info!` line is the actionable one.

**Tests:**

- `in_flight_count_surfaces_in_server_list`: register a server, inject
  an in-flight entry, call `get_server_list()`, assert the JSON contains
  `"in_flight_transfers": 1`.
- `ttl_prunes_total_increments_monotonically`: inject two stale entries,
  prune, assert `ttl_prunes_total() == 2`.
- Manual verification (no automated test needed): run the cluster, inject
  an imbalance, verify the `info!` log fires when a source is busy.
  (A unit test can't easily verify log output without a tracing subscriber
  fixture; keep this verification manual.)

**Done when:**
- `curl -s http://localhost:7871/servers | jq '.ttl_prunes_total, .servers[].in_flight_transfers'`
  returns non-null values.
- Deliberately trigger a skip (inject stuck slot in a dev run) and observe
  the new `info!` line in stdout.
- All new unit tests pass; no regression in existing tests.
- Deployed and verified on UNO Q cluster.

---

### WI-143: Scene test for stuck-rebalance recovery
**Size:** medium
**Depends on:** WI-141 (WI-142 is highly recommended for easier assertions)
**Files:**
- `tools/test-suite/scenarios/rebalance-stuck-recovery.ts` (new)
- `tools/test-suite/index.ts` or wherever scenarios are registered
  (add the new scenario to the export/run list)

**Description:**

Add a scene test that reproduces the worklog scenario and verifies
recovery. Pattern after `peer-restart-reconvergence.ts` for local+board
dual-mode, kill/restart helpers, and metrics polling.

**Flow:**

1. `ctx.spawn(300)` round-robin across S1/S2/S3; sleep 4 s for settle.
2. Poll `/servers` (via `ctx.getAllMetrics()` or direct fetch of
   `COORDINATOR_URL/servers`) until at least one server reports
   `in_flight_transfers >= 1`. Record that server's id as `source`, and
   the target from an inferred heartbeat (whichever peer's entity_count
   is growing). Fallback: wait for the first `Rebalancing:` log line
   (not available programmatically — prefer the `/servers` path using
   WI-142's exposure).
3. Immediately SIGKILL the **target** server. Use the existing
   `killLocal`/`killBoard` helpers copy-pasted from
   `peer-restart-reconvergence.ts`.
4. Sleep 3 s (long enough for the in-flight entry to become "stuck" —
   the target can't ack).
5. Restart the target via `restartLocal`/`restartBoard`.
6. Start a convergence poll with budget **30 s** (15 s TTL + 5 s
   rebalance interval + 5 s rebalance cooldown + 5 s headroom).
7. Assert within the budget:
   - **A:** `source`'s `in_flight_transfers` on `/servers` drops to 0.
     This can happen via TTL expiry (preferred assertion — proves the
     fix works) or via a late `TransferResult` arrival (acceptable —
     also proves correctness).
   - **B:** `ttl_prunes_total` on `/servers` is **≥ 1** at the end of
     the test. This is the strict regression guard: if the TTL never
     fires, the test fails even if recovery happens some other way.
   - **C:** New post-recovery rebalance: after ≥ 20 s from restart,
     check that a fresh `RebalanceDirective` has been issued (detect
     via entity-count movement across servers, or by checking that
     max-min entity diff has decreased from its post-kill peak).

**Failure output:** on any assertion miss, dump the full `/servers`
snapshot at each poll into `details` for post-mortem, matching the
existing scene-test convention.

**Registration:** follow the existing pattern in
`tools/test-suite/scenarios/` — add an `export const rebalanceStuckRecovery: Scenario = { ... }`
and register it in the test-suite's scenario index (check
`tools/test-suite/index.ts` or the nearest equivalent; mirror how
`peer-restart-reconvergence` is wired).

**Done when:**
- `./test-scene.sh --no-start rebalance-stuck-recovery` passes locally.
- Same test run on UNO Q boards (`./test-scene.sh` after
  `./start-unoq.sh`) passes.
- Revert WI-141 temporarily in a scratch branch — the new scene test
  **must fail** (regression confirmed). Re-apply WI-141 — it passes.
  (This is a manual verification step; document the before/after in the
  PR description.)
- Scenario is wired into the default scene-test run so CI catches
  regressions.

---

## Execution order

1. **WI-141** first (core fix, lands the behavior).
2. **WI-142** second (observability — makes WI-143's assertions simpler).
3. **WI-143** third (regression guard, prevents this from recurring).

All three are coordinator-only. No sim-server redeploy needed; only the
coordinator service on zzttpi requires the `systemctl --user restart
mesh-coordinator` (or equivalent local `./start.sh` restart).
