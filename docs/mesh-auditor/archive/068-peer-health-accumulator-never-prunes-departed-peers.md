# Peer Health Accumulator Never Prunes Departed Peers

## Severity: medium

## Category: scalability

## Summary

The `PeerHealthAccumulator` in the UDP receiver grows its `latest_tick` HashMap for every peer server_id it ever receives a packet from, but never removes entries for peers that have left the mesh. The `packets_received` and `chunk_evictions` maps are cleared every 5 seconds but `latest_tick` is explicitly preserved as a high-water mark. While the game loop filters output to only active peers when building the metrics snapshot, the accumulator itself grows without bound over the receiver's lifetime.

## Evidence

In `receiver.rs:141-147` and `receiver.rs:217-222`, every incoming packet inserts or updates an entry in `ph.latest_tick`:

```rust
let lt = ph.latest_tick.entry(server_id.0).or_insert(0);
if tick > *lt {
    *lt = tick;
}
```

In `game_loop.rs:619-625`, the 5-second reset explicitly preserves `latest_tick`:

```rust
ph.packets_received.clear();
ph.chunk_evictions.clear();
// Don't clear latest_tick -- it's a high-water mark, not a counter
```

The game loop filters output at `game_loop.rs:544-545`:

```rust
let active_peers: std::collections::HashSet<u16> =
    world.resource::<PeerAddresses>().0.keys().copied().collect();
```

Only active peers are included in the metrics snapshot. But the accumulator itself is never pruned.

Additionally, the receiver has its own local `latest_tick: HashMap<u16, u64>` (receiver.rs:26) which is separate from the `PeerHealthAccumulator.latest_tick` and is also never pruned. This map is used for chunk reassembly stale-entry eviction and grows with the same pattern.

## Impact

In the current deployment (3 fixed servers), this is a non-issue: at most 2 peer entries per server. However, in a dynamic mesh where servers come and go (cloud scaling, rolling restarts with new server IDs), the maps would accumulate entries for every server ID ever seen. With u16 server IDs, the theoretical maximum is 65,535 entries (~2 MB), but in practice the growth rate is one entry per unique server that joins the mesh.

The more immediate concern is correctness of the receiver's local `latest_tick` map: if a peer crashes and restarts with the same server_id at tick 0, the receiver still has a high latest_tick from the previous incarnation. The inline cleanup at receiver.rs:232 uses this stale high-water mark to evict chunk reassembly entries:

```rust
reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
```

If the old latest_tick was 10000 and the restarted peer is at tick 5, `tick - t <= 2` compares 5 against old entries at tick ~10000, which would underflow. Since `tick` is u64 and the subtraction is not saturating, this wraps to a very large number, so the entries are retained (not evicted). This is benign (retained, not wrongly evicted) but means stale reassembly entries from the old incarnation persist until the global sweep evicts them.

## Recommended Fix

Prune the `PeerHealthAccumulator.latest_tick` map in the game loop's 5-second reset, removing entries for peers not in the current `PeerAddresses`:

```rust
// In the 5-second reset block (game_loop.rs:619-625):
{
    let active_peers: std::collections::HashSet<u16> =
        world.resource::<PeerAddresses>().0.keys().copied().collect();
    let mut ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
    ph.packets_received.clear();
    ph.chunk_evictions.clear();
    // Prune latest_tick for departed peers
    ph.latest_tick.retain(|pid, _| active_peers.contains(pid));
}
```

This keeps the high-water mark semantics for active peers while preventing unbounded growth. The receiver's local `latest_tick` cannot be pruned from the game loop (different task, no shared access), but it is bounded by `MAX_REASSEMBLY_ENTRIES` (1000) on the reassembly map, so the practical risk is small.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:129-145 (PeerHealthAccumulator definition)
- crates/sim-server/src/game_loop.rs:619-625 (5-second reset)
- crates/sim-server/src/game_loop.rs:544-562 (active peer filtering for metrics)
- crates/sim-server/src/receiver.rs:26 (local latest_tick map)
- crates/sim-server/src/receiver.rs:141-147 (accumulator writes)
- crates/sim-server/src/receiver.rs:226-232 (local latest_tick stale eviction)
