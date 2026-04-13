# Client Snapshot System Iterates Queries Three Times

## Severity: medium

## Category: bottleneck

## Summary
The `prepare_client_snapshot` system iterates the `auth_query` twice and the `shadow_query` once for sizing, then iterates both again for data collection. With N authoritative entities and M shadow entities, this performs 2N + 2M iterations per tick instead of N + M. Additionally, it allocates a `HashSet` to deduplicate entities, which is unnecessary because `Authoritative` and `Shadow` are mutually exclusive components (an entity cannot have both).

## Evidence
`systems.rs` lines 680-696:
```rust
// First pass: count both queries to pre-allocate
let mut entities = Vec::with_capacity(auth_query.iter().count() + shadow_query.iter().count());
//                                     ^^^^^^^^^ ITERATION 1       ^^^^^^^^^ ITERATION 2

// Second pass: collect authoritative
let mut seen_ids = std::collections::HashSet::new();  // UNNECESSARY ALLOCATION
for (nid, pos, vel, kind, auth, seq) in auth_query.iter() {  // ITERATION 3
    seen_ids.insert(nid.0);
    entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
}

// Third pass: collect shadows
for (nid, pos, vel, kind, auth, seq) in shadow_query.iter() {  // ITERATION 4
    if !seen_ids.contains(&nid.0) {  // ALWAYS TRUE -- auth/shadow are mutually exclusive
        entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
    }
}
```

The `seen_ids` check is dead logic: an entity with `Authoritative` component can never match the `With<Shadow>` filter, and vice versa. The `Authoritative` and `Shadow` components are mutually exclusive by construction (see `spawn_from_entity_state_direct` at conversion.rs lines 22-28, which inserts one or the other, never both).

## Impact
At 1000 entities (500 auth + 500 shadow), the extra iterations add measurable overhead:
- Two unnecessary query iterations (~1000 entity traversals)
- HashSet allocation and insert for 500 UUIDs (~500 * 24 bytes = 12KB heap allocation per tick)
- HashSet lookup for 500 shadows (O(1) each but unnecessary)

At 20Hz, this is 20,000 unnecessary iterations per second plus 240KB of heap churn per second.

## Recommended Fix
Remove the counting pass and the HashSet. Use a simple Vec that collects from both queries:

```rust
pub fn prepare_client_snapshot(
    tick: Res<TickCounter>,
    mut snapshot: ResMut<ClientSnapshot>,
    auth_query: Query<(...), With<Authoritative>>,
    shadow_query: Query<(...), With<Shadow>>,
) {
    let mut entities = Vec::new();

    for (nid, pos, vel, kind, auth, seq) in auth_query.iter() {
        entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
    }
    for (nid, pos, vel, kind, auth, seq) in shadow_query.iter() {
        entities.push(to_entity_state(nid, pos, vel, kind, auth, seq));
    }

    snapshot.frame = Some(encode_binary_snapshot(tick.0, &entities));
}
```

If pre-allocation is desired, track entity counts in a resource updated by a prior system (or use the `EntityLookup.len()` as a rough estimate).

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:654-697 (prepare_client_snapshot)
- crates/sim-server/src/conversion.rs:22-28 (mutual exclusivity of Authoritative/Shadow)
