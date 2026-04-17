# Receiver Acquires Mutex Up To Three Times Per Packet

## Severity: low

## Category: bottleneck

## Summary

The UDP receiver acquires the `SharedPeerHealth` mutex separately for health tracking, chunk eviction counting, and chunk reassembly eviction counting -- up to three separate lock/unlock cycles for a single `ChunkedEntityUpdate` packet. While each acquisition is sub-microsecond, the pattern creates unnecessary contention points with the game loop, which also acquires this mutex twice per tick (Phase 6 read + 5-second reset).

## Evidence

For a `ChunkedEntityUpdate` packet that advances the latest tick and triggers inline eviction, the receiver performs these mutex acquisitions in sequence:

1. **Health tracking** at `receiver.rs:216-222`:
```rust
{
    let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
    *ph.packets_received.entry(server_id.0).or_insert(0) += 1;
    let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
    if tick > *lt { *lt = tick; }
}
```

2. **Inline eviction counting** at `receiver.rs:234-237`:
```rust
if inline_evicted > 0 {
    let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
    *ph.chunk_evictions.entry(server_id.0).or_insert(0) += inline_evicted as u32;
}
```

For `EntityStateUpdate` packets, there is one acquisition at `receiver.rs:140-147`.

Additionally, the periodic sweep at `receiver.rs:70-73` acquires the mutex for sweep eviction counting:
```rust
let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
for (sid, count) in sweep_evictions {
    *ph.chunk_evictions.entry(sid).or_insert(0) += count;
}
```

And the hard cap eviction at `receiver.rs:95-99` does the same.

In the worst case (chunked packet + inline eviction + periodic sweep in the same iteration), the receiver acquires the mutex 3 times in a single loop iteration.

## Impact

At current scale (3 servers, ~20 packets/second per peer = ~40 packets/second total), this is negligible. Each mutex acquisition takes ~50-200ns on an uncontended std::sync::Mutex. Three acquisitions add ~600ns per packet at most.

The contention risk is with the game loop's Phase 6, which holds the mutex for the duration of building the `health_map` HashMap (game_loop.rs:546-562). If the receiver happens to arrive at a lock() call while Phase 6 is iterating, it blocks for O(P) where P is the number of active peers. At 3 peers, this is sub-microsecond. At 20 peers, it could be a few microseconds. Neither is a concern.

The real concern is on the UNO Q boards (single-core ARM), where the tokio multi-threaded runtime runs on fewer worker threads. If the game loop task and receiver task are on the same worker thread, the std::sync::Mutex will never actually contend (tokio doesn't preempt synchronous code). If on different threads, the ARM's weaker memory bus makes mutex operations slightly more expensive (~500ns vs ~200ns on x86).

This is a code cleanliness issue, not a performance bug.

## Recommended Fix

Batch the health tracking and eviction counting into a single mutex acquisition per packet. For `ChunkedEntityUpdate`, combine the health tracking lock with the eviction counting:

```rust
PeerMessage::ChunkedEntityUpdate { tick, server_id, chunk_index, total_chunks, entities } => {
    let key = (server_id.0, tick);

    // Update latest tick and compute inline evictions
    let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
    let mut inline_evicted = 0;
    if tick > *prev_latest {
        let before = reassembly.len();
        *prev_latest = tick;
        reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
        inline_evicted = before - reassembly.len();
    }

    // Single mutex acquisition for all peer health updates
    {
        let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
        *ph.packets_received.entry(server_id.0).or_insert(0) += 1;
        let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
        if tick > *lt { *lt = tick; }
        if inline_evicted > 0 {
            *ph.chunk_evictions.entry(server_id.0).or_insert(0) += inline_evicted as u32;
        }
    }
    // ... rest of reassembly logic ...
}
```

## Effort: small

## Related Files
- crates/sim-server/src/receiver.rs:140-147 (EntityStateUpdate health tracking)
- crates/sim-server/src/receiver.rs:216-222 (ChunkedEntityUpdate health tracking)
- crates/sim-server/src/receiver.rs:234-237 (inline eviction counting -- separate lock)
- crates/sim-server/src/receiver.rs:70-73 (sweep eviction counting)
- crates/sim-server/src/receiver.rs:95-99 (hard cap eviction counting)
- crates/sim-server/src/game_loop.rs:546 (Phase 6 reads the same mutex)
- crates/sim-server/src/game_loop.rs:621 (5-second reset acquires the same mutex)
