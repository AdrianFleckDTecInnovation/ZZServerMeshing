# Bincode Serialization Cost Scales Linearly Per-Peer Per-Tick

## Severity: medium

## Category: scalability

## Summary
Every tick, the game loop serializes the entire `EntityStateUpdate` into a bincode byte buffer, then sends that same buffer to every peer. The serialization happens once (which is correct), but the cost of serializing N entities every tick becomes significant at scale. Each `EntityState` contains a UUID (16 bytes), two Vec3s (24 bytes), an enum, a u16, and a u32 -- roughly 50-60 bytes per entity in bincode. With 1000 entities, the serialized payload is ~50-60KB, approaching the 60KB safety limit hardcoded in the game loop.

## Evidence
`game_loop.rs` lines 159-178:
```rust
let msg = PeerMessage::EntityStateUpdate {
    tick: repl_tick,
    server_id: server_id_u16,
    entities: entities.clone(),
};

if let Ok(bytes) = bincode::serialize(&msg) {
    if bytes.len() <= 60_000 {  // Hard safety limit
        for addr in &peer_addrs {
            if let Err(e) = udp_socket.send_to(&bytes, addr).await {
```

The 60,000-byte limit means replication silently stops when entity count exceeds ~1000 (60,000 / ~60 bytes per entity). The server logs a warning but continues ticking -- peers stop receiving updates, and their shadows go stale and get cleaned up. From the client perspective, entities from this server disappear.

Bincode serialization cost: `bincode::serialize` performs a single pass over the data, writing to a `Vec<u8>`. For 1000 entities, this is ~60KB of sequential writes plus the allocator cost of growing the Vec. Typical cost: 0.1-0.5ms on modern hardware, 1-3ms on UNO Q boards.

## Impact
- **500 entities**: ~30KB payload. Well within limit. Serialization: ~0.05-0.25ms.
- **1000 entities**: ~60KB payload. At the limit. Serialization: ~0.1-0.5ms.
- **1200 entities**: ~72KB payload. Exceeds limit. **Replication silently stops.** Shadows on all peers go stale in 5 seconds. All non-authoritative entities on this server disappear from the mesh.
- **The 65KB UDP packet limit**: Even without the 60KB safety limit, UDP packets over 65,507 bytes are rejected by the OS. IP fragmentation handles packets up to ~65KB on most systems but with increased loss probability.

The cost breakdown per tick with 1000 entities:
1. `prepare_replication` query: ~0.01ms (iteration only)
2. `to_entity_state` conversion: ~0.05ms (1000 struct copies)
3. `entities.clone()`: ~0.06ms (1000 * 60 bytes)
4. `bincode::serialize`: ~0.3ms
5. Per-peer `send_to`: ~0.005ms * N peers
Total: ~0.5ms + 0.005ms * N peers. About 1% of tick budget.

## Recommended Fix
1. **Immediate fix**: Replace the hard 60KB limit with UDP chunking:
```rust
const CHUNK_SIZE: usize = 50_000;
if bytes.len() > CHUNK_SIZE {
    // Split entities into chunks that fit in UDP
    for chunk in entities.chunks(entities.len() / ((bytes.len() / CHUNK_SIZE) + 1) + 1) {
        let chunk_msg = PeerMessage::EntityStateUpdate {
            tick: repl_tick,
            server_id: server_id_u16,
            entities: chunk.to_vec(),
        };
        // serialize and send chunk
    }
}
```

2. **Medium-term**: Pre-allocate the serialization buffer and reuse it across ticks:
```rust
let mut repl_buf = Vec::with_capacity(65536);
// Each tick:
repl_buf.clear();
bincode::serialize_into(&mut repl_buf, &msg)?;
```

3. **Long-term**: Switch to delta replication (only send entities that changed since the last ack from each peer). This is described in `docs/mesh-improvements.md` Phase C and would reduce the per-tick serialization cost to ~10% of current.

## Effort: medium

## Related Files
- crates/sim-server/src/game_loop.rs:159-178 (serialization and size check)
- crates/sim-server/src/systems.rs:620-647 (prepare_replication -- builds entity list)
- crates/shared/src/entity.rs:36-45 (EntityState definition -- determines serialized size)
