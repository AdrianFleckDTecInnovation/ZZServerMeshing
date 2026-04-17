# Behavioral Divergence: Rebalance Directive entity_ids Field Ignored

## Severity: low

## Category: protocol

## Summary
The coordinator sends `RebalanceDirective { entity_ids: Vec<String>, target_server: u16 }`, specifying which entities to transfer. However, the `process_transfers` system completely ignores the `entity_ids` field and instead selects its own entities to transfer (NPCs sorted by distance from origin, up to 10% of total authoritative entities). This is a behavioral divergence from the coordinator's intent and means the coordinator cannot control WHICH entities are transferred, only WHERE they go.

## Evidence
`systems.rs` lines 465-468:
```rust
pub fn process_transfers(world: &mut World) {
    let transfer = world.resource_mut::<PendingTransfers>().transfers.take();
    let Some((_entity_ids, target_server)) = transfer else {
    //       ^ UNUSED -- entity_ids is destructured but never read
        return;
    };
```

The system then builds its own candidate list at lines 483-498:
```rust
let mut npc_candidates: Vec<(Entity, EntityId, f32)> = Vec::new();
// ...
for (entity, nid, pos, kind) in query.iter(&world) {
    total_auth += 1;
    if kind.0 == EntityType::Npc {
        npc_candidates.push((entity, nid.0, pos.0.length_squared()));
    }
}
npc_candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
let transfer_count = (total_auth / 10).max(1).min(npc_candidates.len());
```

The coordinator's `entity_ids` field (from `registry.rs` rebalance logic) is a `Vec<String>` containing specific entity UUIDs that the coordinator selected for transfer. The server ignores them.

## Impact
- **Functional correctness**: The mesh still works because entities are transferred and load is balanced. But the coordinator's entity selection algorithm (which may consider spatial clustering, entity types, or other heuristics) is completely bypassed.
- **Coordinator-server contract**: The protocol implies the coordinator chooses entities and the server executes. The actual behavior is: coordinator signals "rebalance," server does its own thing. This inconsistency will cause confusion when implementing smarter rebalancing (Phase F from mesh-improvements.md).
- **Player safety**: The server's algorithm correctly excludes players (only transfers NPCs). But if the coordinator ever needed to transfer a specific player entity, it couldn't.

## Recommended Fix
Either:
1. **Use the entity_ids**: Look up the specified entities by UUID and transfer those specifically, falling back to the current heuristic when entity_ids is empty.
2. **Simplify the protocol**: Change the directive to just `{ target_server, max_count }` and document that the server chooses which entities to send.

Option 1 is preferred for future extensibility:
```rust
let Some((entity_ids, target_server)) = transfer else { return };

if entity_ids.is_empty() {
    // Current behavior: server picks NPCs to transfer
} else {
    // Transfer specific entities requested by coordinator
    for id_str in &entity_ids {
        if let Ok(uuid) = uuid::Uuid::parse_str(id_str) {
            let eid = EntityId(uuid);
            if let Some(&entity) = world.resource::<EntityLookup>().0.get(&eid) {
                // transfer this specific entity
            }
        }
    }
}
```

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:464-537 (process_transfers -- ignores entity_ids)
- crates/shared/src/protocol.rs:111-114 (RebalanceDirective definition with entity_ids)
- crates/coordinator/src/registry.rs (coordinator selects entity_ids -- not used by server)
