# Work Items 013: Peer Restart Recovery

Design: `docs/mesh-architect/designs/013-peer-restart-recovery.md`.

Implement in order. Each WI is self-contained and testable. WI-131 (the wire
protocol change) is the blocking precursor — everything downstream depends on
the new `session_id` field existing in `PeerMessage`.

---

### WI-130: Add `SessionId` resource and boot-time generation
**Size:** small
**Depends on:** nothing
**Files:**
- `crates/sim-server/src/resources.rs`
- `crates/sim-server/src/main.rs` (or wherever the ECS world is created and
  resources are inserted — check `run_game_loop_ecs` in `game_loop.rs` for
  the insert site)

**Description:**
- Add `#[derive(Resource, Clone, Copy, Debug)] pub struct SessionId(pub u64);`
  to `resources.rs`.
- At sim-server startup, generate
  `let session_id = rand::random::<u64>();` and insert
  `SessionId(session_id)` as an ECS resource (next to `ServerConfig`).
- Log it on startup:
  `tracing::info!(server_id = server_id.0, session_id = format_args!("0x{:016x}", session_id.0), "sim-server session id generated");`.
- Do NOT yet thread it through to any PeerMessage producers; that's WI-132.

**Tests:**
- `resources.rs`: compile-only test that `SessionId` derives `Resource`,
  `Copy`, `Debug` (add to an existing resources tests module or create one).
- New unit test: two invocations of `rand::random::<u64>()` are overwhelmingly
  likely to differ — assert not equal (flaky at 1/2^64, acceptable).

**Done when:**
- `cargo build -p sim-server` succeeds.
- Starting a sim-server logs its session id line.
- `SessionId` resource is readable from any system via `Res<SessionId>`.

---

### WI-131: Add `session_id` field to every `PeerMessage` variant (wire protocol change)
**Size:** medium
**Depends on:** WI-130
**Files:**
- `crates/shared/src/protocol.rs`
- `crates/shared/src/lib.rs`

**Description:**
- Bump `PROTOCOL_VERSION` from `1` to `2` in `shared/src/lib.rs`.
- Add `session_id: u64` to each of the six `PeerMessage` variants:
  `EntityStateUpdate`, `AuthorityTransfer`, `ForwardedInput`,
  `ForwardedRemove`, `TransferAck`, `ChunkedEntityUpdate`. See the design doc
  for exact field ordering (`session_id` placed after `tick` where `tick`
  exists, otherwise as the first field after `server_id` or directly first).
- Update every existing `peer_message_*_bincode_roundtrip` test in
  `protocol.rs` to construct the variant with a `session_id` value and assert
  it round-trips.
- Update the `all_peer_message_variants_distinct_bincode` test to include
  a `session_id` in each constructed variant.
- Update every construction site in the workspace to pass a value for the
  new field. Producers (for this WI, just make them compile with a
  placeholder like `session_id: 0`; WI-132 replaces 0 with the real
  SessionId):
  - `crates/sim-server/src/systems/replication.rs`
  - `crates/sim-server/src/systems/transfers.rs`
  - `crates/sim-server/src/systems/input.rs`
  - `crates/sim-server/src/game_loop.rs` (`enqueue_entities_to_peer`
    builds `EntityStateUpdate` and `ChunkedEntityUpdate`)
  - Any test that constructs a `PeerMessage` literal.

**Tests:**
- Update every existing roundtrip test to round-trip the new field.
- Add: `peer_message_entity_state_update_session_id_roundtrip` with distinct
  session_id values to confirm ordering/bincode didn't shift.
- Add: `peer_message_wire_version_is_2`:
  `assert_eq!(shared::PROTOCOL_VERSION, 2);`.
- Add: `peer_message_wire_size_grew_by_8_bytes_per_variant` — for each
  variant pair (old hand-rolled u64-less version vs new), assert the bincode
  wire size grew by exactly 8. (This is sanity against accidentally adding
  something wider.)

**Done when:**
- `cargo build` succeeds workspace-wide.
- All of `cargo test -p shared` passes.
- Any `PeerMessage::...` literal without `session_id` is a compile error.

---

### WI-132: Thread `SessionId` into all outbound `PeerMessage` constructions
**Size:** medium
**Depends on:** WI-131
**Files:**
- `crates/sim-server/src/systems/replication.rs`
- `crates/sim-server/src/systems/transfers.rs`
- `crates/sim-server/src/systems/input.rs` (including `send_transfer_ack`)
- `crates/sim-server/src/game_loop.rs` (`enqueue_entities_to_peer` signature
  and callers)
- `crates/sim-server/src/resources.rs` — consider adding a `session_id: u64`
  field to `OutboundReplication` populated once per tick, so
  `enqueue_entities_to_peer` in `game_loop.rs` can read it without an ECS
  dep (the current call site is outside the ECS schedule run)

**Description:**
- Wherever a `PeerMessage::*` is constructed to send outbound, read
  `Res<SessionId>` (or the `OutboundReplication.session_id` cache) and use
  its value as `session_id` instead of the placeholder `0` left by WI-131.
- Recommended: have `systems/replication.rs` write the current
  `session_id` into `OutboundReplication.session_id` at the start of its
  system run, and `enqueue_entities_to_peer` reads from the extracted
  replication struct. Matches the existing pattern where `repl_tick` is
  snapshot-extracted post-schedule.

**Tests:**
- New unit test under `systems/replication.rs` (or new
  `systems/session_id_tests.rs`): build a minimal world with `SessionId(0xAB)`,
  run one replication tick, intercept the outbound PeerMessage, and assert
  `session_id == 0xAB` on every produced message.
- Similar focused tests for: authority transfer producer, forwarded input
  producer, transfer ack producer, forwarded remove producer.

**Done when:**
- Every PeerMessage going out over UDP carries the boot-generated session id
  (not 0, except for the legitimate edge case where `rand` happens to produce
  0).
- `cargo test -p sim-server` passes.

---

### WI-133: Receiver keys reassembly + latest_tick by `(server_id, session_id)`
**Size:** medium
**Depends on:** WI-131
**Files:**
- `crates/sim-server/src/receiver.rs`

**Description:**
- Change `reassembly: HashMap<(u16, u64), ChunkReassembly>` to
  `HashMap<(u16, u64, u64), ChunkReassembly>` (adding `session_id` between
  `server_id` and `tick`).
- Change `latest_tick: HashMap<u16, u64>` to
  `HashMap<(u16, u64), u64>` keyed by `(server_id, session_id)`.
- Add `session_of_peer: HashMap<u16, u64>` — the currently-known session for
  each peer.
- Update the global sweep (lines 44–74) to use the composite key. Eviction
  criterion stays `server_latest.saturating_sub(t) > GLOBAL_STALE_TICKS`, but
  `server_latest` is now looked up via `(server_id, session_id)`.
- Update the inline sweep in the `ChunkedEntityUpdate` arm (lines 223–236)
  to use the composite key.
- Update the `MAX_REASSEMBLY_ENTRIES` hard-cap eviction to sort by `tick`
  component of the composite key (still just `tick`; the composite
  preserves that).
- **Do not** yet add the session-change reset logic (that's WI-134); for
  this WI just make the keying change so WI-134 has something to hook into.
  At this point, if a server restarts, we now have a stale
  `(server_id, old_session)` entry forever in `latest_tick` — memory leak
  bounded by the number of sessions seen, fixed in WI-134.

**Tests:**
- `receiver_reassembly_key_includes_session`: simulate two packets with the
  same `(server, tick)` but different `session_id`; assert they are
  reassembled independently.
- `receiver_latest_tick_keyed_per_session`: feed `(s=2, sess=A, tick=100)`
  then `(s=2, sess=B, tick=50)`; assert both `latest_tick[(2, A)] = 100`
  and `latest_tick[(2, B)] = 50` are tracked.

**Done when:**
- `cargo test -p sim-server receiver` passes (existing tests + new keying
  tests).
- No behavioral regression at single-session load (local `./start.sh` +
  `./test.sh`).

---

### WI-134: Detect session change and clear stale receiver state
**Size:** medium
**Depends on:** WI-133
**Files:**
- `crates/sim-server/src/receiver.rs`
- `crates/sim-server/src/resources.rs` (add `PeerSessionReset` struct and
  `peer_session_resets: Vec<PeerSessionReset>` field on `InboundQueue`)

**Description:**
- Add `pub struct PeerSessionReset { pub server_id: ServerId, pub new_session: u64 }`
  to `resources.rs`.
- Add `pub peer_session_resets: Vec<PeerSessionReset>` to `InboundQueue`
  (and to the `#[cfg(debug_assertions)]` drain-check in `game_loop.rs` if
  present).
- In `receiver.rs`, extract a helper:

  ```rust
  fn note_session(
      server_id: u16,
      session_id: u64,
      session_of_peer: &mut HashMap<u16, u64>,
      reassembly: &mut HashMap<(u16, u64, u64), ChunkReassembly>,
      latest_tick: &mut HashMap<(u16, u64), u64>,
      peer_health: &Arc<SharedPeerHealth>,
      inbound: &Arc<SharedInbound>,
  )
  ```

  Behavior: if the stored session for this server differs from the incoming
  session, log at `info`, retain only this-session entries in `reassembly`,
  remove the old entry from `latest_tick`, clear
  `peer_health.{latest_tick, packets_received, chunk_evictions}` for this
  server, and push a `PeerSessionReset` to `InboundQueue`.
- Call `note_session` at the top of each `PeerMessage` arm that carries
  `(server_id, session_id)` — practically, every arm.
- Drop packets where `server_id` matches a known peer but `session_id`
  doesn't match the stored session AFTER we've updated `session_of_peer`?
  No — we always trust the latest-arriving session_id as the current
  session. An out-of-order straggler from the old session causes a second
  reset back to the old session's state clearing — that's acceptable and
  self-corrects on the next new-session packet (which arrives within ~50 ms
  typically). Document this trade-off in a code comment.

**Tests:**
- `receiver_session_change_clears_reassembly`: seed reassembly with
  `(s=2, sess=A, tick=100)` partial entry, inject
  `ChunkedEntityUpdate(s=2, sess=B, tick=5, chunk 0/2)`; assert the A entry
  is gone and only `(s=2, sess=B, tick=5)` remains.
- `receiver_session_change_clears_latest_tick`: seed
  `latest_tick[(2, A)] = 14000`, inject new session B packet; assert
  `(2, A)` removed and `(2, B)` inserted.
- `receiver_session_change_clears_peer_health`: seed
  `peer_health.latest_tick[2] = 14000`, inject new session; assert
  `peer_health.latest_tick.get(&2)` is either absent or reset.
- `receiver_session_change_pushes_inbound_reset`: inject new session, poll
  `inbound.peer_session_resets`, assert it contains a
  `PeerSessionReset { server_id: 2, new_session: B }`.
- `receiver_same_session_does_not_clear`: inject two packets with the same
  session; assert no `PeerSessionReset` is pushed.
- `receiver_first_session_from_unknown_peer_does_not_clear`: first packet
  from a peer we've never seen — should insert into `session_of_peer`
  but not produce a reset event (nothing to clear yet).

**Done when:**
- All new + existing `receiver` tests pass.
- Running two sim-servers locally, killing and restarting one, survivors
  log `Peer session changed; clearing stale state` within ~50 ms of the
  new process starting to replicate.

---

### WI-135: Reset shadow `last_seen_tick` on `PeerSessionReset`
**Size:** small
**Depends on:** WI-134
**Files:**
- `crates/sim-server/src/systems/inbound.rs`

**Description:**
- In `drain_inbound_events`, before the peer-update loop, drain
  `InboundQueue.peer_session_resets`.
- For each reset, walk all entities with component
  `Shadow` and `AuthorityServer(server_id) == reset.server_id`, and set
  their `Shadow.last_seen_tick = 0`.
- This unblocks the `if update.tick < shadow.last_seen_tick { continue; }`
  check at line 37 so the first post-restart update is accepted.
- Log at `info`: `"Reset N shadow last_seen_tick for server S{server_id}"`.

**Tests:**
- `shadow_last_seen_reset_on_session_change`: spawn two shadows for
  `AuthorityServer(2)` with `last_seen_tick = 500`, push a
  `PeerSessionReset { server_id: 2, .. }`, run `drain_inbound_events`,
  assert both shadows now have `last_seen_tick = 0`.
- `shadow_last_seen_reset_only_affects_matching_server`: spawn shadows for
  server 2 and server 3, reset server 2 only, assert server 3 shadow is
  untouched.
- `shadow_last_seen_reset_with_no_shadows_is_noop`: reset for server 2 with
  no server-2 shadows, no panic, no effect.

**Done when:**
- All new tests pass.
- Existing `inbound` tests still pass.

---

### WI-136: Add session_id to `/metrics` output
**Size:** small
**Depends on:** WI-130
**Files:**
- `crates/sim-server/src/metrics.rs` (or `metrics_server.rs` — whichever
  produces the JSON)
- `crates/sim-server/src/game_loop.rs` (if metrics collection needs to pick
  up `SessionId`)

**Description:**
- Expose the local server's own `session_id` as a top-level field in the
  `/metrics` JSON payload: `"session_id": "0x<hex>"`.
- Expose last-observed peer session IDs as
  `"peer_sessions": { "2": "0x<hex>", "3": "0x<hex>" }`.
  Source: `peer_health` or a dedicated `SharedPeerHealth.peer_sessions: HashMap<u16, u64>`
  populated by the receiver in WI-134 (add a line alongside `session_of_peer`).
- Dashboard (optional, if `crates/board-dash` or the coordinator dashboard
  renders metrics): display the session id for each server so an operator
  can visually confirm a restart.

**Tests:**
- `/metrics` JSON round-trip test asserts `session_id` field is a
  non-empty hex string.
- `/metrics` JSON includes `peer_sessions` for every peer currently in
  `peer_health.latest_tick`.

**Done when:**
- `curl localhost:8101/metrics | jq .session_id` returns a hex string.
- After a peer restart, `peer_sessions["<peer_id>"]` shows the new hex
  value on surviving peers (visible proof the fix works end-to-end).

---

### WI-137: Scene test — `peer-restart-reconvergence`
**Size:** medium
**Depends on:** WI-132, WI-134, WI-135
**Files:**
- `scene-tests/` (check existing structure — e.g.
  `scene-tests/crash_recovery.ts` for a template) or whatever `./test-scene.sh`
  drives
- `test.sh` (register the new scenario if scenarios are listed there)

**Description:**
- Start a 3-server cluster. Spawn 500 entities distributed across S1/S2/S3.
- Record S2's entity count and the shadow counts on S1 and S3.
- Gracefully kill S2's sim-server process (`kill -TERM`, not kill -9, to
  avoid confusing the test with coordinator crash-recovery side-effects
  unrelated to this bug).
- Restart S2 (same server_id). It will re-register and begin replicating.
- Spawn 500 new entities on S2 (since the old entities were lost on clean
  restart without persistence).
- Assert: within 40 ticks (2 s) of S2's first post-restart replication tick,
  S1's `shadow_count` and S3's `shadow_count` each reflect all 500 of S2's
  entities.
- Assert: `/metrics peer_health[2].latest_tick` on S1 and S3 is within 5
  ticks of S2's current tick.
- Assert: `/metrics peer_health[2].chunk_evictions` stops growing (sample
  at T+3s and T+5s, assert difference ≤ some small constant like 5).

**Tests:**
- The scenario itself.
- Run locally via `./test-scene.sh` and on UNO Q via `./start-unoq.sh` +
  `./test-scene.sh --no-start`.

**Done when:**
- Scenario passes locally (3 servers on Mac).
- Scenario passes on the UNO Q board cluster.
- Before the fix (revert WI-133..WI-135 temporarily), the scenario *fails*
  as expected — documents the regression guard.

---

### WI-138: Scene test — `peer-restart-twice`
**Size:** small
**Depends on:** WI-137
**Files:**
- Same location as WI-137

**Description:**
- Extend the previous scenario: kill+restart S2 twice within 5 s.
- Assert final convergence (same acceptance criteria as WI-137).
- Purpose: catch bugs where `session_of_peer` gets confused by back-to-back
  session changes.

**Tests:**
- Run local + UNO Q.

**Done when:**
- Scenario passes on both environments.

---

### WI-139: Document mixed-version rolling redeploy behavior
**Size:** small
**Depends on:** WI-131
**Files:**
- `CLAUDE.md` (deploy section, near `start-unoq.sh` notes)
- Optionally a short note in `docs/api-reference.md`

**Description:**
- Add a short note under the UNO Q deployment section: "Wire
  protocol version is bumped to v2 as of <date>. During a rolling redeploy,
  servers running different versions will log
  `UDP protocol version mismatch ... dropping packet` and drop cross-version
  replication packets. This produces a brief (~5 s) window of cross-version
  invisibility. `start-unoq.sh` deploys all boards in one cycle and minimizes
  this window."
- No code changes.

**Tests:**
- None (docs only). Skim the docs after writing to confirm clarity.

**Done when:**
- Commit includes the doc update.
- Reviewer confirms the explanation is clear.

---

### WI-140: Audit sweep of all `PeerMessage` call sites and update
**Size:** small
**Depends on:** WI-131
**Files:**
- Anywhere `PeerMessage::` is referenced.

**Description:**
- Grep the workspace for `PeerMessage::` and confirm every construction and
  every destructuring match handles the new `session_id` field.
- `cargo check` should already catch missing field adds, but destructure
  patterns may still compile with `..` — audit those to ensure the
  receiver consumes session_id where needed (the main receiver arms in
  WI-133/134 are the critical sites; this WI is the cleanup sweep).

**Tests:**
- `cargo clippy -- -D warnings` should still pass.
- New clippy warnings about unused `session_id` in a destructure indicate
  a site that should be consuming it but isn't — fix or justify.

**Done when:**
- Grep for `PeerMessage::\w+ {` and each site either consumes or
  explicitly ignores `session_id` with a `// NOTE:` comment.

---

## Suggested batching for mesh-dev

- **Batch 1 (protocol foundation):** WI-130, WI-131, WI-132. Produces a
  buildable tree where every outbound PeerMessage carries session_id, but
  the receiver still ignores it. No behavior change yet on the happy path;
  mesh still broken under restart.
- **Batch 2 (receiver fix):** WI-133, WI-134. Receiver keys by session and
  resets on change. Server-local behavior correct on restart, but shadows
  may still be frozen (old last_seen_tick).
- **Batch 3 (ECS fix + metrics):** WI-135, WI-136. Shadows reset on
  session change. End-to-end fix complete; metrics expose the state.
- **Batch 4 (verification):** WI-137, WI-138, WI-140, WI-139. Scene tests,
  audit sweep, docs.

After each batch, smoke-test with `./start.sh` + `./test.sh` and a manual
"kill one sim-server, restart it" check. Only deploy to boards after Batch 3
is complete (before that, the mesh is no-worse but no-better than today).
