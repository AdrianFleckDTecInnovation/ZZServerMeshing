# Tokio Mutex Awaited Inside the 50ms Tick Loop

## Severity: high

## Category: bottleneck

## Summary
The game loop acquires three `tokio::sync::Mutex` locks every tick (inbound queue, coordinator events, metrics collector). These are async mutexes contended with async tasks that receive UDP packets, WebSocket messages, and coordinator events at wire speed. Under load, the game loop can stall waiting for a lock held by a receiver task that is in the middle of pushing a large `EntityStateUpdate` containing hundreds of entities, directly eating into the 50ms tick budget.

## Evidence
`game_loop.rs` lines 91-92, acquiring the inbound lock:
```rust
let mut iq = inbound.queue.lock().await;  // blocks on contention
```

The UDP receiver (`receiver.rs` lines 37-43) holds this same lock while pushing peer updates:
```rust
let mut q = inbound.queue.lock().await;
q.peer_updates.push(PeerUpdate {
    tick,
    server_id,
    entities,  // Vec<EntityState> -- can be hundreds of entities
});
```

The receiver holds the lock for the duration of the `push`, which involves potential Vec reallocation. With N peers each sending updates, the receiver acquires this lock N times per tick period.

Similarly, `game_loop.rs` line 208 acquires the metrics mutex:
```rust
let mut m = metrics.lock().await;
```

The metrics server (`metrics_server.rs` line 243) also locks this to serve HTTP requests:
```rust
let snapshot = collector.lock().await.snapshot();
```

The `snapshot()` method (metrics.rs lines 75-96) sorts a 200-element vector to compute percentiles -- a non-trivial operation that holds the lock during computation.

## Impact
- **Normal operation**: The lock is held briefly by each receiver task. With 2 peers and moderate traffic, contention is low (~microseconds).
- **Under load**: With 5+ peers each sending 500-entity updates at 20Hz, the receiver task is continuously processing packets. The game loop may wait 0.5-2ms for the lock, directly adding to tick budget.
- **Metrics HTTP request during tick**: If a dashboard or monitoring tool polls `/metrics` at the wrong moment, the game loop waits for the percentile computation to complete (~50-100us for 200-element sort).
- **Worst case**: A burst of UDP packets arriving just as the game loop tries to swap causes lock convoy, where the game loop and multiple receiver invocations queue on the same lock.

## Recommended Fix
Replace the `tokio::sync::Mutex<InboundQueue>` with a lock-free swap buffer pattern:

```rust
pub struct SharedInbound {
    pub queue: std::sync::Mutex<InboundQueue>,
    // OR better:
    pub queue: parking_lot::Mutex<InboundQueue>,
}
```

Use `std::sync::Mutex` (or `parking_lot::Mutex`) instead of `tokio::sync::Mutex`. The critical section is always short (just a `std::mem::take`), so a spinning mutex is faster and avoids the overhead of yielding to the async runtime. Tokio's async mutex is designed for locks held across await points, which is not the case here.

Even better, use a double-buffer pattern:
1. Receivers push to buffer A (behind a lock)
2. Game loop swaps buffer A with empty buffer B atomically (single lock acquisition)
3. Game loop processes buffer B without any lock held

For metrics, either:
- Compute the snapshot outside the lock (clone the ring buffer, drop lock, then compute)
- Use `try_lock()` for the metrics HTTP handler and return cached data on contention

## Effort: small

## Related Files
- crates/sim-server/src/game_loop.rs:91-119 (lock acquisitions in tick loop)
- crates/sim-server/src/game_loop.rs:207-221 (metrics lock in tick loop)
- crates/sim-server/src/receiver.rs:37-43 (lock held during push)
- crates/sim-server/src/network.rs:99-101 (lock held during client input push)
- crates/sim-server/src/metrics_server.rs:243 (lock held during snapshot computation)
