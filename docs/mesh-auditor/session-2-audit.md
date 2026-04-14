# Session 2 Changes Audit

**Date:** 2026-04-13
**Auditor:** mesh-auditor (Opus 4.6)
**Scope:** All production code changes on `dev` since `main` (commit `3049ef6`)
**Method:** Line-by-line review of every changed file against correctness, necessity, and simplicity criteria.

---

## Executive Summary

Session 2 implemented fixes for 10 audit findings across delta compression, tick loop health, and coordinator reconnection. The changes are largely correct and well-targeted. However, there are three issues that need attention:

1. **A dead-code bug** in the inbound swap (game_loop.rs:181-193) that acquires and immediately drops an ECS resource for no reason.
2. **The coordinator reconnection plumbing is over-engineered** for the problem it solves. The simpler subset (re-registration on dead connection + sender matching) handles the real failure mode; the rest adds complexity without clear necessity.
3. **The `repl_dur` timing is meaningless** -- it measures zero duration by construction, yet gets reported in metrics.

The test suite improvements are uniformly good and should be kept as-is.

---

## Verdicts by Category

### 1. Delta Compression Fixes

#### Vec3 distance threshold (`delta.rs:147-149`)
**Verdict: KEEP**

Correct fix. `distance_squared()` is the right comparison for movement that can be diagonal. The old per-component `abs()` would miss entities moving 0.008 on both X and Z simultaneously (magnitude 0.011, above threshold). The implementation is clean: single function, single constant, no allocation. The 37 unit tests covering this are thorough, including boundary cases, diagonal movements, and agreement between `change_ratio` and `compute_delta`.

One minor observation: the threshold is slightly more permissive on single axes than the old code (0.009 on X alone was caught by per-component but not by distance_squared, since 0.009^2 = 0.000081 < 0.0001). Finding #045 already documents this. It is the correct behavior for a uniform distance threshold.

#### Delta fallback at >80% change ratio (`systems.rs:1245-1259`)
**Verdict: KEEP**

Sound heuristic. When most entities changed, encoding a delta frame is wasteful -- the full snapshot is smaller (27-byte vs 31-byte header) and can be shared via Arc. The per-client fallback path (lines 1248-1259) correctly re-checks after computing the delta, so a client with a very stale baseline still gets a full snapshot even if other clients have recent baselines. The `change_ratio` pre-loop probe (lines 1190-1195) is a good optimization that avoids O(E*C) wasted work when the answer is obvious.

The only redundancy is that the fallback check appears in TWO places: once as the pre-loop probe (`force_full`, line 1190), and again as the per-client check after `compute_delta` (line 1258). The pre-loop probe makes the per-client check unreachable when `force_full` is true, but both are needed for the case where `force_full` is false (baselines differ across clients). This is correct.

#### Stale-ACK detection (`systems.rs:1204-1210, resources.rs:159-162`)
**Verdict: KEEP**

Well-designed. The 40-tick (2-second) timeout is faster than the 5-second periodic full snapshot (`FULL_SNAPSHOT_INTERVAL = 100`), providing quicker recovery for backgrounded tabs. The `last_sent_tick` field cleanly separates "what we sent" from "what they acknowledged." The server-tick clamp at line 1207 (`.min(tick.0)`) prevents malicious clients from sending future-tick ACKs to bypass stale detection.

#### ACK monotonicity check (`network.rs:182-186`)
**Verdict: KEEP**

Simple and correct. `tick > 0 && tick > prev` with `Ordering::Relaxed` is fine here -- the atomic is only accessed by one writer (the handle_client task) and one reader (the game loop via prepare_client_snapshot). There is no ABA problem because the value is monotonically increasing.

Finding #048 notes that legitimate reordered ACKs are rejected by this check. This is the correct behavior -- a reordered ACK carries stale information and should be discarded. The client will send a newer ACK on the next frame.

#### `last_sent_tick` tracking (`resources.rs:214`)
**Verdict: KEEP**

Necessary for stale-ACK detection. Updated only on successful `try_send`, which is the right place -- it tracks what the client actually received (or had the opportunity to receive).

---

### 2. Tick Loop Health

#### tokio::Mutex to std::sync::Mutex (`game_loop.rs:104,109`, `main.rs:99,101,109`)
**Verdict: KEEP**

This is the most impactful change in the session. `tokio::Mutex::lock().await` in the tick loop yielded to the scheduler on every lock acquisition, adding non-deterministic latency. `std::sync::Mutex::lock()` blocks the thread but is held for sub-microsecond durations (pointer swaps via `mem::take`), so the blocking time is negligible.

The poison-recovery pattern (`unwrap_or_else(|e| e.into_inner())`) is used consistently at all 15 lock sites, which is correct -- a panic in one task should not permanently wedge the game loop.

#### `process::exit(0)` to `break` (`game_loop.rs:388-391`)
**Verdict: KEEP**

Correct and necessary. `process::exit(0)` bypassed all destructors and the tokio runtime shutdown. The `break` lets the game loop return, the task completes, `tokio::select!` in main.rs fires, and the runtime shuts down cleanly. The `ShutdownRequested` resource and drain timeout (600 ticks / 30s) provide the control flow.

#### Dedicated UDP sender task (`game_loop.rs:63-100, main.rs:152-163`)
**Verdict: KEEP**

Good architecture. Moves all `udp_socket.send_to().await` calls off the game loop's tick thread. The dual-channel design (replication 256, control 64) with biased select is sound:

- Replication drops are harmless (shadow staleness recovers next tick)
- Control drops (transfers, forwarded inputs) cause visible glitches, so they get priority
- `try_send` in the game loop means zero awaits in the hot path

The channel sizes are reasonable: 256 replication slots = 12.8 ticks at 20Hz with 1 peer, which handles brief sender-task stalls. 64 control slots is generous for transfers (typically < 10 per rebalance event).

The `try_enqueue` helper function is defined inside `run_game_loop_ecs` (line 263), making it untestable from outside. The tests work around this by re-implementing it (line 1056). This is slightly inelegant but functionally harmless.

#### Yield points (`game_loop.rs:233, 361`)
**Verdict: KEEP, but with a caveat**

The yield after the ECS schedule run (line 233) is correctly placed -- it is after the longest synchronous phase and before outbound data extraction. The yield after UDP enqueue (line 361) gives the sender task a chance to drain.

**Caveat:** The yield points are only necessary BECAUSE `std::sync::Mutex` does not yield to the tokio scheduler. If the mutex sections grow in the future, the yield points become insufficient. This is an inherent tradeoff of the `std::sync::Mutex` choice and is acceptable at current scale. The comments at lines 228-232 document the rationale well.

#### FIX: Dead code in inbound swap (`game_loop.rs:181,193`)
**Verdict: FIX**

Lines 181-193 contain dead code from a refactor:

```rust
let mut iq = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
let ecs_queue = world.resource_mut::<InboundQueue>();  // <-- line 181: UNUSED
// ... mem::take from iq ...
drop(ecs_queue);  // <-- line 193: drops the unused borrow

let mut ecs_queue = world.resource_mut::<InboundQueue>();  // re-acquires
```

Line 181 acquires a mutable reference to the ECS `InboundQueue` resource, but it is never used. It is immediately shadowed by the `mem::take` calls that operate on `iq` (the shared mutex guard). Line 193 explicitly drops this unused borrow before re-acquiring at line 195.

This is not a correctness bug -- the unused `resource_mut` just briefly borrows the ECS resource for no purpose. But it is confusing and suggests the author intended to do a direct swap that was then changed to a two-phase take-then-assign. The two dead lines should be removed.

#### FIX: `repl_dur` measures zero by construction (`game_loop.rs:365-366`)
**Verdict: FIX**

```rust
let t3 = Instant::now();
let repl_dur = t3.elapsed();
```

This creates a timestamp and immediately measures elapsed time since that timestamp, which is always ~0 nanoseconds. The variable is then reported as the "replication" phase timing in metrics (line 381). This phase used to measure the WebSocket broadcast, which was moved into the ECS schedule. The timing capture should either be removed entirely or reassigned to measure something meaningful (e.g., the UDP enqueue phase, which is currently folded into `transfer_dur`).

---

### 3. Coordinator Reconnection

**Verdict: SIMPLIFY**

This is the most over-engineered change category. Let me assess each piece:

#### Re-registration on dead connection (`registry.rs:40-48`)
**Verdict: KEEP**

This is the core fix and is necessary. When a sim-server's TCP connection dies without a clean FIN (e.g., WiFi dropout on the UNO Q boards), the coordinator still holds the old entry. Without this check, the re-registering server is rejected as a duplicate. The `reply_tx.is_closed()` check is the right signal -- when the receiver side of the mpsc channel is gone, the connection is definitively dead.

#### `unregister_if_sender_matches` (`registry.rs:79-97`)
**Verdict: KEEP**

Necessary companion to re-registration. Without this, the race condition is:
1. S1 connects (connection A), registers
2. Connection A dies silently (no FIN)
3. S1 reconnects (connection B), re-registers (replaces A)
4. Connection A's read loop finally detects EOF, sends `Disconnected` event
5. **Without sender matching:** The stale `Disconnected` from A removes the LIVE connection B

The `same_channel()` check prevents step 5. This is correct and well-tested (test at line 849).

#### `Deregistered` message (`protocol.rs:166, registry.rs:67-73, coordinator_client.rs:142-148`)
**Verdict: SIMPLIFY -- could be removed**

The `Deregistered` message tells the sim-server "the coordinator just dropped you" so it can immediately reconnect rather than waiting for a read timeout. In practice, when the coordinator calls `unregister()`, it also drops the `ServerEntry`, which drops `reply_tx`, which closes the mpsc channel, which causes the sink task to exit, which drops the WebSocket write half. The sim-server's read loop will see the stream end and reconnect anyway.

The `Deregistered` message saves at most 1-2 seconds (the time between the coordinator dropping the entry and the sim-server detecting the closed connection). This is a minor improvement for a small amount of protocol surface area. It is not harmful, but it is not necessary. The sim-server's read timeout (30s) and the WebSocket close provide the same functionality with more delay.

**Recommendation:** Keep for now, but document that it is an optimization, not a correctness requirement.

#### 30-second read timeout (`coordinator_client.rs:72-92`)
**Verdict: KEEP**

Necessary defense against half-open TCP connections. Without this, a sim-server whose coordinator connection dies (no FIN, no RST -- e.g., network partition) will wait forever in `ws_stream.next().await`. The 30-second timeout is generous enough to avoid false positives (the coordinator sends PeerList on topology changes, which are infrequent but always accompanied by heartbeat processing).

#### `Activity` event (`main.rs:67-69, 163-164`)
**Verdict: SIMPLIFY -- marginal value**

The `Activity` event refreshes `last_heartbeat` on every WebSocket message from a sim-server, not just explicit `Heartbeat` messages. The rationale is that under heavy load, the heartbeat task (1Hz interval) might be starved by the game loop, but the `Register` message or the WebSocket heartbeats themselves prove the connection is alive.

In practice, this adds an event for every message the coordinator receives from every server. At 1 Heartbeat/sec per server with 3 servers, that is 3 Activity events/sec -- negligible. But it is conceptually muddy: heartbeat timeout detection should be based on application-level heartbeats, not transport-level activity. A TCP connection can be alive while the game loop is completely frozen.

**Recommendation:** Keep but recognize this is a workaround for the real problem (heartbeat task starvation). The yield points in the game loop (#013 fix) address the root cause. If the heartbeat task still gets starved after the yield points, the issue is that 1-second interval + std::sync::Mutex contention is inherently unreliable at high entity counts, and the fix is to send heartbeats from the game loop itself (where we know the server is alive because the tick just ran).

#### Heartbeat timeout 5s to 30s (`registry.rs:128-129`)
**Verdict: KEEP, but 30s is very long**

Increasing from 5s to 30s prevents false dead-server detection under heavy load. However, 30 seconds is a long time to wait before detecting an actually crashed server. During those 30 seconds, shadow entities on other servers are getting staler, and clients connected to the dead server are stuck.

The previous 5s timeout was too aggressive. A middle ground (10-15s) would be more appropriate: enough headroom for brief heartbeat delays, short enough to detect real crashes quickly. The `Activity` event and yield points already reduce the probability of false timeouts, so 30s is over-compensating.

**Recommendation:** Reduce to 15s. With the yield points and Activity events in place, heartbeat starvation lasting more than 15 seconds indicates a genuinely unresponsive server.

#### `touch()` method (`registry.rs:116-119`)
**Verdict: KEEP**

Simple helper that refreshes `last_heartbeat` without overwriting `entity_count`. Necessary for the `Activity` event pattern. If `Activity` is removed, `touch()` can also be removed.

---

### 4. Test Infrastructure

#### `wsOptions` hoisting fix, cleanup all entity types, cleanup verification + cooldown
**Verdict: KEEP**

All correct fixes for real test reliability issues. Cleaning up all entity types (not just NPCs) prevents player entity leaks between scenarios. The batch-with-pause approach for large cleanups and the metrics-based verification loop are robust.

#### Spawn distribution via coordinator `/servers`
**Verdict: KEEP**

The round-robin distribution across all servers via the coordinator is the correct approach for tests like `spawn-burst` that need entities distributed across the mesh.

#### `reassignServer()` for dead server recovery
**Verdict: KEEP**

Necessary for test resilience. After `crash-recovery` kills a server, the runner's original assignment may be dead. Getting a fresh assignment from the coordinator is the correct recovery.

#### Input-latency player detection (diff entity list)
**Verdict: KEEP**

The old approach (`players[players.length - 1]`) was fragile -- it assumed the most recently spawned player was the last in the snapshot, which is not guaranteed with multiple servers. The diff approach (record pre-spawn players, find new ones post-spawn) is correct. The fallback to auto-assigned player is a good safety net.

#### `STEADY_SCALE_MAX` env var
**Verdict: KEEP**

Allows capping the entity ceiling for resource-constrained environments (UNO Q boards). Simple and useful.

---

### 5. Test Quality Assessment

The tests added in this session fall into three categories:

**Category A: Tests that prove the fix works (KEEP)**
- `shared_inbound_lockable_from_non_async_context` -- proves std::sync::Mutex is used (would not compile with tokio::Mutex)
- `shared_inbound_lock_poisoned_recovery` -- proves the poison-recovery pattern works
- `shutdown_requested_causes_game_loop_to_return` -- proves break-not-exit (would kill the test runner with process::exit)
- `udp_sender_task_processes_send_via_control` -- verifies datagrams actually arrive through the sender task
- `ack_monotonicity_rejects_lower` -- verifies the monotonicity invariant

**Category B: Tests that verify constants or trivial properties (COULD REMOVE)**
- `udp_replication_channel_size_is_256` -- tests that a constant equals itself
- `udp_control_channel_size_is_64` -- same
- `full_snapshot_interval_constant_value` -- same
- `change_threshold_sq_value` -- same
- `compact_entity_state_from_entity_state` -- tests a trivially correct `From` impl
- `udp_outbound_send_variant` / `udp_outbound_batch_variant` -- tests that enum constructors work

These "constant-value" tests add noise without catching real bugs. If `CHANGE_THRESHOLD_SQ` changes, the developer will update the constant -- the test adds nothing. That said, they are harmless and running them costs nothing.

**Category C: Thorough delta compression tests (KEEP)**
The delta.rs test suite (37 tests) is genuinely well-done. It covers:
- Empty/full/partial deltas
- Boundary conditions at the threshold
- Incremental baseline updates across multiple rounds
- Diagonal movement in 2 and 3 axes
- Agreement between `change_ratio` and `compute_delta`

These tests are testing real algorithmic behavior, not boilerplate.

---

### 6. Cross-Change Conflicts

#### Yield points vs. mutex swap -- compatible?
**Yes, they are compatible and complementary.** The mutex swap removes the yielding that tokio::Mutex caused at lock acquisition. The explicit yield points add back controlled yielding at specific points where it is safe. The net result is: the game loop blocks during the actual lock (sub-microsecond), then explicitly yields at phase boundaries where no locks are held. This is strictly better than the old pattern where yielding happened unpredictably during lock acquisition.

#### Sender task + yield points -- any interaction?
The second yield point (line 361) yields after enqueuing UDP datagrams via `try_send`. This gives the sender task a chance to run (on the same tokio thread). If the sender task is CPU-bound (unlikely -- it just calls `send_to`), this yield is helpful. If the sender task is I/O-bound (normal), the yield is harmless. No conflict.

#### 30s heartbeat timeout + Activity events -- redundant?
Partially. With the `Activity` events refreshing `last_heartbeat` on every WebSocket message, the 30s timeout becomes even less likely to fire for a live server. The two mechanisms stack: Activity events prevent false positives at the coordinator, while the sim-server's 30s read timeout prevents zombie connections. They work together but are more machinery than strictly needed.

---

## Summary Table

| Change Category | Verdict | Notes |
|----------------|---------|-------|
| Vec3 distance threshold | **KEEP** | Correct fix, well tested |
| Delta fallback (>80%) | **KEEP** | Sound heuristic, correctly layered |
| Stale-ACK detection | **KEEP** | Well-designed, fast recovery |
| ACK monotonicity | **KEEP** | Simple, correct |
| `last_sent_tick` | **KEEP** | Necessary for stale-ACK |
| std::sync::Mutex swap | **KEEP** | Most impactful change |
| `break` instead of `exit` | **KEEP** | Correct lifecycle fix |
| Dedicated UDP sender | **KEEP** | Good architecture |
| Yield points | **KEEP** | Correctly placed |
| Inbound swap dead code | **FIX** | Remove lines 181, 193 |
| `repl_dur` timing | **FIX** | Measures zero, misleading metrics |
| Re-registration on dead conn | **KEEP** | Core fix, necessary |
| `unregister_if_sender_matches` | **KEEP** | Prevents race condition |
| `Deregistered` message | **SIMPLIFY** | Works without it (channel close signals) |
| 30s read timeout | **KEEP** | Necessary for half-open connections |
| `Activity` event | **SIMPLIFY** | Marginal value, root cause fixed by yield |
| Heartbeat timeout 30s | **KEEP** | But 15s would be more appropriate |
| `touch()` method | **KEEP** | Needed if Activity stays |
| Test runner improvements | **KEEP** | All correct, improve reliability |
| Input-latency detection | **KEEP** | Correct approach |
| Delta tests (37) | **KEEP** | Thorough, test real behavior |
| Constant-value tests | **KEEP** | Harmless but low value |

---

## Action Items

### Must Fix (Before Merge)

1. **Remove dead code in inbound swap** (`game_loop.rs:181,193`): Delete the unused `let ecs_queue = world.resource_mut::<InboundQueue>();` at line 181 and the `drop(ecs_queue);` at line 193. The `mem::take` calls between them do not use `ecs_queue`.

2. **Fix `repl_dur` timing** (`game_loop.rs:365-366`): Either remove the `repl_dur` variable entirely (replace with `Duration::ZERO` in the metrics recording, since client frames are now sent inside the ECS schedule), or repurpose it to measure the UDP enqueue phase (move `t2` to before enqueuing and `t3` to after, rename accordingly).

### Should Simplify (After Merge)

3. **Reduce heartbeat timeout** from 30s to 15s. With yield points and Activity events, 30s is unnecessarily generous and delays crash detection.

4. **Consider removing `Deregistered` message** and `Activity` event in a future cleanup pass. The WebSocket close and channel close already signal disconnection. These add protocol and code surface area for marginal improvement.
