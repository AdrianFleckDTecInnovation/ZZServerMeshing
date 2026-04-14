# Chunked Entity Update Does Not Validate Against UDP Packet Size Limit

## Severity: medium

## Category: scalability

## Summary
The chunking implementation uses a fixed `ENTITIES_PER_CHUNK = 100` to split entity replication into multiple UDP packets. However, the actual serialized size of 100 entities is not validated against the UDP datagram size limit (65,507 bytes for IPv4, minus headers). The bincode-serialized size of an EntityState varies with UUID encoding and Vec3 representation, and the per-chunk overhead (tick, server_id, chunk_index, total_chunks) adds to the payload. With 100 entities, the serialized size is approximately 12-14KB per chunk, which is safe. But the constant is not derived from the size limit -- it is a guess that happens to work.

## Evidence

The chunking logic in game_loop.rs:160-214:
```rust
const ENTITIES_PER_CHUNK: usize = 100;

if entities.len() <= ENTITIES_PER_CHUNK {
    // Small payload: use unchunked format
    let msg = PeerMessage::EntityStateUpdate { ... };
    if let Ok(bytes) = bincode::serialize(&msg) {
        for addr in &peer_addrs {
            if let Err(e) = udp_socket.send_to(&bytes, addr).await { ... }
        }
    }
} else {
    // Large payload: chunk it
    let total_chunks = ((entities.len() + ENTITIES_PER_CHUNK - 1) / ENTITIES_PER_CHUNK) as u16;
    for (i, chunk) in entities.chunks(ENTITIES_PER_CHUNK).enumerate() {
        let msg = PeerMessage::ChunkedEntityUpdate { ... };
        if let Ok(bytes) = bincode::serialize(&msg) { ... }
    }
}
```

**No size check on the serialized output.** The `bincode::serialize` result is sent directly to `udp_socket.send_to` without verifying `bytes.len() < 65507`. If the EntityState struct grows (new fields added in the future), or if the bincode encoding changes (e.g., variable-length strings added to entities), the serialized size could exceed the UDP limit, causing a silent send failure.

**The unchunked path has no protection.** When entity count is <= 100, the code uses the unchunked `EntityStateUpdate` format. With 100 entities at ~120 bytes each in bincode, that is ~12KB -- safe. But if EntityState grows to 600+ bytes (plausible with added fields like health, inventory references, names), 100 entities would be ~60KB, approaching the limit. The unchunked path has no fallback.

## Impact
- **Current**: Safe. 100 entities serialize to approximately 12-14KB, well under the 65KB limit.
- **Future risk**: If EntityState gains new fields (health, name, equipment, etc.), the per-entity size grows. At ~650 bytes/entity, 100 entities would hit the UDP limit. The chunking constant would need to decrease, but there is no automated detection of this.
- **Silent failure mode**: `udp_socket.send_to` returns an error for oversized packets, which is logged as a warning but otherwise ignored. The entire chunk is lost, causing the receiver to never complete reassembly for that tick.

## Recommended Fix

1. **Add a post-serialization size check.** After `bincode::serialize`, verify the byte length is under a safe threshold (e.g., 60,000 bytes to leave headroom for headers):
   ```rust
   if let Ok(bytes) = bincode::serialize(&msg) {
       if bytes.len() > 60_000 {
           tracing::error!("Chunk {} serialized to {} bytes, exceeding UDP safe limit",
               i, bytes.len());
           // Could split further or skip
       }
       // send...
   }
   ```

2. **Derive ENTITIES_PER_CHUNK from entity size.** Instead of a fixed constant, compute the chunk size dynamically based on a sample entity's serialized size. Or add a static assertion in tests that verifies 100 entities serialize under the limit.

3. **Add a test.** Create 100 entities, serialize them as a single `EntityStateUpdate`, and assert the byte length is under 60,000.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:160-214 (chunked replication logic)
- crates/sim-server/src/game_loop.rs:160 (ENTITIES_PER_CHUNK constant)
- crates/shared/src/entity.rs (EntityState definition)
