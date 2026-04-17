# Async UDP Sends Block the Game Loop Tick

## Severity: high

## Category: bottleneck

## Summary
The game loop sends UDP replication packets, authority transfers, and forwarded inputs using `await` within the tick loop body. Each `udp_socket.send_to().await` is an async syscall that yields to the runtime. With N peers, the game loop performs N sequential async sends per tick for replication, plus additional sends for transfers and forwards. These sends are serialized -- the loop does not send in parallel, and the game loop cannot proceed to Phase 5 (client broadcast) until all UDP sends complete.

## Evidence
`game_loop.rs` lines 167-195:
```rust
// Sequential sends to each peer
for addr in &peer_addrs {
    if let Err(e) = udp_socket.send_to(&bytes, addr).await {  // AWAIT in loop
        tracing::warn!("Failed to send to peer {addr}: {e}");
    }
}

// Sequential transfer sends
for (addr, bytes) in &transfers {
    if let Err(e) = udp_socket.send_to(bytes, addr).await {  // AWAIT in loop
        tracing::warn!("Failed to send transfer: {e}");
    }
}

// Sequential forward sends
for (addr, bytes) in &forwards {
    if let Err(e) = udp_socket.send_to(bytes, addr).await {  // AWAIT in loop
        tracing::warn!("Failed to forward input: {e}");
    }
}
```

Each `send_to` is a syscall. On Linux/macOS, UDP `sendto` is typically non-blocking and completes in ~1-5us. But with 5 peers and a 60KB replication packet, that's 5 syscalls for replication + N transfer packets + M forward packets.

Worse, `send_to` can block when the OS send buffer is full. Under load (high entity count, many peers), the OS UDP send buffer fills up and `send_to` either blocks or returns `WouldBlock` (which tokio retries). This directly stalls the tick loop.

## Impact
- **3 peers, 500 entities**: 3 replication sends * ~5us + 0 transfers = ~15us. Negligible.
- **10 peers, 500 entities**: 10 replication sends * ~5us = ~50us. Still minor.
- **5 peers, 50 transfers + 100 forwards**: 5 + 50 + 100 = 155 sends * ~5us = ~0.8ms. Starts to matter (1.6% of tick budget).
- **Send buffer full**: A single stalled `send_to` can block for milliseconds. If the network is congested (UNO Q boards on WiFi), this happens frequently.

The total UDP send time is measured as `transfer_dur` in the metrics, but replication sends and forward sends are lumped together, making it hard to diagnose which is the bottleneck.

## Recommended Fix
Move UDP sending to a separate async task that reads from an outbound channel:

```rust
// In main.rs, spawn a UDP sender task:
let (udp_tx, mut udp_rx) = tokio::sync::mpsc::channel::<(SocketAddr, Vec<u8>)>(1024);

tokio::spawn(async move {
    while let Some((addr, bytes)) = udp_rx.recv().await {
        let _ = udp_socket.send_to(&bytes, addr).await;
    }
});

// In game_loop.rs, replace direct sends with channel sends:
for addr in &peer_addrs {
    let _ = udp_tx.try_send((addr, bytes.clone()));
}
```

This decouples UDP I/O from the game loop tick timing. The game loop only needs to push to a channel (non-blocking), and the sender task handles the actual syscalls asynchronously.

For the replication packet specifically, use `sendmmsg` (send multiple messages) on Linux to batch all peer sends into a single syscall.

## Effort: medium

## Related Files
- crates/sim-server/src/game_loop.rs:148-195 (all UDP sends in tick loop)
- crates/sim-server/src/game_loop.rs:196 (transfer_dur timing -- includes all sends)
