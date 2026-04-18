# REGISTRATION-RACE: Client registration and first snapshot can race across tick boundary

## Severity: medium

## Category: race-condition

## Summary
When a new client connects, the `handle_client` task pushes both a `SpawnPlayer` input and a `ClientRegistrationEvent` into the shared inbound queue. These are drained into the ECS on the next tick. However, the schedule runs `drain_client_events` (which registers the client in `ClientRegistry`) and `prepare_client_snapshot` (which sends frames) in the same tick. If the SpawnPlayer entity is not yet processed by the time the snapshot is prepared, the client's first full snapshot will not contain its own player entity.

More subtly, the `client_player_assignments` push in `drain_client_events` (systems.rs:220-225) happens during the same tick, but `process_client_inputs` (which actually spawns the player entity) runs after `drain_client_events`. So the player entity exists by the time `prepare_client_snapshot` runs. But the `handle.player_id` assignment happens in `drain_client_events` via the `client_player_assignments` path, which pushes the assignment into `InboundQueue.client_player_assignments`. This is consumed in the SAME run of `drain_client_events`, so `handle.player_id` is set before `process_client_inputs` runs.

**The actual race:** The `PlayerAssigned` JSON message is sent immediately by `handle_client` (network.rs:114-127), before the registration event is even drained. The client receives `PlayerAssigned` and starts expecting its player in binary frames. But the player entity does not exist until `process_client_inputs` runs (after drain). In the worst case (first tick after connect), the client receives `PlayerAssigned` followed by a binary snapshot that does not contain its player entity. The client's prediction manager tries to reconcile against a player that is not in the snapshot, producing no correction. This is a one-tick gap.

## Evidence
The timeline for a client connecting mid-tick:

1. `handle_client` runs (async): sends `PlayerAssigned` JSON immediately (network.rs:114-118)
2. `handle_client` pushes `SpawnPlayer` + `ClientRegistrationEvent` to shared inbound queue
3. Game loop tick starts:
   a. Phase 1: swap inbound data (game_loop.rs:89-116)
   b. `drain_inbound_events`: does NOT process client registrations (only peer/transfer/forward)
   c. `drain_client_events`: registers client, pushes player assignment
   d. `process_client_inputs`: spawns player entity from SpawnPlayer
   e. `prepare_client_snapshot`: sends first frame to client

The first frame (step 3e) includes the player entity because `process_client_inputs` (3d) runs before `prepare_client_snapshot` (3e). So the first binary frame does contain the player.

**However:** If the client connects during a tick's execution (between Phase 1 swap and the end of the tick), the registration event sits in the shared queue until the NEXT tick. The client receives `PlayerAssigned` immediately but gets no binary frames until the next tick processes the registration. This is a latency gap of up to 50ms, not a correctness issue.

**The real race is between ticks:** If a client connects right after Phase 1 of tick N, it will not be registered until tick N+1. But the `PlayerAssigned` message was already sent. If the client sends `MoveInput` before tick N+1, the input goes into `client_inputs` but the entity does not exist yet. The MoveInput handler logs "Unknown entity" and drops the input. This is a one-tick input loss.

## Impact
**Severity is medium, not high**, because the effects are transient:
- One tick (50ms) of missing binary frames after connect
- One tick of dropped MoveInput if the client sends input immediately
- Self-correcting on the next tick when registration is processed

This is barely noticeable in practice. But in a competitive scenario where a player needs to act immediately after spawning, the 50ms input loss could matter.

## Recommended Fix
The simplest fix is to defer sending `PlayerAssigned` until the player entity actually exists in the ECS. This could be done by having the game loop send the `PlayerAssigned` JSON through the per-client mpsc channel instead of `handle_client` sending it directly:

```rust
// In drain_client_events or process_client_inputs:
// After player entity is spawned, send PlayerAssigned through the client's channel
if let Some(handle) = registry.clients.get(&client_id) {
    let msg = serde_json::to_string(&ServerMessage::PlayerAssigned {
        player_id: entity_id.to_string()
    }).unwrap();
    // Send as text frame through the channel
    // (requires channel type to support both binary and text frames)
}
```

This is a medium effort change since the channel currently only carries binary frames. A simpler alternative: accept the one-tick gap as inherent and document it. The client already handles missing player entities gracefully (prediction manager skips reconciliation when player is not in snapshot).

## Effort: medium

## Related Files
- crates/sim-server/src/network.rs:96-111 (handle_client pushes registration + SpawnPlayer)
- crates/sim-server/src/network.rs:114-127 (PlayerAssigned sent immediately)
- crates/sim-server/src/systems.rs:210-253 (drain_client_events)
- crates/sim-server/src/schedule.rs:14-15 (drain_client_events before process_client_inputs)
