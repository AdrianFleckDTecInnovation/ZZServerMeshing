# Peer Health HashMap Cloned Twice Per Metrics Snapshot

## Severity: low

## Category: bottleneck

## Summary

Every tick, Phase 6 builds a new `HashMap<u16, PeerHealthSnapshot>` from the accumulator, stores it in `MetricsCollector.peer_health`, and then `MetricsCollector::snapshot()` clones it again into the `MetricsSnapshot`. When the metrics HTTP server calls `snapshot()`, the peer_health map is cloned a third time (via serde serialization). At 2-10 peers this is negligible, but the pattern allocates 2 HashMaps per tick unconditionally, even when no metrics consumer is polling.

## Evidence

Phase 6 at `game_loop.rs:543-562` builds a new HashMap every tick:

```rust
let ph = peer_health.stats.lock().unwrap_or_else(|e| e.into_inner());
let mut health_map = std::collections::HashMap::new();
for &pid in &active_peers {
    // ... build PeerHealthSnapshot ...
    health_map.insert(pid, crate::metrics::PeerHealthSnapshot { ... });
}
m.peer_health = health_map;
```

This allocates a new HashMap (heap allocation) and drops the old one (deallocation) every tick = 20 allocations/sec.

Then `MetricsCollector::snapshot()` at `metrics.rs:166` clones it:

```rust
peer_health: self.peer_health.clone(),
```

This is called by the HTTP handler (metrics_server.rs:242), so the clone only happens on poll. But the Phase 6 allocation happens unconditionally every tick.

## Impact

At 3 peers: each HashMap is ~128 bytes (3 entries * ~40 bytes + overhead). 20 allocations/second = ~2.5 KB/s of heap churn. On the UNO Q boards (512 MB RAM, single-core ARM), this is negligible.

At 20 peers: ~800 bytes per map, 16 KB/s heap churn. Still negligible.

The allocation is dwarfed by the Vec allocations in replication (finding #006, #061). This finding is documented for completeness but is not actionable at current scale.

## Recommended Fix

If the allocation matters in profiling (it likely will not), reuse the HashMap by clearing and re-populating instead of replacing:

```rust
// In Phase 6:
m.peer_health.clear();
for &pid in &active_peers {
    // ...
    m.peer_health.insert(pid, ...);
}
```

This avoids deallocation when the peer count stays the same (HashMap retains capacity after clear). Not worth the code churn unless profiling shows this matters.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:543-562 (Phase 6 HashMap allocation)
- crates/sim-server/src/metrics.rs:166 (snapshot clone)
- crates/sim-server/src/metrics_server.rs:242 (HTTP handler calls snapshot)
