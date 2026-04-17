# ENTITIES-CLONE-NO-AOI: Full entity Vec cloned every tick for clients without AoI position

## Severity: medium

## Category: bottleneck

## Summary
When a client has no player entity, or the player entity's position cannot be found in the entity list, the entire `entities` Vec is cloned with `entities.clone()`. The design doc claims this happens "exactly once per client connection," but there are common scenarios where it happens every tick: the client registration sets `player_id` immediately using the UUID from the connection event, but the actual player ECS entity may not exist yet (SpawnPlayer is processed by `process_client_inputs` on the same tick or a later tick). More critically, if the player's authority is transferred to another server and the shadow is stale or removed, the position lookup fails and the unfiltered clone fires on every tick until the shadow reappears.

## Evidence
`crates/sim-server/src/systems.rs:1232-1246`:
```rust
let client_entities: Vec<EntityState> = if let Some(pid) = handle.player_id {
    if let Some(&player_pos) = player_positions.get(&pid) {
        entities.iter().filter(|e| e.id == pid || in_aoi(player_pos, e.position))
            .cloned().collect()
    } else {
        // Player position not found (entity not spawned yet) -- send everything
        entities.clone()  // <-- FULL CLONE
    }
} else {
    // No player assigned yet -- send everything
    entities.clone()  // <-- FULL CLONE
};
```

The `player_positions` HashMap is built at lines 1187-1208 by scanning the `entities` Vec for the player ID. The player only appears in `entities` if it exists as an ECS entity (authoritative, shadow, or transfer-outbound). If the player entity was transferred away and the shadow was cleaned up (after SHADOW_STALE_TICKS = 300 ticks = 15 seconds), it will not be in `entities`.

The `player_id` is set during `drain_client_events` (systems.rs:428) on the same tick the client registers. But the SpawnPlayer message that creates the ECS entity is processed by `process_client_inputs` (systems.rs:491), which runs AFTER `drain_client_events` in the schedule (schedule.rs:17). On the first tick, `player_id` is set but the entity does not exist yet, so `player_positions` has no entry for it.

Schedule ordering (schedule.rs:12-29):
```
drain_client_events -> process_client_inputs -> ... -> prepare_client_snapshot
```

On the client's first tick:
1. `drain_client_events` sets `handle.player_id = Some(entity_id)`
2. `drain_client_events` pushes the SpawnPlayer into `client_inputs` (via the registration event)
3. `process_client_inputs` spawns the ECS entity
4. `prepare_client_snapshot` runs -- player entity now exists in the ECS

Wait -- let me re-trace. The SpawnPlayer comes from the client via WebSocket, not from the registration. The registration only sets `player_id`. The actual SpawnPlayer message arrives as a ClientMessage and is processed by `process_client_inputs`. So the timeline is:

- Tick N: client connects, registration event processed by `drain_client_events`, `player_id` set
- Tick N: if SpawnPlayer message also arrived and was drained into `client_inputs`, the entity is spawned by `process_client_inputs` on the same tick
- Tick N: `prepare_client_snapshot` runs -- if the SpawnPlayer was on the same tick, the entity exists

The race depends on whether the SpawnPlayer WebSocket message arrives in the same inbound drain as the registration event. If it arrives one tick later, there is a one-tick window.

However, the more concerning scenario is authority transfer: if the player's authority moves to another server, this server's only record of the player is a shadow. If the shadow becomes stale and is cleaned up, `player_positions` will have no entry, and the full clone happens every tick for that client until the player entity reappears.

## Impact
Clone cost per occurrence: E * sizeof(EntityState). EntityState is ~48 bytes (UUID + 2 Vec3 + enum + u16 + u32). At E=2000: 96 KB cloned per tick per affected client. At E=5000: 240 KB per tick.

The "no player assigned" path (line 1245) should only fire on the very first tick before SpawnPlayer arrives. This is low impact -- one tick.

The "player position not found" path (line 1241) fires whenever the player entity is not in the world snapshot. This can persist for multiple ticks during authority transfer or if the shadow is stale. At 20Hz with E=2000 and 5 clients in this state: 5 * 96 KB * 20 = 9.6 MB/sec of clone work.

Additionally, sending ALL entities to a client that has lost its player position is a bandwidth waste: the client receives entities it does not need, and the next tick when the player reappears, those entities will be removed via delta, causing a visual flash.

## Recommended Fix
When the player position is unknown, use the last known AoI center instead of falling back to unfiltered. Store the last known player position on `ClientHandle`:

In `crates/sim-server/src/resources.rs`, add to `ClientHandle`:
```rust
pub aoi_center: Option<Vec3>,
```

In `crates/sim-server/src/systems.rs`, update the AoI filter:
```rust
let client_entities: Vec<EntityState> = if let Some(pid) = handle.player_id {
    let center = player_positions.get(&pid).copied()
        .or(handle.aoi_center);  // fallback to last known position
    if let Some(player_pos) = center {
        handle.aoi_center = Some(player_pos);  // update cache
        entities.iter()
            .filter(|e| e.id == pid || in_aoi(player_pos, e.position))
            .cloned().collect()
    } else {
        entities.clone()  // truly first tick, no position ever known
    }
} else {
    entities.clone()  // no player at all
};
```

This limits the unfiltered clone to the very first tick of a client's lifetime, and maintains AoI filtering even during authority transfer gaps.

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:1232-1246 (AoI filter with fallback clones)
- crates/sim-server/src/systems.rs:1187-1208 (player_positions HashMap build)
- crates/sim-server/src/resources.rs:204-218 (ClientHandle struct)
- crates/sim-server/src/schedule.rs:16-17 (drain_client_events -> process_client_inputs ordering)
