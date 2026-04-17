# Cold Tick Burst Amplifies UDP Chunking Load

## Severity: medium

## Category: bottleneck

## Summary

Every 4th tick (cold tick), the system sends ALL authoritative entities to each peer instead of just the hot ones. This creates a periodic bandwidth spike where the number of UDP datagrams jumps 4-10x compared to hot-only ticks. With chunking (ENTITIES_PER_CHUNK = 28), a cold tick with 1000 entities and 3 peers produces 108 chunks (36 per peer), all enqueued into the 256-slot replication channel in one tick. This periodic spike is the most likely trigger for the backpressure system, which then has the catastrophic interaction described in finding #058.

## Evidence

The entities-per-chunk limit is 28 (`game_loop.rs:325`). The replication channel capacity is 256 slots (`game_loop.rs:35`).

Calculation for cold tick with 1000 entities and 3 peers:
- Chunks per peer: ceil(1000 / 28) = 36
- Total messages: 36 * 3 = 108 UdpOutbound::Send messages
- Channel utilization: 108 / 256 = 42%

With 2000 entities and 3 peers:
- Chunks per peer: 72
- Total messages: 216
- Channel utilization: 216 / 256 = 84%

With 2500 entities and 3 peers:
- Chunks per peer: 90
- Total messages: 270
- Channel utilization: 270 / 256 = 105% -- OVERFLOW, triggers backpressure

On hot-only ticks with the same 2500 entities but only 200 hot per peer:
- Chunks per peer: 8
- Total messages: 24
- Channel utilization: 24 / 256 = 9%

The 11x spike between hot-only (9%) and cold (105%) ticks means the system works fine 75% of the time and overflows 25% of the time. The backpressure system at `game_loop.rs:517-536` then blocks ALL replication (including the 9% hot ticks that were fine).

Additionally, the cold tick spike on the UNO Q boards also competes with the tick budget. Serializing 2500 entities via bincode takes ~2-3ms, and `try_send` for 270 messages adds another ~0.5ms of overhead. The 50ms tick budget is strained.

## Impact

At entity counts above ~2500 (with 3 peers), every cold tick overflows the replication channel. This triggers backpressure, which (per finding #058) blocks all replication including future cold ticks, potentially leading to shadow loss.

Even below the overflow threshold, the cold-tick spike causes higher latency for control messages (authority transfers, forwarded inputs) because the UDP sender task must drain 100+ replication messages before processing the next control message. The biased select prioritizes control, but if the sender is mid-send when the cold tick dumps 108 messages, the next control message waits for the current send to complete.

## Recommended Fix

Spread cold-tier entities across multiple ticks instead of sending all at once:

```rust
// Instead of: tick % 4 == 0 sends all cold entities
// Use: each entity has a cold phase based on its hash
let entity_cold_phase = entity_id_hash % 4;
let current_phase = tick % 4;
let is_entities_cold_tick = entity_cold_phase == current_phase;
```

This distributes the cold tier evenly: each tick sends 25% of cold entities (the ones whose phase matches), plus all hot entities. The burst goes from 100% of entities every 4th tick to 25% of entities every tick.

Total bandwidth is identical (every cold entity still sent at 5Hz), but the peak is reduced by 4x. The channel utilization becomes: (hot_count + cold_count/4) chunks per tick instead of (hot_count + cold_count) chunks every 4th tick.

Implementation: use the EntityId's UUID to deterministically assign a phase. Since UUIDs are random, this naturally distributes evenly across phases.

## Effort: medium

## Related Files
- crates/sim-server/src/systems.rs:1157 (is_cold_tick = tick % 4 == 0)
- crates/sim-server/src/systems.rs:1238-1240 (cold entity inclusion)
- crates/sim-server/src/game_loop.rs:325 (ENTITIES_PER_CHUNK = 28)
- crates/sim-server/src/game_loop.rs:35 (UDP_REPLICATION_CHANNEL_SIZE = 256)
- crates/sim-server/src/game_loop.rs:414 (backpressure gate)
