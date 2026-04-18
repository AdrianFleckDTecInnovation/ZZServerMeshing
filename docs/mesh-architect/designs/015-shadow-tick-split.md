# Design 015 — Split `Shadow.last_seen_tick` Into Peer-Tick and Local-Tick

**Status:** proposed
**Related:** design 013 (peer-restart-recovery), WI-135 (session-id shadow reset)
**Scope:** single-field rename + one new field, all local in-memory bookkeeping
**Wire protocol:** unchanged (no PROTOCOL_VERSION bump)
**Target LOC:** ~100 including test updates

---

## 1. Problem

`Shadow.last_seen_tick` (`crates/sim-server/src/components.rs:32`) is overloaded
for two incompatible purposes that live in two different clock domains:

### Use 1 — stale-packet guard (peer-tick domain)
`crates/sim-server/src/systems/inbound.rs:70-73`

```rust
if update.tick < shadow.last_seen_tick {
    continue; // stale packet
}
shadow.last_seen_tick = update.tick;  // set to PEER tick
```

Correct per-peer conflict resolution: ignore a UDP packet whose `tick` is older
than the newest one we've already applied for this entity. The "clock" here is
the authoring peer's `TickCounter`.

### Use 2 — stale-shadow cleanup (local-tick domain)
`crates/sim-server/src/systems/shadow.rs:14-23`

```rust
let tick = world.resource::<TickCounter>().0;  // LOCAL tick
if tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS {
    // remove shadow
}
```

Correct wall-clock TTL: if we haven't received an update for 15 s (300 ticks),
age the shadow out. The "clock" here is *our own* `TickCounter`.

The bug is the cross-clock comparison. `shadow.last_seen_tick` holds the peer's
tick, but cleanup treats it as our tick. Whenever peers' `TickCounter`s differ
(staggered boots, restarts, the coordinator coming back up), the subtraction is
meaningless.

## 2. Observable Symptom (live cluster, 2026-04-17)

- S1 running since T0, `TickCounter = 56,608`.
- S2 restarted ~7 min later, `TickCounter = 48,451`.
- Every S2→S1 `EntityStateUpdate` carries `tick ≈ 48,451`.
- S1's inbound handler sets `shadow.last_seen_tick ≈ 48,000` on each S2 shadow.
- S1's `shadow_cleanup` computes `56,608 − 48,000 = 8,608 > 300` every tick and
  despawns every S2 shadow.
- The next S2 packet respawns them.
- Clients connected to S1 see S2-authoritative entities (blue) flash in/out at
  ~20 Hz. User report: "blue entities fucking flashing."

## 3. Why This Was Masked Before Design 013

Pre-013, the receiver's peer-tick tracking didn't reset across peer restarts —
it stayed stuck at the peer's pre-restart tick (which was close to ours).
`shadow.last_seen_tick` thus accidentally stayed numerically close to our own
`TickCounter`, so the cleanup subtraction produced small values and the bug was
invisible. Design 013 + WI-135 correctly reset peer-tick state on restart
(zeroing `last_seen_tick` when a new `session_id` arrives), which exposed the
latent cleanup-clock bug: now the field *does* track the peer honestly, which
is exactly what breaks the cleanup arithmetic.

This is not a regression of design 013; it's a pre-existing bug design 013
uncovered by fixing the adjacent one.

## 4. Proposed Fix

Split `Shadow.last_seen_tick` into two fields with disjoint semantics:

```rust
#[derive(Component, Debug)]
pub struct Shadow {
    /// The peer's tick from the most recent accepted update. Used by the
    /// stale-packet guard in systems::inbound to reject out-of-order UDP
    /// packets from the same peer. Zeroed on peer session reset (WI-135).
    pub last_peer_tick: u64,

    /// Our local TickCounter when this shadow was last refreshed. Used by
    /// systems::shadow::shadow_cleanup for wall-clock TTL. NEVER reset
    /// by session events — our clock is continuous even when peers restart.
    pub last_local_tick: u64,
}
```

### Call-site transforms

**`inbound.rs:70-73` (stale-packet guard + refresh):**
```rust
let local_tick = world.resource::<TickCounter>().0; // read once above the loop
...
if update.tick < shadow.last_peer_tick {
    continue;
}
shadow.last_peer_tick = update.tick;
shadow.last_local_tick = local_tick;
```

**`inbound.rs:37` (WI-135 session reset):**
```rust
shadow.last_peer_tick = 0;           // unchanged intent
// last_local_tick intentionally left alone — it's in our clock domain.
```

**`shadow.rs:20` (cleanup):**
```rust
if tick.saturating_sub(shadow.last_local_tick) > SHADOW_STALE_TICKS {
```

**`conversion.rs:25-27` (spawn helper):** the `shadow_tick: Option<u64>`
parameter currently doubles as both "peer tick" (when called from `inbound.rs`
path, passing `update.tick`) and "seed tick" (when called from tests). Split
semantics into:

```rust
pub fn spawn_from_entity_state_direct(
    world: &mut World,
    state: &EntityState,
    is_authoritative: bool,
    shadow_ticks: Option<(u64, u64)>,  // (peer_tick, local_tick)
) -> Entity
```

Producer in `inbound.rs:93` becomes `Some((update.tick, local_tick))`. Tests
updating this call pass `Some((peer, local))` explicitly.

## 5. Alternatives Considered

### A. `std::time::Instant`-based cleanup (rejected)
Replace `last_seen_tick` with `last_seen_at: Instant`. Cleanup compares to
`Instant::now()`. Eliminates any tick-math confusion.

**Why not:** forces every shadow to carry an `Instant` (16 bytes vs 8), imports
wall-clock into the deterministic ECS tick loop (today everything measures in
ticks; adding Instant-based staleness is a new concept that breaks symmetry
with other tick-based thresholds like `SHADOW_STALE_TICKS`, replication
cadence, etc.), and doesn't compose with future features that might want
"time-of-last-update in ticks" for telemetry. Could do it later if we ever need
sub-tick precision, but today it's extra complexity without a payoff.

### B. Normalize peer-tick into local-tick at receive (rejected)
Keep a single `last_seen_tick` field but set it to the local tick at accept
time (`shadow.last_seen_tick = local_tick`). The stale-packet guard becomes
"have we seen a packet from this peer this tick" and loses per-packet
resolution.

**Why not:** loses the stale-packet guard. Two UDP packets from the same peer
arriving in the same local tick with out-of-order `update.tick` values would
both be applied because `local_tick < local_tick` is false either way. The
guard only works when we compare peer-tick to peer-tick.

### C. Per-peer last-seen map (rejected)
Store a `HashMap<ServerId, u64>` of peer-tick-per-peer as a resource and keep
cleanup based on local tick stamped on the shadow.

**Why not:** more infrastructure than needed. The shadow entity already has a
perfect home for both fields; adding a side map duplicates state and needs
invalidation on shadow despawn. Split-field on the component wins on locality.

### D. Keep one field, stamp with local tick, add a separate peer-tick map
Stamp `last_seen_tick` with local tick (used by cleanup), and keep a parallel
`HashMap<(ServerId, EntityId), u64>` for the stale-packet guard.

**Why not:** same problem as C — two stores for one concept. Split-field on the
component gives both guarantees with one allocation per shadow (which is what
we already have).

### E. Chosen: split-field (this design)
Two `u64`s on the component, each tagged to its clock domain. One extra 8-byte
field per shadow, zero new infrastructure, each call-site trivially maps to the
correct semantic. Tests change ~10 lines to pass `(peer, local)` instead of a
single tick.

## 6. Failure Modes

- **Peer packet arrives with `update.tick == last_peer_tick` (tie):** accepted
  (the `<` guard is strict; unchanged from today — see
  `peer_update_equal_tick_accepted` test at `inbound.rs:630`).
- **Peer session reset arrives between shadow spawn and first post-reset
  update:** `last_peer_tick` zeroed, `last_local_tick` unchanged. Cleanup keeps
  the shadow alive for up to 15 s of "no update received" — correct.
- **Peer never reconnects after session reset:** `last_local_tick` keeps its
  old value; after 300 local ticks the shadow ages out. Correct.
- **Local tick overflow (u64 saturating subtraction):** identical to today; no
  new overflow surface.
- **Shadow spawned with `shadow_ticks = None` (tests):** defaults to
  `(0, 0)` — matches current behavior where `last_seen_tick` defaulted to 0.
- **Clock skew across servers narrower than before:** cleanup no longer depends
  on peer-tick proximity, so staggered starts / restarts no longer cause
  spurious cleanup.

## 7. Metrics / Observability

No new metrics; the fix resolves an existing user-visible flicker. Existing
`peer_health[N].latest_tick` continues to reflect the peer's tick (unchanged).

Optionally (not required for this design), `shadow_count` gauge movement
becomes *smoother* after this fix — the flicker creates high-frequency
oscillation today that disappears post-fix. If we want a regression signal
beyond the scene test, we could add a `shadow_despawn_count` counter that
stays near zero in steady state. Deferred.

## 8. Migration / Deployment

- Single-binary local change. Not a wire format change.
- Safe to deploy one board at a time; no cross-version interop concern.
- No coordinator changes.
- No client changes.

## 9. Test Strategy

**Unit tests** (updated):
- `components.rs` tests that seed `Shadow { last_seen_tick: N }` → update to
  seed both fields (`last_peer_tick: N, last_local_tick: N` where applicable).
- `conversion.rs` shadow-spawn tests → pass `Some((peer, local))`.
- `shadow.rs` cleanup tests → seed `last_local_tick` specifically, verify
  cleanup uses it (not peer-tick).
- `inbound.rs` stale-packet tests → seed `last_peer_tick` specifically, verify
  guard uses it. WI-135 session-reset tests must verify `last_peer_tick` goes
  to zero AND `last_local_tick` is untouched.
- `snapshot.rs:1640` test seeds `Shadow { last_seen_tick: 10 }` → trivially
  updated.

**New unit test** (cross-clock regression):
- `shadow_cleanup_survives_peer_tick_far_behind_local_tick` — create a shadow
  with `last_peer_tick = 1000`, `last_local_tick = 59_000`, set local tick to
  `59_100`, run `shadow_cleanup`, assert shadow survives. This is the exact
  production pathology reduced to a unit test.

**Scene test** (regression guard):
- Augment `tools/test-suite/scenarios/peer-restart-reconvergence.ts` with a
  post-reconvergence **sustained-visibility** assertion: after `sessionOk &&
  evictionsStable && shadowOk`, sleep 3 s, then poll survivors 20 times at 100
  ms intervals. For each survivor, track the min and max of
  `peer_health[TARGET].packets_received` over the window. Shadow *count* isn't
  directly exposed per-peer, but we can assert that `shadow_count` doesn't
  oscillate by more than ±5% of its median across those 20 samples. The
  pre-fix code shows huge oscillation; the fix shows a flat line.

## 10. Rollout Plan

1. Ship WI-144 (field split + all call-sites + test updates) as one commit.
2. Ship WI-145 (cross-clock unit test) in the same or next commit.
3. Ship WI-146 (scene test augmentation) so CI catches regressions.
4. Deploy to boards via `./start-unoq.sh`. Watch that the observed symptom
   ("blue entities flashing") is gone in the client.
5. Check `shadow_count` in `/metrics` on S1 and S3 — should be stable to within
   a few entities across a 30-second window, not oscillating hundreds.
