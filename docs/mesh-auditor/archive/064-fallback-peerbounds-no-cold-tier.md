# PeerBounds Fallback Path Has No Cold Tier Distinction

## Severity: medium

## Category: scalability

## Summary

When a peer has shadow entities but no Player shadows (only NPC shadows), the replication falls back to the PeerBounds approach from design 007. This fallback path sends all entities within the bounding box on EVERY tick at 20Hz, with no hot/cold distinction. This defeats the bandwidth reduction that the tiered system provides, and for peers with many NPC shadows the bounding box can be very large.

## Evidence

In `systems.rs:1248-1263`, the fallback path:
```rust
_ => {
    // Fallback: no Player shadows for this peer.
    // Use PeerBounds from ALL shadows (current behavior from design 007).
    if let Some(bounds) = peer_bounds.get(&peer_id) {
        let filtered: Vec<EntityState> = entities
            .iter()
            .filter(|e| bounds.contains(e.position))
            .cloned()
            .collect();
        // Count all as hot since we cannot distinguish without player positions
        hot_this_peer = filtered.len();
        filtered
    } else {
        continue; // no bounds at all, skip
    }
}
```

This path runs on every tick (both hot-only and cold ticks). There is no `is_cold_tick` gate. Every entity within the PeerBounds is sent at 20Hz.

The comment "Count all as hot since we cannot distinguish without player positions" is accurate -- without player positions, we cannot do tiered classification. But the result is that this peer gets 4x the bandwidth of a peer with player shadows.

### When this triggers

1. Peer just joined and player shadows have not arrived yet (transient).
2. Peer's player shadows were cleaned up by `shadow_cleanup` but NPC shadows remain (SHADOW_STALE_TICKS is the same for both).
3. Peer has no player entities at all (e.g., a simulation-only server with no clients connected).

Case 3 is a permanent condition. A server with no clients has no player entities, so it will never have Player shadows on other servers. All replication to such peers is permanently at 20Hz with PeerBounds filtering.

## Impact

For peers in the fallback path, bandwidth usage is 4x higher than it would be with proper tiered classification. With a large PeerBounds box (many spread-out NPC shadows), this could approach unfiltered 20Hz replication for all entities.

On the UNO Q boards with 3 servers, if one server has no clients, the other two servers each send all entities to it at 20Hz instead of 5Hz, consuming 4x the bandwidth to that peer.

## Recommended Fix

Apply cold-tick filtering to the fallback path as well. On non-cold ticks, only send entities within a tighter radius (e.g., the center of the PeerBounds), or skip the fallback entirely on hot-only ticks and rely on cold ticks for the PeerBounds path:

```rust
_ => {
    // Fallback: no Player shadows for this peer.
    // Only send on cold ticks to limit bandwidth (5Hz instead of 20Hz).
    // The peer has no players, so no client needs smooth 20Hz updates.
    if !is_cold_tick {
        continue; // Skip non-cold ticks for peers without players
    }
    if let Some(bounds) = peer_bounds.get(&peer_id) {
        let filtered: Vec<EntityState> = entities
            .iter()
            .filter(|e| bounds.contains(e.position))
            .cloned()
            .collect();
        cold_this_peer = filtered.len();
        filtered
    } else {
        continue;
    }
}
```

This is safe because: if the peer has no Player entities, no client on that peer needs smooth entity movement. The 5Hz cold rate is sufficient for maintaining shadow freshness for crash recovery and rebalance awareness.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1248-1263 (fallback path)
- crates/sim-server/src/systems.rs:1178-1186 (PeerBounds construction)
- crates/shared/src/spatial.rs:58-106 (PeerBounds struct)
