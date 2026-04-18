# Duplicate latest_tick Maps in Receiver

## Severity: low

## Category: operational

## Summary

The UDP receiver maintains two separate `latest_tick` maps that track the same information: a local `HashMap<u16, u64>` at `receiver.rs:26` used for chunk reassembly eviction, and the `PeerHealthAccumulator.latest_tick` in `SharedPeerHealth`. Both are updated on every received packet with the same value. They can drift apart if the mutex acquisition for the shared accumulator is delayed, and they have different lifetime/pruning characteristics. This duplication is a maintenance risk -- a future developer might update one and forget the other.

## Evidence

Local map at `receiver.rs:26`:
```rust
let mut latest_tick: HashMap<u16, u64> = HashMap::new();
```

Updated for `ChunkedEntityUpdate` at `receiver.rs:226`:
```rust
let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
if tick > *prev_latest {
    *prev_latest = tick;
    // Clean up stale reassemblies (more than 2 ticks behind)
    reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
}
```

Shared accumulator map updated for both `EntityStateUpdate` (receiver.rs:143) and `ChunkedEntityUpdate` (receiver.rs:219):
```rust
let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
if tick > *lt { *lt = tick; }
```

The local map is also used by the global sweep (receiver.rs:48) for per-server stale eviction, while the shared map is only read by the game loop for metrics. The two maps have identical semantics (high-water mark of tick received from each peer) but are updated at different points in the code path.

For `EntityStateUpdate` packets, the shared accumulator is updated but the local map is NOT (there is no local latest_tick update in the EntityStateUpdate arm). This means the local map only tracks ticks from `ChunkedEntityUpdate` packets, while the shared map tracks both. If a peer sends small updates (no chunking needed), the local map for that peer stays at 0 while the shared map advances normally. The global sweep uses the local map, so it would not evict stale reassembly entries for that peer based on non-chunked updates.

## Impact

The divergence between local and shared latest_tick maps is mostly harmless:
- The local map is used for reassembly eviction. If it is behind the shared map (because non-chunked updates advanced the shared one), reassembly entries are kept slightly longer than necessary. The global sweep's GLOBAL_STALE_TICKS (40) provides a safety net.
- If the shared map is behind the local (theoretically possible if the mutex lock is slow), the metrics would show slightly stale staleness values. This would be sub-tick and invisible.

The maintenance risk is the real concern: future changes to the reassembly logic might use the wrong map, or someone might "optimize" by removing one map not realizing both serve slightly different roles.

## Recommended Fix

Unify into a single source of truth. Since the local map cannot be eliminated (it is needed synchronously in the packet processing loop, and the shared mutex would add latency), document the divergence with a clear comment:

```rust
// Two latest_tick maps exist:
// 1. `latest_tick` (local) -- used for chunk reassembly eviction. Only updated
//    by ChunkedEntityUpdate packets. Safe to use without mutex.
// 2. `peer_health.stats.latest_tick` (shared) -- used for metrics reporting.
//    Updated by both EntityStateUpdate and ChunkedEntityUpdate packets.
//    Protected by std::sync::Mutex.
// The shared map may be ahead of the local map when a peer sends non-chunked
// updates (< 28 entities). This is intentional: the local map only needs to
// track chunked-update freshness for eviction purposes.
```

Alternatively, update the local map on `EntityStateUpdate` as well (one line, no downside):

```rust
PeerMessage::EntityStateUpdate { tick, server_id, entities } => {
    // Update local latest_tick (for reassembly eviction consistency)
    let lt = latest_tick.entry(server_id.0).or_insert(0);
    if tick > *lt { *lt = tick; }
    // ... rest ...
}
```

## Effort: small

## Related Files
- crates/sim-server/src/receiver.rs:26 (local latest_tick declaration)
- crates/sim-server/src/receiver.rs:131-132 (PeerHealthAccumulator definition)
- crates/sim-server/src/receiver.rs:140-147 (EntityStateUpdate: shared updated, local not)
- crates/sim-server/src/receiver.rs:216-222 (ChunkedEntityUpdate: shared updated)
- crates/sim-server/src/receiver.rs:226 (ChunkedEntityUpdate: local updated)
- crates/sim-server/src/receiver.rs:48 (global sweep uses local map)
