# Chunk Reassembly Memory Leak on Incomplete Chunks

## Severity: high

## Category: scalability

## Summary
The UDP receiver's chunk reassembly map (`reassembly: HashMap<(u16, u64), ChunkReassembly>`) grows without bound when chunks from a tick never fully arrive. The cleanup heuristic only triggers when a newer tick arrives from the same server, and the "2 ticks behind" window is too narrow for real packet loss scenarios. If a server is unreachable for an extended period, or if packet loss is bursty, incomplete reassembly entries accumulate and are never freed.

## Evidence

The reassembly cleanup in receiver.rs:118-123:
```rust
let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
if tick > *prev_latest {
    *prev_latest = tick;
    // Clean up stale reassemblies (more than 2 ticks behind)
    reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
}
```

**Problem 1: Cleanup requires forward progress.** If no packets arrive from a server (it crashed or the network path is down), `latest_tick` for that server never advances, so the retain predicate is never evaluated. All incomplete reassembly entries for that server remain forever.

**Problem 2: No per-server cleanup when a different server advances.** The retain only filters entries where `sid == server_id.0`. Entries from other servers are untouched. If server 2 crashes, entries from server 2 are never cleaned up by packets from server 1 or 3.

**Problem 3: The reassembly entry contains a full Vec<EntityState>.** Each partial reassembly holds the entities received so far. With 1000+ entities and multiple chunks, a single incomplete reassembly can hold megabytes of deserialized entity data.

**Problem 4: No upper bound on reassembly map size.** There is no max-entries or max-age check. The HashMap grows proportional to `(num_peers * packet_loss_rate * ticks)`.

## Impact
- **Memory growth**: On embedded hardware (512MB RAM on UNO Q boards), each incomplete reassembly for a 1000-entity server holds ~100+ EntityState objects. At 20Hz with 1% packet loss, that's ~200 incomplete entries per 10 seconds per peer, each holding partial entity vectors. Over minutes this becomes significant.
- **After a server crash**: The crashed server stops sending packets. Its incomplete reassembly entries persist for the lifetime of the receiving process. With 3 servers and frequent crashes during testing, this compounds.
- **Latent DoS vector**: A malicious or buggy peer can send chunk 0 of N but never send the remaining chunks, filling the receiver's memory without any throttling.

## Recommended Fix

1. **Add a global tick-based cleanup sweep.** Every N ticks (e.g., 100 = 5 seconds), iterate the entire reassembly map and remove entries older than a threshold regardless of which server they belong to:
   ```rust
   let global_latest = latest_tick.values().max().copied().unwrap_or(0);
   if global_latest > 0 && global_latest % 100 == 0 {
       reassembly.retain(|&(_sid, t), _| global_latest - t <= 100);
   }
   ```

2. **Cap the reassembly map size.** Set a maximum (e.g., 50 entries). When exceeded, evict the oldest entry before inserting a new one.

3. **Cap per-entry age.** When inserting a new chunk, check the entry's age against the current wall clock or the global latest tick, not just the per-server latest tick.

4. **Log when entries are evicted.** This provides visibility into packet loss patterns on the mesh.

## Effort: small

## Related Files
- crates/sim-server/src/receiver.rs:18-25 (reassembly state definition)
- crates/sim-server/src/receiver.rs:108-149 (ChunkedEntityUpdate handling)
- crates/sim-server/src/receiver.rs:118-123 (cleanup heuristic)
