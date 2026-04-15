# No-Shadows Early Return Prevents Replication Bootstrapping

## Severity: high

## Category: data-loss

## Summary

When a peer has no shadow entities on this server (new peer, post-backpressure recovery, or shadows cleaned up by staleness), `prepare_replication` returns early with empty `per_peer` and no `full_snapshot`. This creates a bootstrapping deadlock: the peer needs shadow data to appear in the shadow query, but replication is skipped because the shadow query is empty. Without `--full-snapshot` enabled, the system cannot recover from this state.

## Evidence

In `systems.rs:1204-1209`:
```rust
// If no shadows from any peer, skip replication on this tick.
// The cold tier or full snapshot (if enabled) handles recovery.
if peer_all_positions.is_empty() {
    outbound.per_peer.clear();
    outbound.full_snapshot = None;
    return;
}
```

And in `systems.rs:1216`:
```rust
for (&peer_id, _all_positions) in &peer_all_positions {
```

This iterates only peers that have shadows. If a peer is in `PeerAddresses` but has no shadow entities (registered via coordinator PeerList but has not yet sent any data, or its shadows were cleaned up), it gets zero replication.

The comment says "The cold tier or full snapshot (if enabled) handles recovery." But:
1. Full snapshot is disabled by default.
2. The cold tier is part of per-peer replication which depends on `peer_all_positions` being non-empty -- the same condition that fails here.

This is a chicken-and-egg problem: Server A cannot send entities to Server B because Server A has no shadows from Server B, and Server B cannot send shadows to Server A because Server B has no shadows from Server A.

### How it works in practice

In a normal startup:
1. Server A starts, registers with coordinator, gets PeerList with Server B.
2. Server B starts, registers, gets PeerList with Server A.
3. On Server A's first tick: `peer_all_positions` is empty (B hasn't sent anything yet). Early return. No replication to B.
4. On Server B's first tick: same -- no replication to A.
5. Neither server ever receives shadows from the other.

**Why this works today**: The initial NPC spawns happen before the first coordinator PeerList arrives. By the time peers discover each other (coordinator registration + PeerList broadcast takes ~1-2 seconds), each server has had 20-40 ticks of running. The shadow query finds entities because the OTHER server sent them via the same `prepare_replication` path before discovering THIS server. But the timing depends on startup order and network latency.

The scenario that triggers the deadlock is:
- Simultaneous startup with fast coordinator registration.
- OR: backpressure clears all shadows (finding #058), then both servers recover at the same time.
- OR: a long network partition that causes shadow cleanup on both sides.

## Impact

Two servers can enter a permanent replication deadlock where neither sends entities to the other. All cross-server visibility is lost. Entity transfers fail because the target has no shadow to promote. Crash recovery fails because there are no shadows to recover from.

This is currently masked by startup timing (servers register at different times) but becomes more likely as the cluster grows or if servers restart simultaneously.

## Recommended Fix

When a peer is registered in `PeerAddresses` but has no entries in `peer_all_positions`, send all authoritative entities to that peer unconditionally (at cold-tick rate to limit bandwidth). This bootstraps the shadow data on the peer.

In `prepare_replication`, after the `peer_all_positions` loop at line 1216, add a second loop for peers with addresses but no shadows:

```rust
// Bootstrap: peers in PeerAddresses with no shadows get all entities
// at cold-tick rate to seed their shadow data.
for (&peer_id, _addr) in peer_addrs.0.iter() {
    if peer_id == config.server_id.0 { continue; }
    if outbound.per_peer.contains_key(&peer_id) { continue; } // already handled
    if is_cold_tick {
        outbound.per_peer.insert(peer_id, entities.clone());
    }
}
```

This costs one clone of the entities Vec per unbootstrapped peer per cold tick (5Hz). Once the peer receives and processes these entities as shadows, the normal tiered path takes over.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1204-1209 (early return on empty shadows)
- crates/sim-server/src/systems.rs:1216 (peer iteration loop)
- crates/sim-server/src/resources.rs:56 (PeerAddresses resource)
- crates/sim-server/src/game_loop.rs:414 (per_peer dispatch)
