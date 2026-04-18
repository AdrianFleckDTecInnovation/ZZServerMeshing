# Work Items 015 — Shadow Tick Split

**Design:** `docs/mesh-architect/designs/015-shadow-tick-split.md`
**Scope:** sim-server only. No wire protocol, coordinator, or client changes.
**Target branch:** `dev`
**Total size:** ~100 LOC across 5 files + 1 scene test file.

Line numbers reference the working tree as of the investigation
(2026-04-17). If a pre-existing edit has shifted lines, search for the quoted
snippet rather than trusting the number.

---

### WI-144: Split `Shadow` component into `last_peer_tick` + `last_local_tick`

**Size:** medium (~60 LOC + ~10 test updates)
**Depends on:** none
**Files:**
- `crates/sim-server/src/components.rs`
- `crates/sim-server/src/conversion.rs`
- `crates/sim-server/src/systems/inbound.rs`
- `crates/sim-server/src/systems/shadow.rs`
- `crates/sim-server/src/systems/snapshot.rs`

**Description:**

Replace the overloaded `Shadow.last_seen_tick` field with two clock-domain-
specific fields. Update every producer and every consumer to use the correct
field.

#### Step 1 — `components.rs`

At `components.rs:30-33`, change:

```rust
#[derive(Component, Debug)]
pub struct Shadow {
    pub last_seen_tick: u64,
}
```

to:

```rust
#[derive(Component, Debug)]
pub struct Shadow {
    /// Peer's tick from the most-recent accepted EntityStateUpdate. Used by
    /// the inbound stale-packet guard. Zeroed on peer session reset (WI-135).
    pub last_peer_tick: u64,
    /// This server's TickCounter when the shadow was last refreshed. Used by
    /// shadow_cleanup for wall-clock TTL. Never zeroed on peer session reset.
    pub last_local_tick: u64,
}
```

Update the in-file tests:
- `components.rs:108` → `Shadow { last_peer_tick: 10, last_local_tick: 10 }`
- `components.rs:202` → `Shadow { last_peer_tick: 99, last_local_tick: 99 }`
- `components.rs:207` → assert `shadow.last_peer_tick == 99` AND
  `shadow.last_local_tick == 99`

#### Step 2 — `conversion.rs`

At `conversion.rs:8-13`, change the `spawn_from_entity_state_direct` signature
parameter from `shadow_tick: Option<u64>` to
`shadow_ticks: Option<(u64, u64)>` where the tuple is `(peer_tick,
local_tick)`.

At `conversion.rs:25-27`, change:
```rust
entity_cmd.insert(Shadow {
    last_seen_tick: shadow_tick.unwrap_or(0),
});
```
to:
```rust
let (peer_tick, local_tick) = shadow_ticks.unwrap_or((0, 0));
entity_cmd.insert(Shadow {
    last_peer_tick: peer_tick,
    last_local_tick: local_tick,
});
```

Update `conversion.rs:157` (shadow_marker_with_tick test) and
`conversion.rs:223` (shadow_with_none_tick_defaults_to_zero test) to assert
both fields.

Update every caller of `spawn_from_entity_state_direct` to pass the new tuple
form. Easiest: grep for `spawn_from_entity_state_direct(` in the whole crate.
All producers in production code route through `inbound.rs:93` (see Step 3);
test producers pass `Some((n, n))` for simple cases or `Some((peer, local))`
for cross-clock tests.

#### Step 3 — `systems/inbound.rs`

At `inbound.rs:12`, the `_tick` binding is unused. Change to:
```rust
let local_tick = world.resource::<TickCounter>().0;
```
and remove any duplicate reads of `TickCounter` below it (there's one at
`inbound.rs:98` for the authority-transfer block — leave that one; it's for a
different scope).

At `inbound.rs:36-37`, change:
```rust
if let Some(mut shadow) = world.get_mut::<Shadow>(entity) {
    shadow.last_seen_tick = 0;
}
```
to:
```rust
if let Some(mut shadow) = world.get_mut::<Shadow>(entity) {
    shadow.last_peer_tick = 0;
    // last_local_tick intentionally untouched — local clock domain is
    // continuous across peer session changes.
}
```

Update the log message at `inbound.rs:44` from `"Reset shadow last_seen_tick
for restarted peer"` to `"Reset shadow last_peer_tick for restarted peer"` so
grep discoverability stays correct.

At `inbound.rs:68-73`, change:
```rust
if let Some(mut shadow) = world.get_mut::<Shadow>(bevy_entity) {
    if update.tick < shadow.last_seen_tick {
        continue; // stale packet
    }
    shadow.last_seen_tick = update.tick;
```
to:
```rust
if let Some(mut shadow) = world.get_mut::<Shadow>(bevy_entity) {
    if update.tick < shadow.last_peer_tick {
        continue; // stale packet
    }
    shadow.last_peer_tick = update.tick;
    shadow.last_local_tick = local_tick;
```

At `inbound.rs:93`, change:
```rust
spawn_from_entity_state_direct(world, entity_state, false, Some(update.tick));
```
to:
```rust
spawn_from_entity_state_direct(world, entity_state, false, Some((update.tick, local_tick)));
```

Update the `// WI-135` comment block at `inbound.rs:16-20` to read:
```rust
// WI-135 / design 015: Drain peer session resets FIRST so any shadow
// `last_peer_tick` values for a restarted peer are zeroed before we process
// this tick's peer_updates. Without this ordering, post-restart updates with
// low ticks would be rejected by the `update.tick < shadow.last_peer_tick`
// guard below at the cost of one additional tick of staleness.
```

Tests in this file to update (search `last_seen_tick` in-file):
- `inbound.rs:642` comment → `// equal to last_peer_tick`
- `inbound.rs:1202-1478` — WI-135 reset tests. Every
  `world.get::<Shadow>(e).unwrap().last_seen_tick` becomes `.last_peer_tick`.
  Every assertion that the reset zeroed the field stays on
  `last_peer_tick`. **Add one new assertion per reset test** that
  `last_local_tick` is unchanged by the reset (important regression guard for
  WI-135 semantics; see `peer_session_reset_zeroes_matching_shadow_last_seen_tick`
  at `inbound.rs:1219` as the canonical case). The post-reset low-tick-accept
  test around `inbound.rs:1405-1442` is now the critical cross-verification:
  low-tick packets must still be accepted (peer-tick guard works), and
  cleanup still respects them (local-tick stamp fresh).
- Any `spawn_shadow_with_tick` helper (`inbound.rs:1207-1214`) — update the
  helper to take `last_peer_tick: u64` and pass `Some((last_peer_tick, 0))`
  to `spawn_from_entity_state_direct` (callers already test peer-tick
  semantics; local=0 is fine for those cases).

#### Step 4 — `systems/shadow.rs`

At `shadow.rs:20`, change:
```rust
if tick.saturating_sub(shadow.last_seen_tick) > SHADOW_STALE_TICKS {
```
to:
```rust
if tick.saturating_sub(shadow.last_local_tick) > SHADOW_STALE_TICKS {
```

In-file tests (`shadow.rs:63-131`) currently seed via
`spawn_from_entity_state_direct(&mut world, &state, false, Some(N))`. Update
each call to `Some((N, N))` — the existing tests all run in a consistent
local-tick regime, so peer == local is correct semantics preservation.

#### Step 5 — `systems/snapshot.rs`

At `snapshot.rs:1640`, change:
```rust
Shadow { last_seen_tick: 10 },
```
to:
```rust
Shadow { last_peer_tick: 10, last_local_tick: 10 },
```

#### Step 6 — `resources.rs` + `receiver.rs` comment updates

Comments only, no behavior change. Grep-discoverability matters so future
debuggers find the right field:

- `resources.rs:61` — `(e.g., shadow last_seen_tick — see WI-135).` → `(e.g.,
  shadow last_peer_tick — see WI-135 + design 015).`
- `resources.rs:68-73` — update the `PeerSessionReset` doc to mention
  `last_peer_tick` (not `last_seen_tick`) and reference design 015.
- `receiver.rs:36, 75` — any `last_seen_tick` references in comments become
  `last_peer_tick`.

**Tests:**

Re-run the full test suite. The following must pass without further change
after the mechanical rename:
- `cargo test -p sim-server components::tests` (3 shadow-related tests)
- `cargo test -p sim-server conversion::tests`
- `cargo test -p sim-server systems::inbound::tests` (WI-135 suite, ~6 tests)
- `cargo test -p sim-server systems::shadow::tests` (4 tests)
- `cargo test -p sim-server systems::snapshot::tests`

**Done when:**
- `cargo build -p sim-server` clean.
- `cargo test -p sim-server` green.
- `rg last_seen_tick crates/` returns zero hits (field fully renamed; only
  historical references in design docs and worklogs should remain).
- `rg -n 'last_(peer|local)_tick' crates/sim-server/src/` shows the field used
  in the expected files (components, conversion, inbound, shadow, snapshot).

---

### WI-145: Add cross-clock regression unit test

**Size:** small (~30 LOC)
**Depends on:** WI-144
**Files:**
- `crates/sim-server/src/systems/shadow.rs` (add to existing `#[cfg(test)]`
  module)

**Description:**

Add a unit test that reproduces the production pathology (high local tick,
low peer tick) at minimum scope and verifies cleanup does not remove the
shadow.

```rust
/// Design 015 regression: shadow_cleanup must not treat a peer's tick as
/// our tick. Pre-015, a peer with a lower TickCounter (e.g., restarted
/// mid-session) caused `local_tick - peer_tick > SHADOW_STALE_TICKS` and
/// every replication packet triggered a despawn-then-respawn cycle that
/// showed to clients as rapid flicker.
#[test]
fn shadow_cleanup_survives_peer_tick_far_behind_local_tick() {
    let mut world = setup_world(1);
    let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
    let eid = state.id;
    // Peer is thousands of ticks behind us (simulates a peer that
    // restarted 7 minutes after us).
    spawn_from_entity_state_direct(&mut world, &state, false, Some((1_000, 59_000)));

    // Our tick is now 59_100 — 100 ticks after the refresh.
    world.resource_mut::<TickCounter>().0 = 59_100;

    shadow_cleanup(&mut world);

    assert!(
        world.resource::<EntityLookup>().0.contains_key(&eid),
        "shadow must survive: peer tick is far behind ours but we stamped \
         last_local_tick=59_000, so 59_100-59_000=100 <= SHADOW_STALE_TICKS(300)"
    );
}
```

Add a complementary negative test:

```rust
/// Design 015 complement: cleanup must still fire when the LOCAL tick has
/// moved past SHADOW_STALE_TICKS since the last refresh — regardless of
/// what peer_tick holds.
#[test]
fn shadow_cleanup_fires_when_local_tick_stale_even_if_peer_tick_recent() {
    let mut world = setup_world(1);
    let state = EntityState::new_npc(Vec3::ZERO, ServerId(2));
    let eid = state.id;
    // Peer's tick is recent relative to local (pretend we received a packet
    // at local=1000, then got stalled).
    spawn_from_entity_state_direct(&mut world, &state, false, Some((50_000, 1_000)));

    // Our tick has advanced well past the TTL.
    world.resource_mut::<TickCounter>().0 = 1_000 + SHADOW_STALE_TICKS + 1;

    shadow_cleanup(&mut world);

    assert!(
        !world.resource::<EntityLookup>().0.contains_key(&eid),
        "shadow must be removed when last_local_tick is stale regardless of peer_tick"
    );
}
```

**Tests:**

Both tests above. Run:
```sh
cargo test -p sim-server --lib systems::shadow::tests::shadow_cleanup_survives_peer_tick_far_behind_local_tick
cargo test -p sim-server --lib systems::shadow::tests::shadow_cleanup_fires_when_local_tick_stale_even_if_peer_tick_recent
```

**Done when:**
- Both new tests pass.
- Running against the pre-WI-144 codebase (mental exercise: if
  `last_local_tick` didn't exist and cleanup used `last_peer_tick` like the
  old code did), the first test would fail — i.e., the test meaningfully
  guards the fix.

---

### WI-146: Augment peer-restart-reconvergence scenario with sustained-visibility assertion

**Size:** small (~40 LOC inside existing file)
**Depends on:** WI-144
**Files:**
- `tools/test-suite/scenarios/peer-restart-reconvergence.ts`

**Description:**

The existing scenario asserts reconvergence. It does **not** assert that
shadows remain stable *after* reconvergence, which is exactly the pre-fix
failure mode (shadows reconverge within one packet, then flicker every tick).

After the existing `if (failures.length === 0) { return pass }` block, insert
a sustained-visibility check before returning. Replace:

```ts
    if (failures.length === 0) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'pass',
        summary: `S${TARGET} restart reconverged on survivors in ${(elapsed / 1000).toFixed(1)}s (session, evictions, shadow all OK)`,
        details,
        durationMs: 0,
      };
    }
```

with:

```ts
    if (failures.length === 0) {
      // Design 015 regression guard: after reconvergence, shadow_count
      // must remain stable. Pre-015, a cross-clock comparison in
      // shadow_cleanup caused ~20Hz flicker of S2-authoritative shadows
      // on S1/S3. Sample shadow_count 20 times at 100ms intervals and
      // require oscillation within ±5% of the median.
      ctx.log('Checking sustained visibility (20 samples over 2s)...');
      await ctx.sleep(3000); // settle after reconvergence
      const shadowSeries: Record<number, number[]> = {};
      for (const sid of survivorIds) shadowSeries[sid] = [];
      for (let i = 0; i < 20; i++) {
        const samples = await Promise.all(
          survivorIds.map(sid => ctx.getMetrics(sid).catch(() => null))
        );
        for (const m of samples) {
          if (m) shadowSeries[m.server_id]?.push(m.shadow_count);
        }
        await ctx.sleep(100);
      }
      const flickerFailures: string[] = [];
      for (const sid of survivorIds) {
        const series = shadowSeries[sid] ?? [];
        if (series.length < 10) continue;
        const sorted = [...series].sort((a, b) => a - b);
        const median = sorted[Math.floor(sorted.length / 2)];
        const min = sorted[0];
        const max = sorted[sorted.length - 1];
        const swing = max - min;
        // Allow ±5% of median OR ±2 entities (whichever is larger) for
        // normal rebalance/replication jitter.
        const allowed = Math.max(Math.floor(median * 0.05), 2);
        details.push(
          `  S${sid} shadow_count series: median=${median} min=${min} max=${max} swing=${swing} allowed=${allowed}`
        );
        if (swing > allowed) {
          flickerFailures.push(
            `S${sid} shadow_count oscillating: swing=${swing} > allowed=${allowed} (median=${median})`
          );
        }
      }
      if (flickerFailures.length > 0) {
        return {
          name: 'peer-restart-reconvergence',
          status: 'fail',
          summary: `Reconvergence OK but shadows flicker post-convergence: ${flickerFailures.join('; ')}`,
          details,
          durationMs: 0,
        };
      }
      return {
        name: 'peer-restart-reconvergence',
        status: 'pass',
        summary: `S${TARGET} restart reconverged + stable in ${(elapsed / 1000).toFixed(1)}s (session, evictions, shadow, sustained OK)`,
        details,
        durationMs: 0,
      };
    }
```

Keep the comment at the top of the file in sync — add a one-liner that
mentions design 015 sustained-visibility assertion.

**Tests:**

```sh
./test.sh peer-restart-reconvergence            # local cluster
./test.sh --json peer-restart-reconvergence     # JSON output
```

Then repeat against the board cluster:
```sh
./start-unoq.sh
METRICS_HOSTS=1 ./test.sh --no-start peer-restart-reconvergence
```

**Done when:**
- Scenario passes on local cluster post-WI-144.
- Scenario passes on board cluster post-WI-144.
- Intentionally reverting WI-144 (locally, not committed) causes the
  scenario to fail with a "shadows flicker post-convergence" message — this
  validates the test actually catches the bug.

---

### WI-147 (optional): shadow_despawn_count metric

**Size:** small (~20 LOC)
**Depends on:** WI-144
**Files:**
- `crates/sim-server/src/systems/shadow.rs`
- `crates/sim-server/src/resources.rs` (or wherever the metrics struct lives)
- `crates/sim-server/src/metrics_server.rs`

**Description:**

Defer unless cluster-ops asks for it. Add a monotonic `shadow_despawns_total:
u64` counter incremented inside `shadow_cleanup` per removal, surface it in
`/metrics`. In steady state this counter should grow slowly (only true peer
crashes / long stalls). Pre-fix it would grow at ~20 Hz per affected shadow.
Useful as a long-term alarm.

**Tests:**

Assert in a local unit test that `shadow_despawns_total` increments by N when
N stale shadows are cleaned. No scene test.

**Done when:**
- `/metrics` exposes `shadow_despawns_total`.
- Unit test verifies monotonic increment.

---

## Ordering & Suggested Commits

1. **Commit 1** — WI-144: the surgical field split + mechanical test updates.
   Compiles, full test suite green. This alone fixes the bug.
2. **Commit 2** — WI-145: regression unit tests.
3. **Commit 3** — WI-146: scene-test sustained-visibility assertion.
4. **Commit 4 (optional)** — WI-147: metric. Only if cluster-ops wants it.

All four commits can ship in one PR; reviewer can take them as a chain.
