# Transfer Retry Reads Components After Mutation Phase Without Entity Existence Check

## Severity: low

## Category: race-condition

## Summary
In `process_transfer_timeouts` (systems.rs:337-378), the retry path collects entity handles and IDs from a query, then accesses components individually via `world.get::<Component>(entity).unwrap()`. Between the query and the component access, no other system runs (this is a single exclusive system). However, the `unwrap()` calls assume the entity still exists and has the expected components. If a future code change introduces entity removal between the query collection and the component read (within the same system), this will panic. The current code is safe because the query iterates, collects into a Vec, and then processes the Vec -- all within the same exclusive system call. But the pattern is fragile.

## Evidence

The retry path in systems.rs:337-398:
```rust
// Collect phase (line 320-334)
let mut query = world.query::<(Entity, &NetworkId, &TransferOutbound)>();
for (entity, nid, outbound) in query.iter(&world) {
    if ... {
        to_retry.push((entity, nid.0, outbound.target_addr, outbound.target_server));
    }
}

// Process phase (line 337-378) -- uses unwrap on component access
for (entity, eid, target_addr, target_server) in to_retry {
    let nid = world.get::<NetworkId>(entity).unwrap();  // panics if entity gone
    let pos = world.get::<Position>(entity).unwrap();
    let vel = world.get::<Velocity>(entity).unwrap();
    let kind = world.get::<EntityKind>(entity).unwrap();
    let seq = world.get::<LastInputSeq>(entity);
    ...
}
```

This is safe today because nothing removes entities between the collect and process phases. But it is the same pattern that finding #007 flagged (drain-despawns-during-iteration). The abort path (line 382-399) is safe because it only reads from `TransferOutbound` and then removes it.

## Impact
No current bug. The risk is purely about future maintainability -- if someone adds entity removal logic between the query and the component reads, the `unwrap()` calls will panic in production.

## Recommended Fix

Replace `unwrap()` with checked access and skip the retry if any component is missing:

```rust
let Some(nid) = world.get::<NetworkId>(entity) else { continue; };
let Some(pos) = world.get::<Position>(entity) else { continue; };
// ...
```

This is defensive and has zero cost.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:337-378 (process_transfer_timeouts retry path)
- crates/sim-server/src/systems.rs:320-334 (query collection)
