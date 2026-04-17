# Game Loop Calls std::process::exit During Drain

## Severity: medium

## Category: operational

## Summary
When the drain is complete and all entities have been transferred, the game loop calls `std::process::exit(0)` immediately. This kills the process without allowing the tokio runtime to shut down gracefully, which means in-flight UDP packets in the send buffer may not be flushed, WebSocket connections are not closed cleanly (no close frame), and any pending writes to the broadcast channel are lost.

## Evidence
`game_loop.rs` lines 224-227:
```rust
if world.resource::<ShutdownRequested>().0 {
    tracing::info!("Drain complete -- shutting down");
    std::process::exit(0);
}
```

This executes at the end of the tick, after Phase 4 (UDP send) and Phase 5 (client broadcast). However, the UDP sends in Phase 4 are async `send_to` calls that may be buffered by the OS. `std::process::exit(0)` does not call destructors and does not flush buffers.

Specifically, the authority transfer UDP packets sent during the drain (Phase 4, lines 184-188) are sent and then `process::exit` is called. If the OS hasn't flushed the UDP send buffer, the last batch of transfers may be lost -- the very transfers that ensure no entities are dropped during graceful shutdown.

## Impact
- **Lost transfers**: The final tick's authority transfer packets may be dropped if the OS hasn't flushed the UDP send buffer before `process::exit` terminates the process.
- **Unclean WebSocket close**: Clients connected to this server see an abrupt connection reset instead of a clean WebSocket close frame. This may cause reconnection delays.
- **Tracing logs lost**: Buffered tracing output may not be flushed.

## Recommended Fix
Replace `std::process::exit(0)` with a cooperative shutdown:

```rust
if world.resource::<ShutdownRequested>().0 {
    tracing::info!("Drain complete -- shutting down");
    // Give the OS time to flush UDP buffers
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Break out of the game loop -- let tokio runtime shut down gracefully
    break;
}
```

Then have `run_game_loop_ecs` return a `Result` or signal, and let `main.rs` handle the actual process termination after the tokio runtime shuts down. The `tokio::select!` in `main.rs` (line 133) will notice the game loop task completed and exit.

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:224-227 (process::exit call)
- crates/sim-server/src/main.rs:133-153 (tokio::select in main)
- crates/sim-server/src/systems.rs:608-612 (ShutdownRequested set)
