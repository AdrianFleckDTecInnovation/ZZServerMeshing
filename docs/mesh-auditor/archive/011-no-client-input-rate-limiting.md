# No Rate Limiting on Client Input

## Severity: medium

## Category: operational

## Summary
Client WebSocket messages are pushed directly into the shared inbound queue without any rate limiting, validation, or backpressure. A malicious or buggy client can send thousands of `MoveInput` or `SpawnEntity` messages per second, all of which are queued and processed by the game loop, consuming tick budget and potentially spawning unlimited entities.

## Evidence
`network.rs` lines 97-101:
```rust
match serde_json::from_str::<ClientMessage>(&text) {
    Ok(input) => {
        let mut q = inbound.queue.lock().await;
        q.client_inputs.push(input);  // No rate check, no validation
    }
```

Every valid JSON message is blindly pushed to the queue. The game loop processes ALL queued client inputs every tick (`process_client_inputs` at systems.rs line 179 takes all inputs with `std::mem::take`).

Specific attack vectors:
1. **Spawn flood**: Client sends `SpawnEntity` at 1000/sec. Each spawns a new entity in the ECS world. 10 seconds = 10,000 entities, overwhelming the server.
2. **Input flood**: Client sends `MoveInput` at 1000/sec. Each is processed (velocity set, potentially forwarded to peers). 50 inputs per tick = 50 bincode serializations + UDP sends.
3. **RemoveEntity abuse**: Client sends `RemoveEntity` for any entity ID (not just its own). There is no ownership check -- any client can delete any entity.

## Impact
- **Entity spam**: A single client can create thousands of entities, degrading performance for all clients and peers.
- **Tick budget exhaustion**: Processing 1000+ client inputs per tick consumes the entire 50ms budget.
- **Entity deletion**: Any client can delete any entity by sending `RemoveEntity` with a guessed or observed UUID.
- **Forward amplification**: MoveInput for shadow entities generates forwarded UDP packets. A client can amplify its traffic by targeting shadow entities, causing outbound UDP floods.

## Recommended Fix
1. **Per-client rate limiter** in the WebSocket handler:
```rust
let mut msg_count_this_second = 0u32;
let mut last_reset = Instant::now();

// Inside the message loop:
if last_reset.elapsed() > Duration::from_secs(1) {
    msg_count_this_second = 0;
    last_reset = Instant::now();
}
msg_count_this_second += 1;
if msg_count_this_second > 30 {  // 30 msgs/sec max
    continue;  // drop excess
}
```

2. **Ownership check for RemoveEntity**: Only allow a client to remove its own player entity:
```rust
ClientMessage::RemoveEntity { entity_id } => {
    if entity_id != player_uuid.to_string() {
        tracing::warn!("Client tried to remove entity {} (not their player)", entity_id);
        continue;
    }
}
```

3. **SpawnEntity gating**: Either remove client-side spawning entirely (spawn only through the coordinator), or rate-limit to 1 spawn per second per client.

4. **Queue depth limit**: If the inbound queue exceeds a threshold (e.g., 1000 entries), drop new inputs and log a warning.

## Effort: small

## Related Files
- crates/sim-server/src/network.rs:94-113 (client message handling -- no rate limit)
- crates/sim-server/src/systems.rs:177-304 (process_client_inputs -- processes all)
- crates/sim-server/src/systems.rs:290-299 (RemoveEntity -- no ownership check)
