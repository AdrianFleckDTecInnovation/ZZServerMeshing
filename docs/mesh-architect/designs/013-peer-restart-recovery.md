# Design 013: Peer Restart Recovery (Session/Epoch ID)

## Problem

When any sim-server in the mesh restarts, every surviving peer permanently stops
reassembling that server's UDP replication batches until the survivors are also
bounced. The restarted server's entities shadow-out on all other clients and
never come back until the entire mesh is manually restarted.

### Blast radius

- **Trigger:** any restart of any sim-server (crash, OOM, systemd restart,
  operator restart, rolling redeploy — anything that bumps the UDP source to a
  new process with a fresh tick counter).
- **Impact:** indefinite one-way desync. All of the restarted peer's entities
  remain invisible on every surviving peer's clients until *every* survivor is
  also restarted. A 2-server mesh (1 peer per server) is already fully broken;
  a 3-server mesh is half-broken on each survivor.
- **Production severity:** this converts a transient single-peer restart into a
  mesh-wide outage. With 500 entities already triggering it on the UNO Q
  hardware, the bug is active at any realistic load.
- **User-visible symptom:** S2's entities "blink in and out" on S1/S3 clients,
  shadow-out, briefly reappear, shadow-out. Eventually they stay gone.

## Evidence (captured 2026-04-17 on the UNO Q cluster)

- **~500 entities total**, S2 restarted during operation.
- On S1 and S3, `/metrics` showed `peer_health[2].latest_tick = 14730` — S2's
  pre-restart tick value.
- S2's actual post-restart counter reached only 2972, then 10061; never caught
  up to the stale latest_tick.
- `chunk_evictions[2]` climbed continuously on S1 (10) and S3 (5).
- `packets_received[2]` went from 42 up to 552 — packets were arriving, they
  were just being evicted before reassembly could complete.
- Raw-UDP flood S2→S1 delivered 100% at 1000×1300B at 400 pps — not a network
  layer problem.
- `shadow_count` on S1/S3 stuck at 534 (the healthy peer's entities only);
  S2's 506 entities were missing.
- Bouncing the mesh restored full convergence.

## Root cause

Located in `crates/sim-server/src/receiver.rs`.

### Primary bug: stuck `latest_tick` causes immediate chunk eviction

Line 26: `let mut latest_tick: HashMap<u16, u64> = HashMap::new();`

Lines 223–228 (inside the `ChunkedEntityUpdate` arm):

```rust
let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
if tick > *prev_latest {
    *prev_latest = tick;
    // inline eviction ...
}
```

This is a per-peer monotonic high-water mark that is **never reset**. There is
no code path that clears or rewinds `latest_tick` on peer disconnect,
re-registration, or any form of restart detection.

Lines 44–74 periodically sweep the reassembly map:

```rust
if server_latest.saturating_sub(t) > GLOBAL_STALE_TICKS {
    // evict
}
```

with `GLOBAL_STALE_TICKS = 40` (2 seconds at 20 Hz).

**Failure mode after a peer restart:**

1. S2's pre-restart tick peaks at, say, 14730. `latest_tick[S2] = 14730`.
2. S2 crashes/restarts. New process starts at tick 0, rises through 2972,
   10061, etc. `latest_tick[S2]` is never touched.
3. New `ChunkedEntityUpdate` arrives with `tick = 2972`. The `if tick >
   *prev_latest` branch is false (2972 < 14730), so `latest_tick` stays at
   14730.
4. The chunk goes into the reassembly map with key `(S2, 2972)`.
5. Within 100 loop iterations (< 1 ms under load), the global sweep runs and
   computes `14730.saturating_sub(2972) = 11758 > 40` → evicted.
6. Follow-up chunks for the same tick land in a fresh reassembly entry, which
   gets evicted by the next sweep before all chunks arrive.
7. Reassembly never completes. Inbound queue never gets a `PeerUpdate` for S2.
8. Result: **100% of S2's multi-chunk replication is lost indefinitely.**

This is maximally painful when many entities are owned by the restarted peer:
`ENTITIES_PER_CHUNK = 28`, so 500 entities = ~18 chunks/tick. A single chunk
needs to survive until all siblings arrive (< 1 tick in theory, but the sweep
runs much faster than that). In practice the sweep always wins.

### Secondary bug: shadow `last_seen_tick` has the same problem

`crates/sim-server/src/systems/inbound.rs` line 37:

```rust
if update.tick < shadow.last_seen_tick {
    continue; // stale packet
}
```

Even if a `PeerUpdate` did reach the ECS (single-chunk `EntityStateUpdate`
path, which hits `peer_updates` without needing reassembly), every existing
shadow would reject it because the stored `last_seen_tick` is from
pre-restart. Those shadows then age out over `SHADOW_STALE_TICKS` (300 ticks =
15 s), producing the visible "blink out" followed by nothing coming back.

Once the old shadow is evicted, a *new* shadow is spawned fresh with
`last_seen_tick = (new peer tick)`, which explains the brief "blink back in"
before the next reassembly failure cycle kills it again.

### Tertiary bug: `peer_health.latest_tick` has the same problem

`crates/sim-server/src/game_loop.rs` line 567 explicitly comments:

> Don't clear latest_tick — it's a high-water mark, not a counter

This is consistent with the receiver's design assumption: ticks are
monotonically increasing forever. The assumption is wrong as soon as any peer
restarts.

## Why the existing coordinator signals don't help

- **Heartbeat-based crash detection** (`coordinator/registry.rs` line 126): the
  coordinator detects dead servers after a 15 s timeout and broadcasts
  `RecoverEntities`. But for an operator-initiated restart that completes in
  under 15 s, the coordinator may never see a crash — the new process
  re-registers before the old registration times out.
- **`re_register_replaces_dead_connection`** (registry.rs line 879): the
  coordinator cleanly handles a re-register when the old reply channel is
  closed, but this is a coordinator↔server signal. It does **not** notify
  peers that anything changed.
- **`broadcast_peer_list`** fires on topology change, but the peer list
  content is unchanged if a server restarts and re-registers with the same
  ID/host/ports — peers may not even observe a `PeerList` message (and if
  they do, the content is identical and they'd have nothing to key off).
- **Coordinator goes down entirely:** the peers continue to replicate to each
  other over UDP with no coordinator in the path, so any fix that relies on
  coordinator notification has a single point of failure during coordinator
  outages.

## Design goals

1. **Detect peer restart without relying on the coordinator.** The fix must
   work peer-to-peer over the same UDP path that's already broken.
2. **Deterministic, not heuristic.** No thresholds to tune, no "probably a
   restart" guesses.
3. **Self-healing within bounded time.** Survivors must reconverge shadows for
   the restarted peer within a few ticks of its first replicated tick, not 15 s
   later once shadow cleanup evicts the old state.
4. **Backward compatible during rolling redeploy.** A mixed mesh with one
   old-protocol server and one new-protocol server must not break worse than
   today (ideally: new<->old behaves like old<->old, i.e. today's broken-but-
   recoverable-by-full-bounce behavior).
5. **Minimal per-tick cost.** Each PeerMessage already carries server_id +
   tick. Adding one u64 per message is ~8 bytes — negligible.

## Approaches evaluated

### (a) Session/epoch ID in PeerMessage — RECOMMENDED

Add a `session_id: u64` (random nonce generated once at sim-server startup) to
every `PeerMessage` variant. The receiver keys its reassembly map, its
`latest_tick` map, and its `peer_health.latest_tick` by
`(server_id, session_id)` instead of just `server_id`. The shadow
`last_seen_tick` check becomes `(session_id, tick)` — a different
`session_id` means "this is a fresh instance, accept the update and reset the
monotonic guard."

**How it works:**

- On sim-server boot, generate `SESSION_ID = rand::random::<u64>()`.
- Every outbound PeerMessage carries that `session_id`.
- Receiver:
  - Reassembly key: `(server_id, session_id, tick)`.
  - `latest_tick`: `HashMap<(u16, u64), u64>` keyed by
    `(server_id, session_id)`.
  - On seeing a new `(server_id, session_id)` pair where the server_id has a
    prior session stored:
    - Drop all reassembly entries for the old session.
    - Reset `latest_tick` for that server to the new session's tick.
    - Walk shadows owned by that server and reset `last_seen_tick` to 0 (or
      mark them "stale — accept next update unconditionally").
    - Log at `info` level: `peer S2 new session: old=0x1234 new=0x5678`.
  - Entities from the old session that don't appear in the first new-session
    update are allowed to age out via `SHADOW_STALE_TICKS` — they're genuinely
    gone if the new authority no longer owns them.

**Wire cost:** +8 bytes per PeerMessage. At 500 entities / 18 chunks / peer /
tick × 3 peers × 20 Hz = 1080 messages/sec. +8 KB/s total. Negligible.

**Pros:**
- Deterministic: restart detection is identity-based, not heuristic.
- Self-healing: the first packet of the new session immediately clears stale
  state; no wait for timeouts.
- Works with coordinator down.
- Also fixes a latent class of bugs where a rogue/delayed packet from a
  previous session could corrupt a new session's state.

**Cons:**
- Protocol change: requires incrementing `PROTOCOL_VERSION` from 1 to 2.
- Rolling redeploy awkwardness: old-version packets (without the field) get
  rejected by the version check on a new-version receiver, and vice versa. See
  "Migration" below.
- Adds a new field to every PeerMessage variant — six variant definitions to
  update and their test coverage to extend.

### (b) Heuristic backward-jump detection

If an incoming `tick` is less than the stored `latest_tick` by more than some
threshold (e.g., `tick + 500 < latest_tick`), assume a restart and reset state.

**Pros:**
- No protocol change.
- No wire cost.
- Works with coordinator down.

**Cons:**
- Brittle threshold. What value? `500`? If a peer is only mildly ahead and
  restarts, the new tick may not be small enough relative to the old tick to
  trigger reset.
- **Race window:** between restart and "threshold crossed," the mesh is still
  broken for some window. If the threshold is `500`, that's 25 s of continued
  desync at 20 Hz.
- Easy to trip accidentally: an out-of-order UDP packet from 30 s ago (unlikely
  in LAN but possible after a momentary network partition) could falsely
  trigger a reset, wiping legitimate state.
- Doesn't help with packet confusion if a residual UDP packet from the *old*
  session arrives after the new session has already bumped latest_tick up past
  it.

### (c) Coordinator-driven "peer restart" notification

Add a `PeerRestart { server_id }` message the coordinator broadcasts when it
detects a re-registration. Receivers clear state for that peer on the hint.

**Pros:**
- Piggybacks on an existing control path (coordinator↔server TCP/WebSocket).
- No data-plane protocol change.

**Cons:**
- **Race:** UDP can arrive before or after the WebSocket hint. A dropped
  WebSocket message (it shouldn't happen, but the coordinator process can die)
  leaves the mesh broken without any self-healing mechanism.
- **Single point of failure:** if the coordinator is down when a peer
  restarts, the hint never goes out. The whole point of peer-to-peer
  replication is that it tolerates coordinator outages.
- **Can't distinguish two restarts vs one:** if a peer restarts twice in
  quick succession, the coordinator may collapse both into a single notify.
- Doesn't cover the "new process, same registration" case cleanly — the
  coordinator currently only replaces a dead connection if the old reply_tx
  is closed; if the old process exited cleanly and a new one took its place
  immediately, the coordinator sees a reject (see `duplicate_register_with_
  live_connection_rejected`) or a replace, and the semantics depend on which
  connection event arrives first.

### (d) Time-based eviction instead of tick-based

Replace `server_latest.saturating_sub(t) > GLOBAL_STALE_TICKS` with "evict if
`received_at` is older than 2 seconds." Stop tracking `latest_tick` in the
receiver entirely.

**Pros:**
- Removes the broken monotonic assumption altogether.
- No protocol change.

**Cons:**
- Still doesn't fix the `last_seen_tick` bug in shadow updates in
  `inbound.rs`. Shadows would still reject all new-session updates until the
  15 s cleanup runs.
- Still doesn't fix `peer_health.latest_tick`, so metrics lie during a
  restart window.
- Partially solves the symptom (reassembly works) but leaves shadows
  permanently frozen from the ECS side.

## Recommendation: Approach (a) with the fallback hardening from (d)

**Primary fix: session_id field in PeerMessage.**

In parallel, apply a smaller change from (d): when the receiver sees a new
session for a known server, it explicitly clears the reassembly map entries
for that `(server_id, *)`, the `latest_tick`, and the `peer_health.latest_tick`
for that server. This is cheap and prevents stale-session packets from
confusing the new session if they arrive interleaved.

## Files and fields changed

### `crates/shared/src/lib.rs`

- `PROTOCOL_VERSION: u16 = 1` → `2`.

### `crates/shared/src/protocol.rs`

Every variant of `PeerMessage` gains `session_id: u64`:

```rust
pub enum PeerMessage {
    EntityStateUpdate {
        tick: u64,
        server_id: ServerId,
        session_id: u64,         // NEW
        entities: Vec<EntityState>,
    },
    AuthorityTransfer {
        tick: u64,
        session_id: u64,         // NEW
        entity_id: EntityId,
        from_server: ServerId,
        to_server: ServerId,
        full_state: EntityState,
    },
    ForwardedInput {
        session_id: u64,         // NEW
        entity_id: EntityId,
        direction: [f32; 3],
        seq: u32,
    },
    ForwardedRemove {
        session_id: u64,         // NEW
        entity_id: EntityId,
    },
    TransferAck {
        session_id: u64,         // NEW
        entity_id: EntityId,
        tick: u64,
    },
    ChunkedEntityUpdate {
        tick: u64,
        server_id: ServerId,
        session_id: u64,         // NEW
        chunk_index: u16,
        total_chunks: u16,
        entities: Vec<EntityState>,
    },
}
```

All existing `peer_message_*_bincode_roundtrip` tests need the new field.

### `crates/sim-server/src/main.rs` (or wherever `ServerConfig`/startup lives)

At process boot, generate `let session_id: u64 = rand::random();` and store in
a new resource `SessionId(pub u64)` inserted into the ECS world. Log it on
startup: `tracing::info!(session_id = format!("0x{:x}", sid), "sim-server
session id");`.

### `crates/sim-server/src/resources.rs`

Add:

```rust
#[derive(Resource, Clone, Copy)]
pub struct SessionId(pub u64);
```

### `crates/sim-server/src/systems/replication.rs` (and any other PeerMessage producer)

Every PeerMessage construction site reads the `SessionId` resource and passes
it into the new `session_id` field. Sites:

- `systems/replication.rs` — `EntityStateUpdate` and `ChunkedEntityUpdate`
  paths (via `enqueue_entities_to_peer` in `game_loop.rs` line 349). The
  session id can be threaded through `enqueue_entities_to_peer` as a new
  parameter, or fetched once per tick and stored in `OutboundReplication`.
- `systems/transfers.rs` — `AuthorityTransfer` and `TransferAck`.
- `systems/input.rs` — `ForwardedInput` and `ForwardedRemove`, `send_transfer_ack`.

### `crates/sim-server/src/receiver.rs`

Significant rewrite of the state maps:

```rust
// Per-peer session tracking. Key changes from u16 to (u16, u64).
let mut session_of_peer: HashMap<u16, u64> = HashMap::new();  // current known session per server
let mut reassembly: HashMap<(u16, u64, u64), ChunkReassembly> = HashMap::new();
// ^ (server_id, session_id, tick)
let mut latest_tick: HashMap<(u16, u64), u64> = HashMap::new();
// ^ (server_id, session_id) -> tick

// When handling any PeerMessage that carries (server_id, session_id):
fn note_session(
    server_id: u16,
    session_id: u64,
    session_of_peer: &mut HashMap<u16, u64>,
    reassembly: &mut HashMap<(u16, u64, u64), ChunkReassembly>,
    latest_tick: &mut HashMap<(u16, u64), u64>,
    peer_health: &Arc<SharedPeerHealth>,
    inbound: &Arc<SharedInbound>,
) {
    match session_of_peer.get(&server_id).copied() {
        Some(prev) if prev != session_id => {
            tracing::info!(server_id, old=%format!("{:x}", prev), new=%format!("{:x}", session_id),
                "Peer session changed; clearing stale state");
            reassembly.retain(|&(sid, ssid, _), _| !(sid == server_id && ssid == prev));
            latest_tick.remove(&(server_id, prev));
            {
                let mut ph = peer_health.lock().unwrap_or_else(|e| e.into_inner());
                ph.latest_tick.remove(&server_id);
                ph.packets_received.remove(&server_id);
                ph.chunk_evictions.remove(&server_id);
            }
            // Signal the ECS to reset shadow last_seen_tick for entities
            // owned by this server. See SessionReset below.
            let mut q = inbound.lock().unwrap_or_else(|e| e.into_inner());
            q.peer_session_resets.push(PeerSessionReset { server_id, new_session: session_id });
        }
        _ => {}
    }
    session_of_peer.insert(server_id, session_id);
}
```

The global sweep also uses the new composite key; eviction only affects
`(sid, ssid, t)` entries where `latest_tick[(sid, ssid)]` is set and far
behind.

### `crates/sim-server/src/resources.rs` — InboundQueue

Add a new field:

```rust
pub peer_session_resets: Vec<PeerSessionReset>,
```

with a new struct:

```rust
pub struct PeerSessionReset {
    pub server_id: ServerId,
    pub new_session: u64,
}
```

### `crates/sim-server/src/systems/inbound.rs`

At the start of `drain_inbound_events`, drain `peer_session_resets` and for
each reset:

- Walk all shadow entities with `AuthorityServer == server_id`.
- Reset their `Shadow.last_seen_tick` to 0 (or to `u64::MAX` then check on
  first update — simpler is 0).

This ensures the first update from the new session isn't rejected by the
`if update.tick < shadow.last_seen_tick` guard.

### `crates/sim-server/src/game_loop.rs`

- `PeerHealthAccumulator` can stay keyed by `u16` (stable server id) because
  the session reset clears it explicitly.
- The comment at line 567 (`"Don't clear latest_tick — it's a high-water
  mark, not a counter"`) stays true as a reset-window principle; it's now
  correct because explicit session-change resets do the clearing.

### Client-side impact: none

Clients don't see `PeerMessage` — they consume binary snapshots from their
local server. The session_id is a pure server-to-server concern.

## Protocol backward-compat and rolling redeploy

### Status quo

`shared::PROTOCOL_VERSION` is prefixed on every UDP datagram (receiver.rs
lines 115–123) and mismatched versions are dropped with a warning.

### Plan

Bump `PROTOCOL_VERSION` from 1 to 2 for this change.

**Consequence of mixed-version mesh during a rolling redeploy:**

- Old-version (v1) server → new-version (v2) server: v2 receiver sees
  `wire_version = 1`, logs warning, drops the packet. The v2 server cannot see
  the v1 server's entities.
- New-version (v2) server → old-version (v1) server: v1 receiver sees
  `wire_version = 2`, logs warning, drops the packet. The v1 server cannot
  see the v2 server's entities.

This is *worse than the current bug-free case* (full mesh) but *no worse than
the current buggy case* (restarted peer desynced until bounce) — during a
rolling deploy, each upgraded server is effectively "restarted" anyway.

**Mitigation for rolling redeploys:**

Option 1 (simpler): deploy all sim-servers in one short window. The boards
already do this via `start-unoq.sh`, which is our primary deployment. Accept
~5–10 s of cross-version invisibility during the window.

Option 2 (future, not in scope): support two protocol versions at once
(accept v1 and v2, emit only v2). Adds complexity; defer until we have a
reason beyond this one change.

Recommend **Option 1**. Document in the rollout procedure that cross-version
traffic is dropped until all servers are upgraded.

### Coordinator protocol: unchanged

This change is purely in the UDP data-plane protocol. `CoordClientMsg` and
`CoordServerMsg` are untouched. Coordinator and sim-servers of mixed
versions can still talk control-plane to each other.

## Failure modes

- **Random session_id collision across restarts:** probability of collision
  with a u64 is ~1 in 2^64. Negligible.
- **Session_id collision across two different servers at the same time:**
  Also negligible, and irrelevant — the receiver keys on
  `(server_id, session_id)`, so a collision across different server_ids is
  not a source of confusion.
- **Attacker forges a session_id change:** UDP is already unauthenticated;
  an attacker who can inject bincode PeerMessages can already wreak havoc
  with authority transfers. Not a new attack surface.
- **Shadow last_seen_tick reset resurrects a gone entity:** if the new
  session's first update doesn't include an entity that was present in the
  old session, the shadow for it will linger (reset to 0, aging out over 15 s
  via `SHADOW_STALE_TICKS`). Acceptable — this is no worse than today.
- **Coordinator down during a peer restart:** fix works entirely peer-to-
  peer. No regression vs current behavior. Current behavior is already broken
  in this case; the fix works.
- **Out-of-order UDP: old-session packet arrives after new-session packet
  has bumped state:** receiver checks `session_of_peer` — if the incoming
  `session_id` doesn't match the current known session, and the incoming
  session isn't *newer* (we don't have a "newer" ordering — sessions are
  random), we drop the packet with a warning.

  Subtlety: we can't tell from the packet alone whether the old session's
  packet is from "before the restart" or "some future second restart". We
  simply track the current known session and drop anything that doesn't match
  it. If two restarts happen in quick succession (rare), the middle session's
  stragglers drop; correct behavior.

## Test coverage needed

### Unit tests (`crates/sim-server/src/receiver.rs`)

- **Restart detection:** feed the receiver a packet at `(server=2, session=A,
  tick=14730)`, then a packet at `(server=2, session=B, tick=100)`. Assert
  that the reassembly map is cleared for session A and the tick-100 chunk is
  kept.
- **Stale session reject:** feed packets `(session=A, tick=100)`,
  `(session=B, tick=200)`, `(session=A, tick=101)` in order. Assert the
  middle (B) packet is processed, and the third (A) packet is dropped (or
  at least doesn't disturb B's state).
- **Session reset signals inbound queue:** after a session change, assert
  that `PeerSessionReset { server_id=2, new_session=B }` is pushed to the
  inbound queue.
- **Latest-tick keying:** feed two packets with the same `(server, session)`
  and different ticks; assert `latest_tick[(server, session)]` reflects the
  max. Feed the same `tick` with a different session; assert a separate
  entry.

### Unit tests (`crates/sim-server/src/systems/inbound.rs`)

- **PeerSessionReset clears shadow last_seen_tick:** spawn a shadow for
  server 2 with `last_seen_tick = 500`. Push a `PeerSessionReset` for
  server 2. Run `drain_inbound_events`. Assert `last_seen_tick = 0` on that
  shadow.
- **PeerSessionReset only affects that server's shadows:** spawn shadows for
  server 2 and server 3. Reset server 2's session. Assert server 3's shadow
  `last_seen_tick` is unchanged.

### Unit tests (`crates/shared/src/protocol.rs`)

- Add `session_id` round-trip assertion to every existing
  `peer_message_*_bincode_roundtrip` test.
- Add a new test confirming bincode wire size grew by exactly 8 bytes per
  message (sanity that we added a u64, not accidentally something larger).

### Scene/integration tests (`scene-tests/` or `test.sh`)

- **peer-restart-reconvergence:** start a 3-server cluster, spawn 500
  entities across them, kill S2 mid-run, restart S2, assert that on S1 and
  S3 the shadow count for S2's entities returns to its pre-restart value
  within N ticks (target: < 40 ticks = 2 s, conservatively 100 ticks).
  This test is the direct acceptance gate for the bug.
- **peer-restart-twice:** as above, but restart S2 twice in 5 s. Assert
  shadow convergence still reaches steady state. Ensures the session tracking
  isn't confused by two session changes.
- **peer-restart-during-chunk:** restart S2 at a moment when S1/S3 have
  partial reassembly entries. Assert the partial entries are cleared and
  don't leak memory.

### Metrics verification

- After restart, `/metrics peer_health[2].latest_tick` should track S2's new
  tick (not the pre-restart high-water mark).
- `chunk_evictions` should rise by at most `N` where N is the number of
  in-flight chunks at the moment of restart (i.e., bounded by the reassembly
  capacity, not unbounded).
- On the UNO Q cluster: restart a board, verify convergence via the
  dashboard within the 2 s target.

## Migration / rollout

1. Implement all work items on `dev`.
2. Run local `./start.sh` + `./test.sh` + the new `peer-restart-reconvergence`
   scene test.
3. Deploy all 4 boards (coordinator + 3 sim-servers) in one `./start-unoq.sh`
   cycle — this is already the norm. Cross-version window is a few seconds.
4. Verify on the cluster: kill a board's sim-server service, restart it,
   confirm shadows reconverge on the survivors within 2 s (vs. the current
   indefinite desync).
5. Keep the pre-change audit finding (if present in `docs/mesh-auditor/`)
   linked in the PR so the regression test can reference it.

## Deferred / follow-on work (not in this design)

- Binary header compression: we now carry (version=2 bytes) + (server_id=2
  bytes from inside the message) + (session_id=8 bytes) + (tick=8 bytes) on
  every message. If we ever decide to optimize, a fixed header prefix outside
  the bincode payload would let the receiver key before deserializing the
  whole message. Not worth it now.
- Session_id exposed in `/metrics`: useful for debugging, trivial to add.
  Include in WI list below.
- Session_id in `CoordClientMsg::Register`: the coordinator could cross-
  reference session_id changes with its own re-registration detection and
  warn if they disagree. Nice-to-have, defer.
