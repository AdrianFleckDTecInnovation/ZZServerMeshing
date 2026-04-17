# Sender Task Drain Cancelled on Game Loop Exit

## Severity: low

## Category: data-loss

## Summary
When the game loop exits via `break` (the #009 fix), the `tokio::select!` in `main.rs` immediately returns. The tokio runtime then shuts down, cancelling all spawned tasks -- including the UDP sender task, which may be mid-drain. Authority transfer packets or forwarded inputs queued in the control channel during the final tick may never be sent.

## Evidence
`main.rs:176-198` -- the `tokio::select!` returns as soon as any branch completes:
```rust
tokio::select! {
    // ...
    r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
    r = sender_handle => { if let Err(e) = r { tracing::error!("UDP sender error: {e}"); } }
    // ...
}
```

When `sim_handle` completes (game loop breaks), `main()` returns. The tokio runtime drops all remaining tasks. The sender task at `game_loop.rs:63-100` has shutdown drains:
```rust
None => {
    // Control channel closed. Drain remaining replication.
    while let Some(m) = repl_rx.recv().await {
        send_outbound(&udp_socket, m).await;
    }
    break;
}
```
But this drain is cancelled by the runtime shutdown before it completes.

The game loop's final tick (lines 331-349) enqueues transfers and forwards via `try_send` to the control channel. These messages exist in the channel buffer but the sender task may never process them.

## Impact
- **Normal drain (coordinator-initiated)**: The game loop runs until `ShutdownRequested` is set, which requires all transfers to be ACK'd or timed out. By the time the loop breaks, the channels are empty. **No data loss in this path.**
- **Ctrl+C shutdown**: The drain directive has empty targets (`main.rs:191`: `q.drain_directives.push(vec![])`), so `process_drain` returns early without transferring anything. After 3 seconds, `main()` exits. Entities are lost regardless of whether the sender drains.
- **Panic/error exit**: If the game loop panics or a preceding `select!` branch fires (e.g., WS server crashes), the sender task is cancelled with pending messages.

The practical impact is low because the coordinator-initiated drain (the main use case) waits for ACKs before breaking, so the channels are empty by the time the game loop exits.

## Recommended Fix
After the `tokio::select!` in `main.rs`, explicitly await the sender task before returning:
```rust
// After select! returns:
sender_handle.abort();  // signal shutdown
let _ = tokio::time::timeout(
    std::time::Duration::from_millis(500),
    sender_handle,
).await;
```

Or, have the game loop signal shutdown by dropping the channel senders explicitly before breaking, then await the sender task with a timeout.

## Effort: small

## Related Files
- crates/sim-server/src/main.rs:176-198 (tokio::select exits on first completion)
- crates/sim-server/src/game_loop.rs:63-100 (sender task drain logic)
- crates/sim-server/src/game_loop.rs:377-379 (game loop break)
