# PER-CLIENT-AOI-CLONE: AoI filter clones EntityState for every in-range entity per client per tick

## Severity: low

## Category: bottleneck

## Summary
The AoI filter at line 1234-1238 uses `.filter().cloned().collect()` to produce a per-client `Vec<EntityState>`. Each EntityState is ~48 bytes (UUID + 2 Vec3 + enum + u16 + u32). For C clients each seeing E_aoi entities, this allocates C * E_aoi * 48 bytes per tick. The filter itself (5 float ops per entity) is cheap, but the clone + Vec allocation is the dominant cost. This is the expected design tradeoff documented in the design doc, but it is worth quantifying for the scalability record.

## Evidence
`crates/sim-server/src/systems.rs:1234-1238`:
```rust
entities
    .iter()
    .filter(|e| e.id == pid || in_aoi(player_pos, e.position))
    .cloned()   // <-- 48-byte memcpy per entity
    .collect()  // <-- Vec allocation
```

EntityState fields (from `crates/shared/src/entity.rs:36-45`):
```rust
pub struct EntityState {
    pub id: EntityId,           // Uuid = 16 bytes
    pub position: Vec3,         // 12 bytes
    pub velocity: Vec3,         // 12 bytes
    pub entity_type: EntityType,// 1 byte (enum)
    pub authority_server: ServerId, // 2 bytes
    pub last_input_seq: u32,    // 4 bytes
}                               // Total: ~48 bytes (with alignment)
```

Per-tick cost calculation:
- Filter pass: E entities * 5 float ops = negligible
- Clone: E_aoi * 48 bytes memcpy per client
- Vec allocation: 1 heap allocation of E_aoi * 48 bytes per client
- Total: C * (E_aoi * 48) bytes of allocation + memcpy

At E_aoi=200, C=3: 3 * 200 * 48 = 28,800 bytes = 28.8 KB per tick.
At E_aoi=500, C=10: 10 * 500 * 48 = 240,000 bytes = 240 KB per tick.
At 20 Hz: 4.8 MB/sec for the latter case.

## Impact
On the UNO Q boards with 3 clients and 200 entities in AoI, 28.8 KB per tick is negligible (well within the 50ms budget). Even at 10 clients with 500 entities in AoI, 240 KB per tick is manageable on modern hardware but begins to matter on the boards.

The clone is necessary because `compute_delta` and `encode_binary_snapshot` consume `&[EntityState]`, and the entities Vec is borrowed immutably by the outer loop while the per-client Vec is being built. Passing references instead of clones would require lifetime gymnastics or a different data flow.

This is a known, accepted cost documented in the design doc's "Per-Tick Cost Analysis" section. The clone cost is proportional to E_aoi (not E), which is the whole point of AoI filtering.

## Recommended Fix
No immediate fix needed. For future optimization at high client counts:

1. **Indices instead of clones**: Build a `Vec<usize>` of indices into the `entities` Vec instead of cloning EntityState. Pass the indices + entities slice to `compute_delta` and `encode_binary_snapshot`. This replaces 48-byte clones with 8-byte index copies.

2. **Bitmap filtering**: Use a `BitVec` per client marking which entities are in AoI. Pass the bitmap to delta/encoding functions. Zero allocation beyond the bitmap itself.

3. **Shared entities + skip encoding**: If most clients have overlapping AoIs (e.g., all near origin), encode once and share. This is the pre-AoI optimization that was lost.

These optimizations are only warranted when profiling shows the clone is a significant fraction of the 50ms tick budget.

## Effort: medium (for index-based approach)

## Related Files
- crates/sim-server/src/systems.rs:1234-1238 (filter + clone + collect)
- crates/shared/src/entity.rs:36-45 (EntityState size)
- crates/sim-server/src/delta.rs:86-115 (compute_delta consumes &[EntityState])
- crates/sim-server/src/encoding.rs:34-47 (encode_binary_snapshot consumes &[EntityState])
