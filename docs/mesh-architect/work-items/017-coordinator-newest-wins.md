# Work Items 017: Coordinator Newest-Wins Registration

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 1).
Audit finding: `docs/mesh-auditor/074-churn-audit-summary.md` (076).

Implement in order. Each WI is self-contained and testable. WI-170 is the
core correctness change; WI-171 is defense-in-depth; WI-172 is the board
verification.

---

### WI-170: `Registry::register` replaces the existing entry unconditionally

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/coordinator/src/registry.rs`

**Description:**

Current behavior (`registry.rs:54-92`): if an entry for `server_id` already
exists and its `reply_tx.is_closed()` returns `false`, `register()` logs a
warning and returns early without inserting. This makes sense for "same
process reconnecting through some bug", but fails under `kill -9` /
power-off because the mpsc channel stays open as long as the `sink_task`
has not yet noticed the dead TCP socket. Restarted sim-servers are ignored
for up to 15 s (heartbeat timeout) or longer.

New behavior: **a fresh `Register` call always replaces the existing
entry**. The mesh's source of truth for "who owns `server_id` right now" is
the most recent live WebSocket connection, matching the session-id
"newest wins" semantics already used for UDP peers.

Concretely:

1. Remove the `existing.reply_tx.is_closed()` branch entirely. The function
   should unconditionally replace any existing entry for the given
   `server_id`.
2. When replacing, log at `info` level: `"S{server_id} re-registering —
   replacing existing entry (new connection wins)"`. Include the
   `registered_at.elapsed()` of the old entry in the log message so
   operators can see how stale the replaced entry was.
3. The old entry's `reply_tx` is dropped when the `HashMap::insert` returns
   the old value. **Don't explicitly close the old sender** — dropping it
   is equivalent, and the old `sink_task` holds its own clone anyway so the
   mpsc channel won't fully close until TCP fails. That's fine: no code
   path sends messages to the old `reply_tx` once the new entry is in
   place.
4. Preserve the existing signature. No new parameters.

**Tests (in `registry.rs` tests module):**

- **Invert** the existing `duplicate_register_with_live_connection_rejected`
  test (at `registry.rs:1073-1085`). Rename to
  `duplicate_register_with_live_connection_replaces_entry`. Assertion
  changes from `tx1.same_channel(...)` to `tx2.same_channel(...)` — the
  new tx is stored.
- **Invert** `duplicate_register_ignored` (`registry.rs:891-897`). Rename to
  `duplicate_register_replaces_entry`. Still asserts `server_count == 1`
  (replacement, not addition), but now also asserts the `reply_tx` matches
  the second call's sender.
- **Keep** `re_register_replaces_dead_connection`
  (`registry.rs:1050-1070`) as-is — its assertion still holds under the
  new semantics (replaces dead connection → still replaces).
- **Add** `re_register_preserves_host_from_new_connection`: register with
  `host = Some("host-a.local")`, then register again with
  `host = Some("host-b.local")`. Assert the stored `host` is `"host-b"`
  (not `"host-a"`), since the new connection is authoritative.
- **Add** `re_register_resets_entity_count_to_zero`: register; heartbeat
  with `entity_count=100`; re-register same server_id. Assert
  `entity_count == 0` (the new process has a fresh entity set; it will
  re-announce via the next heartbeat).
- **Add** `re_register_clears_in_flight_transfers`: register S1 and S2;
  issue a rebalance directive from S1 to S2 (push an entry onto S1's
  `in_flight_transfers`); re-register S1. Assert S1's
  `in_flight_transfers` is empty. This prevents the replaced server from
  carrying stale transfer bookkeeping into its new life.

**Done when:**
- `cargo test -p coordinator registry::tests` passes.
- A manual `kill -9 sim-server` + restart lands within the same heartbeat
  window instead of waiting 15 s.
- The coordinator log line `"S{n} re-registering — replacing existing
  entry"` appears on restart.

---

### WI-171: TCP SO_KEEPALIVE on the coordinator accept path

**Size:** small
**Depends on:** WI-170 (not strictly; the two are independent, but we
sequence WI-171 second so WI-170's behavior change is observable without
kernel-level variables in play)
**Files:**
- `crates/coordinator/Cargo.toml`
- `crates/coordinator/src/main.rs`

**Description:**

Even with WI-170, there are secondary paths that depend on the kernel
noticing a dead connection: the `sink_task` in `handle_server_connection`
exits when `ws_sink.send(...)` fails, which only happens when the kernel
returns an error on a TCP write. Without TCP keepalive configured, that
can take minutes to hours.

Add TCP keepalive to every accepted sim-server connection:

1. Add `socket2 = "0.5"` to `crates/coordinator/Cargo.toml` under
   `[dependencies]`.
2. In `main.rs`, right after the `listener.accept()` at line 116 (and
   before `tokio_tungstenite::accept_async`), apply keepalive settings to
   the `TcpStream`:
   ```
   use socket2::{SockRef, TcpKeepalive};
   let ka = TcpKeepalive::new()
       .with_time(Duration::from_secs(10))
       .with_interval(Duration::from_secs(5))
       .with_retries(3);
   let sock = SockRef::from(&stream);
   if let Err(e) = sock.set_tcp_keepalive(&ka) {
       tracing::warn!("set_tcp_keepalive failed for {addr}: {e}");
   }
   ```
   Total time to death detection: `10s + 3 * 5s = 25s` worst case, well
   within the heartbeat timeout semantics already in place. Failure to set
   keepalive is a warning, not a fatal — we proceed anyway.
3. No test is strictly required here (this is OS-level plumbing and would
   need an integration test with a misbehaving peer). Add a module-level
   comment pointing at the design doc. If a test is trivial to write
   (e.g. assert the socket option is readable after setting), add it; if
   not, skip.

**Note on macOS:** `TcpKeepalive::with_interval` and `with_retries` are
not supported on all Darwin versions. If the socket2 docs indicate a
compile error on darwin, gate those two calls with `#[cfg(not(target_os
= "macos"))]` and keep `.with_time(...)` unconditional. The Pi Zero W2
(coordinator host) is aarch64 linux-gnu, so the full set works there.

**Done when:**
- `cargo build -p coordinator` succeeds on both Mac and the cross-compiled
  armv7 Pi target.
- On a Pi deployment, after killing an upstream UNO Q with power-off (not
  graceful kill), the coordinator's `sink_task` for that peer exits
  within ~30 s rather than hanging.

---

### WI-172: Verify on UNO Q boards

**Size:** small
**Depends on:** WI-170 + WI-171
**Files:** none (verification only)

**Description:**

1. Run `./start-unoq.sh` — cross-compiles, deploys, restarts all three
   sim-servers and the coordinator on zzttpi. Wait for the cluster to
   stabilize (watch `/servers` on https://192.168.0.208:7871 show all
   three sim-servers).
2. From the Mac, `ssh unoq` then `sudo systemctl kill -s SIGKILL
   mesh-sim-server.service`. Verify from `/servers` that S1 (or whichever
   was killed) disappears within ~15 s.
3. `ssh unoq` then `sudo systemctl start mesh-sim-server.service`. Verify
   S1 reappears in `/servers` within 2–3 seconds of reconnecting.
4. Check coordinator logs on zzttpi: `journalctl -u mesh-coordinator -n 50
   | grep re-registering` — should show the "replacing existing entry"
   log line from WI-170.
5. Repeat with all three sim-servers in sequence to cover every UDP-peer
   pair.
6. **Negative case:** `kill -9` on a sim-server and restart it *within
   2 seconds* (before heartbeat timeout fires). Verify: coordinator logs
   a single `re-registering` line, NOT a `RecoverEntities` broadcast
   (which would mean it thought the peer was dead). This is the
   regression case WI-170 is designed to prevent.

**Done when:**
- All six test cases pass on the boards.
- The worklog entry is updated in `worklogs/mesh-dev/` with screenshots or
  log snippets of the success/failure cases.

---

## Open questions (none for Batch 1)

The plan doc's open questions (Section 5) all apply to Batches 3–6. Batch
1 has no open questions; it's pure implementation.

## Out of scope

- Changing `unregister_if_sender_matches` — the existing matching logic is
  already correct under WI-170 and actually becomes more important (late
  Disconnected events from the replaced connection must not wipe the new
  entry).
- Changing the `broadcast_recovery` trigger path — that's Batch 5 (finding
  084).
- Any sim-server-side changes — this batch is 100% coordinator-local.
