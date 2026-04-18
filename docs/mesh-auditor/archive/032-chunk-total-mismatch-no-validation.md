# Chunk Reassembly Accepts Mismatched total_chunks Without Validation

## Severity: medium

## Category: protocol

## Summary
The chunk reassembly logic in the UDP receiver trusts the `total_chunks` field from the first chunk it receives for a given `(server_id, tick)` key, and never validates that subsequent chunks carry the same `total_chunks` value. If the sender calculates a different `total_chunks` due to a race or bug, or if a malicious peer sends conflicting values, the reassembly either completes prematurely (missing entities) or never completes (expecting more chunks than exist).

## Evidence

In receiver.rs:125-128, the entry is created with the `total_chunks` from whichever chunk arrives first:
```rust
let entry = reassembly.entry(key).or_insert_with(|| ChunkReassembly {
    total_chunks,
    received: HashSet::new(),
    entities: Vec::new(),
});
```

If chunk 0 arrives with `total_chunks: 5` and chunk 1 arrives with `total_chunks: 4` (due to a retransmission from a slightly different entity count), the entry keeps `total_chunks: 5` from chunk 0. The completion check at line 138:
```rust
if entry.received.len() == entry.total_chunks as usize {
```
waits for 5 chunks. If only 4 were actually sent (the sender computed `total_chunks: 4`), chunk index 4 never arrives and the reassembly is never completed. The entities from chunks 0-3 sit in the reassembly map until the 2-tick cleanup evicts them, and that tick's replication is lost entirely.

Additionally, there is no validation that `chunk_index < total_chunks`. A chunk with `chunk_index: 10` and `total_chunks: 5` is accepted silently, inflating the received set and potentially triggering premature completion.

## Impact
- **Silent data loss**: If `total_chunks` is inconsistent between chunks of the same tick, the reassembly either never completes (data lost) or completes with a subset (partial data). No error is logged.
- **Protocol corruption vector**: An untrusted peer (or a bug in the sender's chunking logic) can cause the receiver to lose replication data for arbitrary ticks.
- **Likelihood**: Low under normal operation since the sender serializes all chunks atomically in one tick. But if entity counts change between ticks (entity spawned between chunking and sending, which does not happen in current code but could with future changes), or if UDP packets from different ticks are reordered, the values could mismatch.

## Recommended Fix

1. **Validate chunk_index < total_chunks** on receipt. If violated, log a warning and drop the chunk.
2. **Validate total_chunks consistency.** If a subsequent chunk has a different `total_chunks` than the entry, log a warning and either: (a) reset the entry, or (b) drop the inconsistent chunk.
3. **Log on reassembly timeout/eviction.** When the cleanup evicts an incomplete reassembly, log the server_id, tick, and received/expected chunk counts so the operator can diagnose packet loss.

```rust
if chunk_index >= total_chunks {
    tracing::warn!(server = server_id.0, tick, chunk_index, total_chunks,
        "Invalid chunk_index >= total_chunks, dropping");
    continue;
}
let entry = reassembly.entry(key).or_insert_with(|| ChunkReassembly {
    total_chunks,
    received: HashSet::new(),
    entities: Vec::new(),
});
if entry.total_chunks != total_chunks {
    tracing::warn!(server = server_id.0, tick,
        expected = entry.total_chunks, got = total_chunks,
        "total_chunks mismatch in chunk reassembly, dropping chunk");
    continue;
}
```

## Effort: small

## Related Files
- crates/sim-server/src/receiver.rs:108-149 (ChunkedEntityUpdate handling)
- crates/sim-server/src/receiver.rs:125-128 (entry creation with unvalidated total_chunks)
- crates/sim-server/src/receiver.rs:131-138 (completion check)
