# 005: Tick Loop Health

**Status:** Designed, ready for implementation
**Priority:** High
**Audit findings:** #003 (HIGH), #013 (HIGH), #009 (MED)
**Files affected:** `game_loop.rs`, `receiver.rs`, `coordinator_client.rs`, `network.rs`, `metrics_server.rs`, `metrics.rs`, `resources.rs`, `main.rs`

## Problem Statement

The game loop runs at 20 Hz (50ms budget per tick). Three classes of blocking operations inside the loop introduce unpredictable latency that steals time from simulation:

1. **Tokio mutex contention (#003).** Three `tokio::Mutex::lock().await` calls per tick yield to the tokio scheduler when contended. The UDP receiver, WebSocket handlers, and coordinator client all push events through these same mutexes from concurrent tasks. Under load (UDP burst, many connected clients), a single lock acquisition can stall the game loop for hundreds of microseconds or more while tokio polls other tasks.

2. **UDP sends blocking the tick (#013).** After the ECS schedule runs, the game loop serializes replication data and calls `udp_socket.send_to().await` once per chunk per peer, plus once per transfer message and once per forwarded input. With 3 peers and 200+ entities (3 chunks), that is 9+ awaited send calls per tick. Each await yields to the scheduler and adds syscall latency. This is the "transfers" phase in metrics, and it is the only phase that does network I/O.

3. **`process::exit` bypasses cleanup (#009).** When drain completes, the game loop calls `std::process::exit(0)`, which terminates the process immediately without running Drop impls or letting tokio shut down. This is harmless today but will break once player persistence needs a flush-on-shutdown path, and it makes the server unkillable by the tokio runtime (the `tokio::select!` in main.rs never regains control).

## Design

### Fix 1: Replace tokio::Mutex with std::sync::Mutex (Finding #003)

**Current architecture.** Three shared buffers use `tokio::sync::Mutex`:

| Buffer | Struct | Producers | Consumer |
|--------|--------|-----------|----------|
| `SharedInbound.queue` | `InboundQueue` | UDP receiver, WebSocket handlers, coordinator client | Game loop (start of tick) |
| `SharedCoordEvents.queue` | `CoordEventQueue` | Coordinator client | Game loop (start of tick) |
| `MetricsCollector` | `MetricsCollector` | Game loop (end of tick) | Metrics HTTP server |

All three follow the same pattern: producers hold the lock briefly to push one item, the consumer holds it briefly to `std::mem::take` all items. The critical section is always sub-microsecond (a pointer swap or a Vec push). There is no async work done while the lock is held.

**Why tokio::Mutex is wrong here.** `tokio::Mutex` is designed for locks held across `.await` points. When contended, it yields the current task back to the tokio scheduler, which polls other futures before retrying. For a sub-microsecond critical section, this context-switch overhead is far worse than spinning. The tokio documentation explicitly recommends `std::sync::Mutex` for short critical sections.

**Fix.** Replace `tokio::sync::Mutex` with `std::sync::Mutex` on all three buffers. Use `.lock().unwrap()` (the lock is never poisoned because no thread panics while holding it). In the game loop, change `.lock().await` to `.lock().unwrap()`. In all producers, do the same.

The semantics are identical: lock, push/take, unlock. The difference is that `std::sync::Mutex` spins briefly on contention instead of yielding, which is correct for sub-microsecond holds.

**Impact on producers.** The producers (`receiver.rs`, `network.rs`, `coordinator_client.rs`) are all `async fn` running on the tokio runtime. Calling `std::sync::Mutex::lock()` from an async context is safe as long as the lock is not held across an `.await` point, which is already the case -- every lock scope in the codebase is a block `{ let mut q = lock(); q.push(...); }` with no await inside.

**Quantified cost.** `std::sync::Mutex::lock()` on an uncontended lock is ~15-25ns on ARM (the UNO Q boards). Under contention with the sub-microsecond critical sections in this codebase, worst case is a few hundred nanoseconds of spinning. Compare to tokio::Mutex contention which involves a full task yield + reschedule (~1-10us minimum, potentially much more under load).

### Fix 2: Move UDP sends off the game loop (Finding #013)

**Current architecture.** After `schedule.run()`, the game loop:

1. Extracts `OutboundReplication.snapshot` (a `Vec<EntityState>`) from the ECS world
2. Serializes it into one or more `PeerMessage` variants via `bincode::serialize`
3. Sends each serialized chunk to each peer via `udp_socket.send_to(&bytes, addr).await`
4. Extracts `OutboundTransfers.messages` and `OutboundForwards.messages` (pre-serialized `Vec<(SocketAddr, Vec<u8>)>`)
5. Sends each one via `udp_socket.send_to().await`

Steps 2-3 and 5 are the problem. Each `.await` on `send_to` yields to the tokio scheduler. The send itself is a UDP write syscall (~0.5-2us per call on Linux), but the await/reschedule overhead can add 5-50us per call depending on scheduler load.

With 3 peers and 200 entities (2 chunks each), step 3 alone is 6 awaited sends. With transfer retries and forwarded inputs, the total can exceed 15 awaited sends per tick.

**Fix.** Introduce a dedicated UDP sender task that reads from an `mpsc` channel. The game loop serializes all outbound data (which is pure CPU work, no I/O) and sends the resulting byte buffers through the channel. The sender task drains the channel and does the actual `send_to` calls.

The channel type is `tokio::sync::mpsc::Sender<UdpOutbound>` where:

```rust
pub enum UdpOutbound {
    /// Serialized bytes to send to a specific peer.
    Send { addr: SocketAddr, data: Vec<u8> },
    /// Batch of (addr, data) pairs to send.
    Batch(Vec<(SocketAddr, Vec<u8>)>),
}
```

The game loop becomes:

```
Phase 4 (was: send UDP replication):
  1. Extract OutboundReplication.snapshot from ECS
  2. Serialize into PeerMessage chunks (CPU only)
  3. For each (chunk_bytes, peer_addr): push UdpOutbound::Send to channel
  4. Extract OutboundTransfers and OutboundForwards
  5. Push UdpOutbound::Batch for transfers
  6. Push UdpOutbound::Batch for forwards
  -- no awaits, channel send is synchronous (try_send or send) --
```

The sender task:

```
loop {
    while let Ok(msg) = rx.try_recv() {  // drain all queued
        match msg {
            UdpOutbound::Send { addr, data } => udp_socket.send_to(&data, addr).await,
            UdpOutbound::Batch(items) => {
                for (addr, data) in items {
                    udp_socket.send_to(&data, addr).await;
                }
            }
        }
    }
    // yield to let more items accumulate
    tokio::task::yield_now().await;
}
```

**Serialization stays in the game loop.** Serialization is pure CPU (~5-20us for 100 entities with bincode). It does not yield to the scheduler. Moving it to another task would require cloning the entity data, which costs more than just serializing it in place. The game loop already has the data; it should do the serialization, then hand off the bytes.

**Channel sizing.** The channel should be bounded to prevent unbounded memory growth if the sender task falls behind. A capacity of 64 is sufficient: worst case is ~15 messages per tick, and the sender task processes them faster than they arrive (UDP sends are fast, the issue was the await overhead in the game loop context).

If the channel is full (sender task is badly lagging), `try_send` fails and the game loop drops that batch. This is correct behavior -- UDP is unreliable, and dropping one tick of replication data is equivalent to a lost packet. The peer will recover on the next tick.

**Timing measurement.** The current "transfers" phase timer covers both serialization and sending. After this fix, it should cover only serialization (since sending is async). This gives more accurate metrics -- the game loop tick total will no longer include I/O time.

**Quantified impact.** Currently, with 3 peers and 2 chunks (200 entities), Phase 4 costs approximately:
- Serialization: ~30us (2 chunks * ~15us each)
- 6 send_to awaits: ~30-300us (5-50us each depending on scheduler)
- Total: 60-330us

After the fix:
- Serialization: ~30us
- 6 channel sends: ~6us (try_send is ~1us each, no await)
- Total: ~36us

This saves 25-295us per tick, which is 0.05-0.6% of tick budget. Not transformative, but it removes the unpredictable variance -- the p99 spike potential goes from hundreds of microseconds to near-zero.

### Fix 3: Clean game loop exit (Finding #009)

**Current code.** At the end of each tick:

```rust
if world.resource::<ShutdownRequested>().0 {
    tracing::info!("Drain complete -- shutting down");
    std::process::exit(0);
}
```

This terminates the entire process immediately. The `tokio::select!` in `main.rs` never completes. Drop impls on the World, UDP socket, metrics collector, etc. never run.

**Fix.** Change `run_game_loop_ecs` to return normally when shutdown is requested:

```rust
if world.resource::<ShutdownRequested>().0 {
    tracing::info!("Drain complete -- shutting down");
    break;  // exit the tick loop
}
```

The function `run_game_loop_ecs` is `async fn` returning `()`. When it returns, the `tokio::spawn` in `main.rs` completes, which triggers the `tokio::select!` match arm for `sim_handle`. The runtime can then shut down gracefully, running Drop impls.

**No changes to main.rs.** The existing `tokio::select!` already handles the sim_handle completing:

```rust
r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
```

When the game loop breaks and returns, this arm fires with `Ok(())`, and the program exits normally. All other spawned tasks (WS server, UDP receiver, coordinator client, metrics server) are dropped when the runtime shuts down.

**Optional: return a result.** For future extensibility (persistence flush errors, etc.), the function could return `Result<(), Box<dyn std::error::Error>>`. But this is over-engineering for now. Returning `()` and breaking the loop is sufficient.

## Protocol Changes

None. All three fixes are internal to the sim-server. No wire format changes, no coordinator protocol changes, no client changes.

## Failure Modes

### Fix 1 (std::sync::Mutex)

**Risk: deadlock from holding lock across await.** Mitigated by code review -- every lock scope in the codebase is a `{ let mut q = lock(); q.push(...); }` block with no `.await` inside. The compiler will not catch this (unlike tokio::Mutex where holding across await is the intended use), so care must be taken in future modifications.

**Risk: lock poisoning.** If a producer panics while holding the lock, `std::sync::Mutex` poisons it and subsequent `.lock().unwrap()` calls will panic. In practice, the critical sections are infallible (Vec::push, std::mem::take), so this cannot happen. As defense-in-depth, use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poisoned locks (the data is still valid).

**Risk: blocking the tokio runtime.** `std::sync::Mutex::lock()` blocks the OS thread. On a single-threaded tokio runtime (which is not what we use -- we use multi-threaded), this would be catastrophic. On the default multi-threaded runtime, it is fine for sub-microsecond holds. The UNO Q boards have 4 cores, so blocking one thread briefly does not starve the runtime.

### Fix 2 (UDP sender task)

**Risk: channel full, replication dropped.** If the sender task falls behind, `try_send` fails and the game loop drops that tick's replication data. This is identical to a lost UDP packet -- peers miss one tick of updates and recover on the next. Logging the drop is important for observability.

**Risk: sender task crashes.** If the sender task panics (e.g., socket error), the channel sender is dropped and `try_send` returns `Err(Full)` or the receiver end is closed. The game loop should handle this gracefully -- log the error and continue. The replication data is lost but the game loop is unaffected.

**Risk: ordering.** The channel preserves FIFO order. The sender task sends in the same order the game loop enqueues. This matches the current behavior where replication is sent before transfers before forwards.

### Fix 3 (clean exit)

**Risk: tasks not shut down.** When the game loop returns, the `tokio::select!` fires and the runtime begins shutdown. All other spawned tasks (WS, UDP, coordinator, metrics) are cancelled. This is the correct behavior -- the server is draining, and all entities have been transferred.

**Risk: main.rs ctrl-c handler race.** The ctrl-c handler in `main.rs` pushes a drain directive and then sleeps 3 seconds. If the drain completes (game loop breaks) before the 3 second sleep expires, the `tokio::select!` fires for `sim_handle` instead of the sleep. This is fine -- it means the drain was fast.

## Testing Plan

### Unit tests

1. **std::sync::Mutex swap compiles.** Change the types, run `cargo build`. The compiler will catch any `.await` on `std::sync::Mutex::lock()`.

2. **UDP sender channel.** Test that `UdpOutbound::Send` and `UdpOutbound::Batch` round-trip through an mpsc channel. Test that the channel drops items when full.

3. **Game loop exit.** In the existing schedule tests, verify that setting `ShutdownRequested(true)` causes the game loop to return (not call `process::exit`). This requires restructuring the test to run the game loop function, which may be impractical. Instead, verify via the schedule tests that `ShutdownRequested` is set correctly after drain completes.

### Integration tests

1. **Drain shutdown.** Start a single-server cluster, trigger drain via coordinator, verify the server process exits cleanly (exit code 0) within 5 seconds. Verify the coordinator detects the server going away (heartbeat timeout).

2. **Tick timing under load.** Spawn 200 entities, measure p95 tick total before and after the fixes. Expect p95 to decrease and variance to decrease.

### Scene tests

The existing `crash-recovery` and `authority-transfer` scene tests exercise the drain and replication paths. They should continue passing after these changes.

## Migration

No migration needed. These are internal implementation changes. Deploy the new binary and it works. No client changes, no coordinator changes, no protocol version bump.

## Implementation Order

The three fixes are independent and can be implemented in any order, but the recommended sequence is:

1. **#003 (mutex swap)** -- smallest change, highest impact, touches the most files but all changes are mechanical (`.lock().await` becomes `.lock().unwrap()`).
2. **#009 (clean exit)** -- one-line change, no risk, good to do while the game_loop.rs file is being modified.
3. **#013 (UDP sender)** -- largest change, introduces a new task and channel, should be done last so it can be tested against the already-fixed mutex behavior.
