# Work Items 022: Simplifications and Hardening

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 6).
Audit findings: `docs/mesh-auditor/074-churn-audit-summary.md`
(087 + 092 + 090 + 079).

The final churn-readiness batch. All pure cleanup and hardening — no
new user-visible behavior. Depends on Batches 1-5 being solid so
regressions are not masked by the refactors.

**Open-question resolutions:**
- **Q6 (delta compression — delete or feature-flag):** **outright
  delete**. The code has been effectively dormant (80% fallback fires
  every tick under the current always-moving NPC workload). Preserving
  it as a feature flag preserves option value at the cost of continued
  maintenance; deleting is cleaner. Revive later with the probe proving
  its value if/when a workload emerges (dormant inventory, stationary
  players).

Implement in order. WI-220 (delta removal) is the largest and touches
both server and client; do it first so the refactor in WI-221 works
against a simpler code shape.

---

### WI-220: Delete delta compression machinery (finding 092)

**Size:** large
**Depends on:** nothing
**Files (server):**
- `crates/sim-server/src/delta.rs` — delete entire file
- `crates/sim-server/src/lib.rs` or `main.rs` — remove `mod delta;`
- `crates/sim-server/src/systems/snapshot.rs` — simplify: always full
- `crates/sim-server/src/resources.rs` — strip delta-related fields
  from `ClientHandle`
- `crates/sim-server/src/network.rs` — remove FRAME_ACK handling path
- `crates/shared/src/protocol.rs` — remove delta-related binary frame
  types if separate
- `crates/sim-server/src/systems/inbound.rs` — remove frame-ack
  processing if present

**Files (client):**
- `client/src/binary-protocol.ts` — remove delta-frame decoding
- `client/src/network.ts` — remove `applyDelta` path, FRAME_ACK
  sending, delta diagnostics
- `client/src/world-view.ts` — remove `applyDelta` method
- `client/src/config.ts` — remove `FRAME_ACK` constant if present

**Files (tooling):**
- `tools/test-suite/runner.ts` — remove delta-frame assertions if
  present
- `tools/tests/binary-protocol.test.ts` — remove delta-specific tests

**Description:**

Today the server computes a delta against a per-client baseline, runs a
"is delta worth it" probe, and falls back to a full snapshot when the
delta would be >80% of the full size. With constant-velocity NPCs
moving every tick, the fallback fires every tick. The delta pipeline
runs for nothing.

Ripping it out:

1. **Server-side snapshot always full.** In `systems/snapshot.rs`:
   - Remove the `send_full = ...` decision logic.
   - Remove the delta-compute branch entirely (`compute_delta`,
     `update_baseline`, `DELTA_FALLBACK_THRESHOLD`, `FULL_SNAPSHOT_INTERVAL`,
     stale-ack logic specific to deltas).
   - Keep the per-client frame construction — but it always produces a
     full snapshot.
   - `ClientHandle.baseline`, `ticks_since_full`, `last_sent_tick`,
     and `acked_tick` all become dead. **Audit whether any other
     system reads them** — specifically `stale_ack` at `snapshot.rs:152`
     is about deltas; goes away.

2. **Delete `delta.rs` entirely** (~951 lines).

3. **Remove FRAME_ACK from client→server protocol.** In
   `client/src/network.ts:162-170`, the client currently sends a
   9-byte ACK frame after every snapshot. That goes away. The server
   side in `inbound.rs` or `network.rs` that parses FRAME_ACK goes
   away.

4. **Binary protocol: one frame type.** If the binary frame header at
   `client/src/binary-protocol.ts` has a `type: 'full' | 'delta'`
   discriminator, the type becomes always-full. Can drop the
   discriminator entirely (simpler) or leave it as always-0x01 and
   reject anything else (more defensive, less cleanup). Pick drop.

5. **Client `applyDelta` method goes away.** Replace all calls with
   `applySnapshot`. The `world-view.ts` delta-handling code (diffs,
   removals, add/update logic against a previous state) goes with it.
   Client can just blow away its entity map each frame and rebuild
   from the snapshot — which is what `applySnapshot` already does.

6. **Diagnostics cleanup.** `network.ts` has `deltaFrames`,
   `deltaFrameBytes`, delta_ratio logging, etc. Strip all of it.
   Keep `fullFrames`, `bytesReceived`, `frames`. The DIAG report
   becomes simpler.

7. **Dashboard** (`crates/coordinator/src/main.rs` or wherever the
   dashboard HTML lives) — if it displays delta ratio, rip out. Check
   first.

**Tests:**
- The snapshot.rs tests that assert delta behavior go away. Keep only
  the "full snapshot contains expected entities" tests.
- Add a regression test: `snapshot_is_always_full`: after many ticks,
  assert every emitted frame is a full snapshot.
- Client: no Vitest; smoke-test via the browser.

**Done when:**
- `cargo test --workspace` passes with no delta-specific tests.
- `cd client && npx tsc --noEmit` passes.
- `delta.rs` is gone from git.
- Local cluster still connects, spawns entities, renders them.

---

### WI-221: Single-pass `prepare_replication` classifier (finding 087)

**Size:** medium
**Depends on:** WI-220 (so the refactor isn't complicated by delta
baselines layered on top)
**Files:**
- `crates/sim-server/src/systems/replication.rs`

**Description:**

Today `prepare_replication` has three overlapping code paths for
phase-filtered entity sets (lines 100-120, 176-196, 203-236). The
phase filter `(e.id.0.as_u128() % 4) as u64 == current_phase` appears
three times. Each branch allocates a new `Vec<EntityState>`.

Rewrite as a single-pass classifier. For each entity, compute:
- `phase_match: bool = (entity.id % 4) == current_phase`
- For each peer: `is_hot_for_peer = near any player shadow on peer`
- For each peer: `is_cold_for_peer = phase_match AND in peer bounds AND NOT hot`
- For each peer: `is_bootstrap_for_peer = phase_match AND peer has no shadows`

Then insert into `outbound.per_peer[peer_id]` if any of the three
conditions is true for that peer.

Suggested shape:

```rust
outbound.per_peer.clear();
outbound.full_snapshot = None;
outbound.hot_count = 0;
outbound.cold_count = 0;

// Pre-compute per-entity phase bit.
for entity in &entities {
    let phase_match = (entity.id.0.as_u128() % 4) as u64 == current_phase;

    for &peer_id in peer_addrs.0.keys() {
        let mut include = false;
        let mut is_hot = false;

        // Hot: near any player shadow on peer
        if let Some(players) = peer_player_positions.get(&peer_id) {
            is_hot = players.iter().any(|pp| {
                let dx = entity.position.x - pp.x;
                let dz = entity.position.z - pp.z;
                (dx * dx + dz * dz) <= PEER_AOI_RADIUS_SQ
            });
            if is_hot { include = true; }
        }

        // Cold: phase-matched AND in peer bounds (and not already hot)
        if !include && phase_match {
            if let Some(bounds) = peer_bounds.get(&peer_id) {
                if bounds.contains(entity.position) {
                    include = true;
                }
            }
        }

        // Bootstrap: phase-matched AND peer has no shadows at all
        if !include && phase_match {
            let peer_has_shadows = peer_all_positions
                .get(&peer_id)
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            if !peer_has_shadows {
                include = true;
            }
        }

        if include {
            outbound.per_peer.entry(peer_id).or_default().push(entity.clone());
            if is_hot { outbound.hot_count += 1; } else { outbound.cold_count += 1; }
        }
    }
}
```

Behavior should match the old three-path logic exactly. Review the
original branches for edge cases (e.g., "no shadows anywhere in
the mesh" = all peers have no shadows = all get bootstrap; "peer
with no player shadows but has other entity shadows" = uses bounds
path, no hot entities).

**Tests:**
- Every existing `prepare_replication` test in `replication.rs` must
  still pass. If a test covered a specific edge case in the old
  branching, the new single-pass must preserve it.
- Add an equivalence test: construct a world state, run both the old
  (commented out or in a branch) and new logic, assert
  `outbound.per_peer` equivalent in content. Delete the old code
  after equivalence holds.

**Done when:**
- `cargo test -p sim-server systems::replication` passes.
- Function is measurably shorter (the WI spec said "~200 lines to
  ~60"; actual reduction may differ but should be significant).
- No regression in `peer-replication`, `multi-client`,
  `authority-transfer`, `rebalance-storm` scene tests.

---

### WI-222: Move `shadow_cleanup` before `prepare_client_snapshot` (finding 090)

**Size:** trivial
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/schedule.rs`

**Description:**

At `schedule.rs:28-29`:
```rust
systems::prepare_client_snapshot.after(systems::prepare_replication),
systems::shadow_cleanup.after(systems::prepare_client_snapshot),
```

Flip so `shadow_cleanup` runs BEFORE `prepare_client_snapshot`:
```rust
systems::shadow_cleanup.after(systems::prepare_replication),
systems::prepare_client_snapshot.after(systems::shadow_cleanup),
```

This ensures stale shadows don't appear in snapshots — they get
cleaned up BEFORE the snapshot is built. Small visible improvement:
entities "die" the same tick they go stale, not one tick later.

The existing test `stale_shadow_included_in_snapshot_then_cleaned` at
`schedule.rs:308-337` asserts the OLD ordering. Update the test to
assert the opposite:
- Stale shadow NOT in the snapshot on the tick it crosses threshold.
- Shadow is gone from the world after the tick runs.

Rename the test to `stale_shadow_cleaned_before_snapshot_built`.

**Tests:**
- Update `stale_shadow_included_in_snapshot_then_cleaned` →
  `stale_shadow_cleaned_before_snapshot_built`.

**Done when:**
- Test passes (with the inverted assertion).
- No scene-test regression.

---

### WI-223: `--coordinator-tls` flag (finding 079)

**Size:** small-medium
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/main.rs` — new CLI flag
- `crates/sim-server/src/coordinator_client.rs` — use `connect_async` with
  TLS connector when flag is true
- `crates/sim-server/Cargo.toml` — ensure `tokio-rustls` is available
  (probably already is for WSS client listener)
- `start-unoq.sh` — pass `--coordinator-tls` on the Pi deployment

**Description:**

Today the sim-server always connects to the coordinator with plain
`ws://` at `coordinator_client.rs:22-33`. On UNO Q deployment where the
coordinator might be behind an HTTPS LB or TLS-only load balancer,
this fails silently (no PeerList updates, etc).

Add a `--coordinator-tls` CLI flag. When set, use `wss://` and a
TLS-aware connector:

1. **CLI flag** in `main.rs`:
   ```rust
   /// Use WSS (TLS) when connecting to the coordinator control channel.
   /// Required when the coordinator is fronted by a TLS-only load balancer.
   #[arg(long, default_value_t = false)]
   coordinator_tls: bool,
   ```

2. **coordinator_client.rs** URL branch:
   ```rust
   let scheme = if coordinator_tls { "wss" } else { "ws" };
   let url = format!("{scheme}://{coordinator_addr}");
   ```

3. **TLS connector.** Use `tokio_tungstenite::connect_async_tls_with_config`
   with a `native_tls::TlsConnector::builder().danger_accept_invalid_certs(true).build()`
   **OR** `tokio_rustls::TlsConnector` with a webpki + optional
   custom trust anchor from `--tls-cert` (the sim-server already has
   `--tls-cert` for its listener; reuse it as the trust anchor for its
   outbound connection to the coordinator with the same self-signed
   cert).

   **Recommended:** use `rustls` (matches the rest of the workspace
   since the coordinator uses `tokio-rustls` for its WSS listener).
   Build a `RootCertStore` from the cert file specified by
   `--tls-cert`, then construct `ClientConfig` with that store. Pass
   to `connect_async_tls_with_config`.

4. **Update `start-unoq.sh`**: add `--coordinator-tls --tls-cert
   "$CERT_PATH"` to the sim-server command line on the boards. The
   coordinator is on zzttpi and serves both HTTP and HTTPS; with
   this flag, sim-servers connect via WSS. Verify the zzttpi
   coordinator actually binds WSS on port 7870 too — or does it bind
   WS only for sim-server control and HTTPS only for `/assign`?
   **Check this before starting** — if the coordinator's 7870 port is
   plain WS today, this WI adds no value on the boards unless we
   also update the coordinator to bind WSS on 7870. That's a
   coordinator-side change that should be in scope here.

   Expected coordinator change: at `coordinator/src/main.rs:110-135`,
   the `listener.accept()` → `tokio_tungstenite::accept_async` path
   today is plain. If `--tls-cert`/`--tls-key` are provided to the
   coordinator, wrap the TCP stream in `TlsAcceptor::accept` before
   `accept_async`. This is already done for the HTTP server at
   `run_assignment_server`; do the same for the WS server.

**Tests:**
- Unit test: `coordinator_client_uses_wss_when_tls_flag_set` (string-
  level check on the URL builder).
- Integration: run `./start-unoq.sh` with the flag and verify
  sim-servers connect (via coordinator logs "Sim-server connecting
  from ...").

**Done when:**
- Sim-server connects to coordinator over WSS on the boards.
- Tests pass.
- Mac local (`./start.sh`) unchanged — coordinator still binds plain
  WS by default, sim-servers don't pass `--coordinator-tls`.

---

### WI-224: Board verification

**Size:** small
**Depends on:** WI-220 + WI-221 + WI-222 + WI-223
**Files:** none (verification only)

**Description:**

Deploy via `./start-unoq.sh`. Run:

1. **Delta code truly gone:** `grep -r "applyDelta\|compute_delta\|deltaFrames" crates/ client/ tools/` returns no hits (except possibly in comments / archived docs). CI-level check.

2. **Regression suite:** all existing scene tests pass —
   `./test-scene.sh --no-start` against the running cluster.

3. **prepare_replication refactor regression:** run the
   `peer-replication`, `multi-client`, `rebalance-storm` scenarios
   from `./test.sh`. All green.

4. **Shadow cleanup ordering:** observe debug overlay — entities
   go away cleanly when their authoritative server departs, no
   one-tick "dead but rendered" flicker.

5. **Coordinator TLS on boards:** verify sim-servers' logs show they
   connected to the coordinator via WSS:
   ```
   tracing::info!("Connected to coordinator at wss://zzttpi.local:7870")
   ```

6. **Full churn regression:** redo a Batch 2 scenario — kill a
   sim-server, confirm the client migrates. Redo Batch 4 —
   overload a server, confirm a player migrates with identity
   preserved. Make sure none of the cleanup broke churn behavior.

**Done when:**
- All six pass. Evidence in `worklogs/mesh-dev/`.

---

## Out of scope

- Re-introducing delta compression behind a feature flag. If needed
  later, start from a fresh design.
- Compressing UDP replication payloads with bincode deltas. Separate
  future design.
- Adding a mesh-wide entity compression layer. Out of scope.
- Rewriting the coordinator to shard or horizontally scale. Out of
  scope per plan Section 4.

## Post-batch

After this batch lands, update `docs/mesh-architect/designs/016-churn-resilience-plan.md`
to mark all six batches as landed, and archive `074-churn-audit-summary.md`
to `docs/mesh-auditor/archive/`. The plan then becomes "done, here's
what shipped." New churn-related findings can start from 094+.
