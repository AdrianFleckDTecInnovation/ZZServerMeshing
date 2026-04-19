# Work Items 019: Graceful Drain End-to-End

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 3).
Audit findings: `docs/mesh-auditor/074-churn-audit-summary.md` (075 + 077 + 083 + 088).

This batch makes operator `Ctrl-C` a real, observable, bounded drain
instead of a 3-second sleep followed by an abrupt exit. Four coordinated
changes; each WI is independently compilable.

**Open-question resolution (plan section 5, Q2):** use
`tokio::sync::Notify` for drain-complete signaling. It is simpler than a
`watch` channel for the two-state (not-done → done) case, requires no
channel receiver lifecycle management, and is already transitively
available via `tokio` with `full` features. `ShutdownRequested` stays as
an ECS resource for the game loop's internal break condition; the Notify
is the cross-task handshake.

---

### WI-190: `DrainInfo.started_tick` → `Option<u64>` (finding 088)

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/resources.rs`
- `crates/sim-server/src/systems/drain.rs`
- Any test file that constructs `DrainInfo { ..., started_tick: 0 }`

**Description:**

Change `DrainInfo.started_tick` from `u64` (with `0` as "not yet stamped"
sentinel) to `Option<u64>`.

1. In `resources.rs:140-143`, change
   ```
   pub struct DrainInfo {
       pub targets: Vec<ServerId>,
       pub started_tick: u64,
   }
   ```
   to
   ```
   pub struct DrainInfo {
       pub targets: Vec<ServerId>,
       pub started_tick: Option<u64>,
   }
   ```

2. In `drain.rs:14-18`, the destructure becomes:
   ```
   let (targets, started_tick) = match &world.resource::<DrainState>().0 {
       Some(info) if !info.targets.is_empty() => {
           (info.targets.clone(), info.started_tick)
       }
       _ => return,
   };
   ```
   `started_tick` is now `Option<u64>`.

3. In `drain.rs:24-29`, the "record drain start tick" block becomes:
   ```
   if started_tick.is_none() {
       if let Some(info) = &mut world.resource_mut::<DrainState>().0 {
           info.started_tick = Some(tick);
       }
   }
   ```

4. In `drain.rs:108-120`, the timeout check becomes:
   ```
   if let Some(info) = &world.resource::<DrainState>().0 {
       if let Some(started) = info.started_tick {
           if tick.saturating_sub(started) > DRAIN_TIMEOUT_TICKS {
               tracing::error!(
                   remaining,
                   pending_transfers,
                   "Drain timeout after {} ticks, forcing shutdown",
                   tick.saturating_sub(started),
               );
               world.resource_mut::<ShutdownRequested>().0 = true;
           }
       }
   }
   ```

5. Every existing test site in `drain.rs` tests module (e.g. line 175,
   224, 261, 280, 312, 342, 368, 421, 459, 480, 507, 531, 556) constructs
   `DrainInfo { ..., started_tick: 0 }`. Change to `started_tick: None`.
   Test behavior is preserved — all these tests assert behavior, not the
   raw value.

6. `drain_records_start_tick` at `drain.rs:547-573` asserts
   `started_tick == 42` after processing. Change to
   `assert_eq!(..., Some(42))`.

**Tests:**
- **Add** `drain_starts_at_tick_zero_stamps_correctly`: construct a
  drain with `started_tick: None`, set `TickCounter(0)`, run
  `process_drain`. Assert `info.started_tick == Some(0)` (NOT treated as
  "not yet stamped"). This is the regression that the old sentinel
  design couldn't express.
- **Add** `drain_timeout_does_not_fire_before_stamp`: run with
  `started_tick: None` and `TickCounter(700)` (past the
  `DRAIN_TIMEOUT_TICKS` ceiling of 600). Assert `ShutdownRequested` is
  false after the first process_drain — because this tick is the stamp
  tick, timeout check uses the freshly-stamped start, `tick.saturating_sub(700) = 0`.

**Done when:**
- `cargo test -p sim-server systems::drain::tests` passes.
- `cargo build -p sim-server` succeeds (no stray `started_tick: 0`
  sites anywhere in the workspace).

---

### WI-191: Plumb `drain_complete` Notify between game loop and main

**Size:** small
**Depends on:** WI-190 (soft; they touch different files but logically
share the drain flow)
**Files:**
- `crates/sim-server/src/game_loop.rs` (`run_game_loop_ecs` signature)
- `crates/sim-server/src/main.rs` (spawn site + select arm)

**Description:**

Today, `run_game_loop_ecs` at `game_loop.rs:580-591` breaks out of its
tick loop when `ShutdownRequested` is set, returning from the task.
`sim_handle` in `main.rs` then completes — but the ctrl_c branch of the
`tokio::select!` at `main.rs:210-224` runs in parallel and ignores it.

Add a `tokio::sync::Notify` that the game loop pokes when it's about to
break, so the ctrl_c branch in main can await it.

1. In `main.rs`, near the other `Arc` declarations (around line 110-130):
   ```
   let drain_complete = Arc::new(tokio::sync::Notify::new());
   ```

2. Pass `Arc::clone(&drain_complete)` to `run_game_loop_ecs`. Add the
   parameter at the end of the argument list:
   ```
   pub async fn run_game_loop_ecs(
       ...existing args...,
       peer_health: Arc<std::sync::Mutex<PeerHealthAccumulator>>,
       drain_complete: Arc<tokio::sync::Notify>,
   ) { ... }
   ```

3. At `game_loop.rs:588-591`, change:
   ```
   if world.resource::<ShutdownRequested>().0 {
       tracing::info!("Drain complete -- shutting down");
       break;
   }
   ```
   to:
   ```
   if world.resource::<ShutdownRequested>().0 {
       tracing::info!("Drain complete -- shutting down");
       drain_complete.notify_waiters();
       break;
   }
   ```
   Using `notify_waiters` (not `notify_one`) so that if no one is waiting
   yet, the next `.notified()` call still returns promptly — wait, this
   is backwards. `notify_waiters` wakes CURRENT waiters and stores nothing
   for future ones. `notify_one` stores a permit. For this handshake we
   want `notify_waiters` because the ctrl_c branch is already `await`ing
   by the time the game loop breaks — and if it isn't, there's no
   waiting to wake (main is exiting another way anyway). **Actually
   use `notify_one`** — safer: if the notify fires before the ctrl_c
   branch gets to `.notified().await`, the permit is stored and the
   subsequent await returns instantly. This is the "signal that drain
   has completed exactly once" semantics we want.

4. Update the existing tests in `game_loop.rs` that call
   `run_game_loop_ecs` directly (search for `run_game_loop_ecs(`) — they
   need an `Arc::new(Notify::new())` passed as the new argument. Look
   for the test helper(s) that construct the args.

5. If no test calls `run_game_loop_ecs` directly (only
   run-the-whole-thing tests), skip step 4.

**Tests:**
- **Add** `drain_complete_notify_fires_on_shutdown` in a suitable place
  (likely `game_loop.rs` tests): construct a game loop with immediately
  drained state, ensure the Notify fires. If this is hard to test in
  isolation because of all the resources needed, skip the direct test —
  the integration test in WI-193 covers the path.

**Done when:**
- `cargo build -p sim-server` succeeds.
- `cargo test -p sim-server` passes.

---

### WI-192: Ctrl-C sends `Draining` and waits on `drain_complete`

**Size:** medium
**Depends on:** WI-191
**Files:**
- `crates/sim-server/src/main.rs`

**Description:**

Rewrite the ctrl_c branch of the `tokio::select!` at `main.rs:210-224`.
Today it pushes an empty drain directive to `shared_coord_events`, sleeps
3 s, and returns — triggering none of the graceful drain machinery. Fix:

1. On ctrl_c:
   - Push `CoordClientMsg::Draining { server_id: ServerId(server_id) }`
     to `coord_outbound`. The coordinator receives it, calls
     `registry.drain_server()`, and broadcasts `DrainDirective` back —
     which the existing `coordinator_client` and
     `systems::coordinator::process_coord_events` pipeline delivers to
     the game loop. **Remove the local `shared_coord_events` push** —
     it's the wrong source of truth.
   - Log `"Received shutdown signal — sent Draining to coordinator,
     waiting for drain to complete (max 35s)"`.
   - Await `drain_complete.notified()` with a 35-second timeout
     (matches `DRAIN_TIMEOUT_TICKS = 600 ticks = 30s` with 5s slack for
     coordinator round-trip + transfer ACK):
     ```
     match tokio::time::timeout(
         std::time::Duration::from_secs(35),
         drain_complete.notified(),
     ).await {
         Ok(()) => tracing::info!("Graceful drain complete"),
         Err(_) => tracing::warn!("Drain timeout — forcing exit"),
     }
     ```

2. The select's outer shape stays the same. The ctrl_c branch simply
   becomes a real drain instead of a sleep.

3. **Second-ctrl-c force-exit:** spawn a second task that handles
   subsequent SIGINTs. The first SIGINT is consumed by the main select
   arm; subsequent ones mean the operator is impatient. Second ctrl_c →
   `std::process::exit(130)`:
   ```
   tokio::spawn(async {
       // Absorb the first SIGINT (it's consumed by the select arm above)
       tokio::signal::ctrl_c().await.ok();
       // Subsequent SIGINTs force exit
       loop {
           tokio::signal::ctrl_c().await.ok();
           tracing::warn!("Received additional SIGINT — forcing exit");
           std::process::exit(130);
       }
   });
   ```
   This must be spawned BEFORE the main select (so both handlers race
   fairly). Note: `tokio::signal::ctrl_c()` returns a future that
   resolves on SIGINT; spawning two concurrent waiters is safe because
   tokio's signal handler broadcasts to all.

**Tests:**

Hard to unit-test end-to-end. Acceptance is via WI-193 board verification.

**Done when:**
- `cargo build -p sim-server` succeeds.
- Manual local test: `./start.sh`, SIGINT one sim-server, observe the
  coordinator log a `Draining` message, drain directive broadcast, and
  the sim-server exits cleanly within ~5 s (no authoritative entities =
  instant drain).

---

### WI-193: Verify on UNO Q boards — graceful drain end-to-end

**Size:** small
**Depends on:** WI-190 + WI-191 + WI-192
**Files:** none (verification only)

**Description:**

The boards have sim-servers managed by systemd (`mesh-sim-server.service`).
systemd's default `TimeoutStopSec` + `KillSignal` determine whether
SIGTERM or SIGINT reaches the process and how long it has. The service
file at `/etc/systemd/system/mesh-sim-server.service` on each board
should send SIGTERM by default; tokio's `tokio::signal::ctrl_c()` only
catches SIGINT. **Check the service file**: if it sends SIGTERM, either
(a) change `KillSignal=SIGINT` in the service file, or (b) also wire a
`tokio::signal::unix::signal(SignalKind::terminate())` handler in
`main.rs`. For this WI, pick (b) — catch both SIGINT and SIGTERM via
`tokio::select!` on both futures.

Specifically: change the ctrl_c branch to:
```
_ = async {
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate()
    ).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
} => { /* drain logic */ }
```

(Windows doesn't matter here — deployment is linux-aarch64.)

**Verification scenarios:**

1. **SIGTERM via systemd stop (graceful path):** Deploy with
   `./start-unoq.sh`. Open client at
   `https://192.168.0.208:7871`, verify 3 servers in `/servers`. SSH to
   unoq: `sudo systemctl stop mesh-sim-server.service`. Capture:
   - `journalctl -u mesh-sim-server -n 50` on unoq — should show
     `Received shutdown signal — sent Draining to coordinator` then
     `Graceful drain complete` within 5 s.
   - `journalctl -u mesh-coordinator -n 50` on zzttpi — should show
     `Drain directive` log for the drained server.
   - Peer logs on unoq2 / unoq3 — should show `AuthorityTransfer`
     messages received, entities being promoted to authoritative.
   - Client `/servers` — S1's entities redistribute to S2 and S3
     within 5 s. Total entity count constant (proves no drops on
     floor).

2. **Drain under load:** Start client, click to spawn 100 NPCs on S2
   (direct it to S2 by selecting it from the coordinator's
   load-balanced assignment). SSH to unoq2, `sudo systemctl stop
   mesh-sim-server.service`. Verify 100 entities migrate to S1 and S3.
   No client disconnect from S2's perspective happens before the
   migration — client continues seeing entities render from S1 / S3.

3. **Drain timeout path:** Simulate a peer that won't ACK. Hardest to
   set up in production; for WI-193 it's acceptable to leave this
   scenario as "unit tested via `drain_timeout_forces_shutdown_with_pending`
   in drain.rs." Skip on the boards unless easily reproducible.

4. **Restart after drain:** After step 1, `sudo systemctl start
   mesh-sim-server.service` on unoq. Verify (with Batch 1 in place) the
   coordinator re-registers S1 instantly, S1 rejoins the mesh, client
   reconnects if it was on S1.

**Done when:**
- Scenarios 1 and 2 pass with evidence in `worklogs/mesh-dev/`. Scenario
  3 noted as deferred to unit test. Scenario 4 passes.

---

## Open questions

None remaining — architecture decision (Notify vs AtomicBool vs watch)
is made inline above (use `tokio::sync::Notify`).

## Out of scope

- Changing `DRAIN_TIMEOUT_TICKS` itself (currently 600 ticks / 30 s).
  That's a tuning parameter, not a correctness fix.
- Cleaning up the `shared_coord_events` local-push pattern more broadly.
  Only the ctrl_c drain path is touched; other users of that queue
  (coordinator_client → game_loop) stay as-is.
- Drain of inter-sim-server UDP traffic mid-flight. Covered by the
  existing AuthorityTransfer ACK retry logic.
- The second SIGINT force-exit could be a scope creep; keep it small —
  just `std::process::exit(130)` is enough, no retry bookkeeping.
