# 005: Tick Loop Health -- Work Items

**Design:** `designs/005-tick-loop-health.md`
**Audit findings:** #003, #013, #009
**Total work items:** 7
**Estimated total:** 5-8 hours

---

## Phase 1: Mutex Swap (Finding #003)

### WI-030: Replace tokio::Mutex with std::sync::Mutex on SharedInbound

**Size:** small
**Depends on:** none
**Files:** `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/receiver.rs`, `crates/sim-server/src/network.rs`, `crates/sim-server/src/main.rs`

**Description:**

Change `SharedInbound.queue` from `tokio::sync::Mutex<InboundQueue>` to `std::sync::Mutex<InboundQueue>`.

1. In `game_loop.rs`:
   - Change the struct field type: `pub queue: std::sync::Mutex<InboundQueue>`
   - Line ~92: change `inbound.queue.lock().await` to `inbound.queue.lock().unwrap_or_else(|e| e.into_inner())`

2. In `receiver.rs`:
   - Every `inbound.queue.lock().await` (lines ~78, ~92, ~112, ~119, ~128, ~169) becomes `inbound.queue.lock().unwrap_or_else(|e| e.into_inner())`
   - Remove `.await` from each lock call

3. In `network.rs`:
   - Every `inbound.queue.lock().await` (lines ~99, ~121, ~159, ~163, ~203) becomes `inbound.queue.lock().unwrap_or_else(|e| e.into_inner())`
   - Remove `.await` from each lock call

4. In `main.rs`:
   - Line ~100: change `tokio::sync::Mutex::new(...)` to `std::sync::Mutex::new(...)` for the SharedInbound construction
   - Line ~172 (ctrl-c handler): change `shared_coord_events.queue.lock().await` -- wait, that is SharedCoordEvents, handled in WI-031

Verify: `cargo build` must succeed. No `.await` on any `std::sync::Mutex::lock()` call.

**Tests:**
- `cargo build` -- the compiler will reject `.await` on a non-Future (std::sync::MutexGuard is not a Future)
- Run `cargo test -p sim-server` -- existing tests must pass
- Run `./test.sh mesh-verify` -- basic cluster health

**Done when:** All `SharedInbound.queue.lock()` calls use `std::sync::Mutex` with no `.await`, and the cluster starts and runs without issues.

---

### WI-031: Replace tokio::Mutex with std::sync::Mutex on SharedCoordEvents

**Size:** small
**Depends on:** none (can be done in parallel with WI-030)
**Files:** `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/coordinator_client.rs`, `crates/sim-server/src/main.rs`

**Description:**

Change `SharedCoordEvents.queue` from `tokio::sync::Mutex<CoordEventQueue>` to `std::sync::Mutex<CoordEventQueue>`.

1. In `game_loop.rs`:
   - Change the struct field type: `pub queue: std::sync::Mutex<CoordEventQueue>`
   - Line ~120: change `coord_events.queue.lock().await` to `coord_events.queue.lock().unwrap_or_else(|e| e.into_inner())`

2. In `coordinator_client.rs`:
   - Every `coord_events.queue.lock().await` (lines ~72, ~80, ~85, ~99) becomes `coord_events.queue.lock().unwrap_or_else(|e| e.into_inner())`
   - Remove `.await` from each lock call

3. In `main.rs`:
   - Line ~103: change `tokio::sync::Mutex::new(...)` to `std::sync::Mutex::new(...)` for the SharedCoordEvents construction
   - Line ~172 (ctrl-c handler): change `shared_coord_events.queue.lock().await` to `shared_coord_events.queue.lock().unwrap_or_else(|e| e.into_inner())`

**Tests:**
- `cargo build` -- compiler rejects `.await` on std MutexGuard
- `cargo test -p sim-server` -- existing tests pass
- Run `./test.sh crash-recovery` -- coordinator events still flow correctly

**Done when:** All `SharedCoordEvents.queue.lock()` calls use `std::sync::Mutex` with no `.await`.

---

### WI-032: Replace tokio::Mutex with std::sync::Mutex on MetricsCollector

**Size:** small
**Depends on:** none (can be done in parallel with WI-030 and WI-031)
**Files:** `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/metrics_server.rs`, `crates/sim-server/src/main.rs`

**Description:**

Change the metrics collector from `Arc<tokio::sync::Mutex<MetricsCollector>>` to `Arc<std::sync::Mutex<MetricsCollector>>`.

1. In `game_loop.rs`:
   - Change the function signature parameter type: `metrics: Arc<std::sync::Mutex<MetricsCollector>>`
   - Remove the `use tokio::sync::Mutex;` import if it becomes unused
   - Line ~242: change `metrics.lock().await` to `metrics.lock().unwrap_or_else(|e| e.into_inner())`

2. In `metrics_server.rs`:
   - Change the function signature: `collector: Arc<std::sync::Mutex<MetricsCollector>>`
   - Remove `use tokio::sync::Mutex;` import
   - Line ~243: change `collector.lock().await` to `collector.lock().unwrap_or_else(|e| e.into_inner())`
   - Note: this lock is called from a spawned async task handling an HTTP request. The lock is held only while calling `.snapshot()` (pure computation, ~5us). This is safe.

3. In `main.rs`:
   - Line ~110: change `Arc::new(Mutex::new(...))` to `Arc::new(std::sync::Mutex::new(...))`
   - Remove `use tokio::sync::Mutex;` if it becomes unused

**Tests:**
- `cargo build`
- `cargo test -p sim-server` -- the MetricsCollector unit tests still pass (they do not use the Arc/Mutex wrapper)
- `curl localhost:8101/metrics` after starting a local cluster -- verify JSON response

**Done when:** MetricsCollector uses `std::sync::Mutex`, metrics endpoint still serves valid JSON.

---

### WI-033: Remove unused tokio::sync::Mutex imports

**Size:** small
**Depends on:** WI-030, WI-031, WI-032
**Files:** `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/main.rs`, `crates/sim-server/src/metrics_server.rs`

**Description:**

After WI-030 through WI-032, the `tokio::sync::Mutex` import should be unused in several files. Clean up:

1. `game_loop.rs` line ~17: remove `use tokio::sync::Mutex;` if unused
2. `main.rs` line ~22: remove `use tokio::sync::Mutex;` if unused
3. `metrics_server.rs` line ~5: remove `use tokio::sync::Mutex;` if unused
4. Run `cargo build` and fix any remaining import warnings

Also verify there are no remaining `tokio::sync::Mutex` usages in the sim-server crate:

```
grep -rn "tokio::sync::Mutex" crates/sim-server/src/
```

The only remaining usage should be `tokio::sync::mpsc` (channels), which is correct and should not be changed.

**Tests:**
- `cargo build` with no warnings about unused imports
- `cargo clippy -p sim-server` -- no new warnings

**Done when:** No `tokio::sync::Mutex` remains in the sim-server crate. Build is clean.

---

## Phase 2: Clean Game Loop Exit (Finding #009)

### WI-034: Replace process::exit with loop break

**Size:** small
**Depends on:** none (independent of Phase 1, but recommended to do after)
**Files:** `crates/sim-server/src/game_loop.rs`

**Description:**

Replace `std::process::exit(0)` with `break` to allow the tokio runtime to shut down gracefully.

1. In `game_loop.rs`, at the shutdown check (line ~259-262), change:

   ```rust
   // Before:
   if world.resource::<ShutdownRequested>().0 {
       tracing::info!("Drain complete -- shutting down");
       std::process::exit(0);
   }
   ```

   To:

   ```rust
   // After:
   if world.resource::<ShutdownRequested>().0 {
       tracing::info!("Drain complete -- shutting down");
       break;
   }
   ```

2. After the `loop { ... }` block, add a final log line:

   ```rust
   tracing::info!("Game loop exited cleanly");
   ```

3. Remove `use std::process;` or similar if it becomes unused (check -- it may not exist as a separate import since it was used inline as `std::process::exit`).

4. Verify that `main.rs` handles the game loop returning. The existing code:

   ```rust
   r = sim_handle => { if let Err(e) = r { tracing::error!("Game loop error: {e}"); } }
   ```

   This handles `Ok(())` silently and `Err` with a log. This is correct behavior. When the game loop returns normally after drain, the `tokio::select!` fires, and the process exits.

**Tests:**
- `cargo build`
- `cargo test -p sim-server` -- the `coord_drain_two_phase_in_schedule` and `coord_drain_timeout_forces_shutdown` tests verify that `ShutdownRequested` is set correctly. These tests exercise the ECS schedule, not the outer game loop, so they are unaffected.
- Manual test: start cluster, send drain directive (via coordinator API or by stopping the coordinator), verify server exits with code 0 and logs "Game loop exited cleanly"

**Done when:** Server exits cleanly on drain without calling `process::exit`. The tokio runtime shuts down naturally. Exit code is 0.

---

## Phase 3: UDP Sender Task (Finding #013)

### WI-035: Create UDP sender task with channel

**Size:** medium
**Depends on:** WI-030 (mutex swap should be done first to avoid mixing tokio::Mutex and std::Mutex patterns)
**Files:** `crates/sim-server/src/game_loop.rs`, `crates/sim-server/src/main.rs`

**Description:**

Move all `udp_socket.send_to().await` calls out of the game loop tick and into a dedicated sender task.

**Step 1: Define the outbound message type.**

In `game_loop.rs` (or a new file `udp_sender.rs` if preferred), add:

```rust
/// Message from the game loop to the UDP sender task.
pub enum UdpOutbound {
    /// Single datagram to send to a specific peer.
    Send { addr: SocketAddr, data: Vec<u8> },
    /// Batch of datagrams to send.
    Batch(Vec<(SocketAddr, Vec<u8>)>),
}
```

**Step 2: Create the sender task function.**

```rust
/// Dedicated task that sends UDP datagrams from a channel.
/// Runs until the channel sender is dropped (game loop exits).
pub async fn run_udp_sender(
    udp_socket: Arc<UdpSocket>,
    mut rx: tokio::sync::mpsc::Receiver<UdpOutbound>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            UdpOutbound::Send { addr, data } => {
                if let Err(e) = udp_socket.send_to(&data, addr).await {
                    tracing::warn!("UDP send to {addr} failed: {e}");
                }
            }
            UdpOutbound::Batch(items) => {
                for (addr, data) in items {
                    if let Err(e) = udp_socket.send_to(&data, addr).await {
                        tracing::warn!("UDP send to {addr} failed: {e}");
                    }
                }
            }
        }
    }
    tracing::info!("UDP sender task exiting (channel closed)");
}
```

**Step 3: Wire up in main.rs.**

```rust
// Create UDP sender channel
const UDP_SENDER_CHANNEL_SIZE: usize = 64;
let (udp_tx, udp_rx) = tokio::sync::mpsc::channel::<game_loop::UdpOutbound>(UDP_SENDER_CHANNEL_SIZE);

// Start UDP sender task
let sender_handle = tokio::spawn(game_loop::run_udp_sender(
    Arc::clone(&udp_socket),
    udp_rx,
));
```

Pass `udp_tx` to `run_game_loop_ecs` instead of `udp_socket`. Update the function signature:

```rust
pub async fn run_game_loop_ecs(
    server_id: ServerId,
    initial_npcs: u32,
    inbound: Arc<SharedInbound>,
    coord_events: Arc<SharedCoordEvents>,
    udp_tx: tokio::sync::mpsc::Sender<UdpOutbound>,  // was: udp_socket: Arc<UdpSocket>
    metrics: Arc<std::sync::Mutex<MetricsCollector>>,
    entity_count_atomic: Arc<AtomicU32>,
)
```

**Step 4: Replace send_to with channel sends in the game loop.**

Replace the entire Phase 4 block (lines ~162-232) with channel-based sends:

```rust
// -- Phase 4: Enqueue UDP replication for sender task --
let t2 = Instant::now();
if let Some(entities) = &repl_data {
    let peer_addrs: Vec<_> = world
        .resource::<PeerAddresses>()
        .0
        .values()
        .copied()
        .collect();

    if !peer_addrs.is_empty() {
        let server_id_u16 = world.resource::<ServerConfig>().server_id;

        if entities.len() <= ENTITIES_PER_CHUNK {
            let msg = PeerMessage::EntityStateUpdate {
                tick: repl_tick,
                server_id: server_id_u16,
                entities: entities.clone(),
            };
            if let Ok(bytes) = bincode::serialize(&msg) {
                for addr in &peer_addrs {
                    let _ = udp_tx.try_send(UdpOutbound::Send {
                        addr: *addr,
                        data: bytes.clone(),
                    });
                }
            }
        } else {
            let total_chunks =
                ((entities.len() + ENTITIES_PER_CHUNK - 1) / ENTITIES_PER_CHUNK) as u16;
            for (i, chunk) in entities.chunks(ENTITIES_PER_CHUNK).enumerate() {
                let msg = PeerMessage::ChunkedEntityUpdate {
                    tick: repl_tick,
                    server_id: server_id_u16,
                    chunk_index: i as u16,
                    total_chunks,
                    entities: chunk.to_vec(),
                };
                if let Ok(bytes) = bincode::serialize(&msg) {
                    for addr in &peer_addrs {
                        let _ = udp_tx.try_send(UdpOutbound::Send {
                            addr: *addr,
                            data: bytes.clone(),
                        });
                    }
                }
            }
        }
    }
}

// Enqueue authority transfers
if !transfers.is_empty() {
    let _ = udp_tx.try_send(UdpOutbound::Batch(transfers));
}

// Enqueue forwarded inputs
if !forwards.is_empty() {
    let _ = udp_tx.try_send(UdpOutbound::Batch(forwards));
}
let transfer_dur = t2.elapsed();
```

Key points:
- Use `try_send` (non-blocking) instead of `.send().await`
- If `try_send` returns `Err(TrySendError::Full)`, the data is dropped -- equivalent to a lost UDP packet
- Serialization still happens in the game loop (pure CPU, no yield)
- The bytes are cloned once per peer for replication (same as before -- `entities.clone()` was already happening)
- Transfers and forwards are batched into single channel messages to reduce channel overhead

**Step 5: Add the sender handle to the tokio::select! in main.rs.**

```rust
tokio::select! {
    // ... existing handles ...
    r = sender_handle => { if let Err(e) = r { tracing::error!("UDP sender error: {e}"); } }
    // ...
}
```

**Tests:**
- `cargo build`
- `cargo test -p sim-server` -- existing tests pass (they test the ECS schedule, not the outer loop)
- New unit test: create a channel, send `UdpOutbound::Send` and `UdpOutbound::Batch`, verify they arrive at the receiver
- `./test.sh peer-replication` -- verify shadow counts are correct (replication still works)
- `./test.sh authority-transfer` -- verify transfers complete (transfer messages still sent)
- `./test.sh forwarded-input` -- verify forwarded inputs work

**Done when:** No `udp_socket.send_to().await` calls remain in `game_loop.rs`. All UDP sends go through the channel. Replication, transfers, and forwarded inputs work correctly in the test suite.

---

### WI-036: Add try_send failure logging for UDP sender channel

**Size:** small
**Depends on:** WI-035
**Files:** `crates/sim-server/src/game_loop.rs`

**Description:**

After WI-035, `try_send` failures are silently discarded with `let _ = udp_tx.try_send(...)`. Add logging so operators can see when the sender task is falling behind.

1. Replace each `let _ = udp_tx.try_send(...)` with:

   ```rust
   if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = udp_tx.try_send(...) {
       tracing::warn!("UDP sender channel full, dropping outbound data");
   }
   ```

2. Use a rate-limited log to avoid spam. Add a counter to the game loop:

   ```rust
   // Before the loop:
   let mut udp_send_drops: u64 = 0;

   // On each drop:
   udp_send_drops += 1;

   // In the debug logging block (every 5 seconds):
   if udp_send_drops > 0 {
       tracing::warn!("UDP sender channel full: {} drops in last 5s", udp_send_drops);
       udp_send_drops = 0;
   }
   ```

   This avoids per-tick log spam while still surfacing the problem.

3. If the channel is closed (sender task crashed), `try_send` returns `TrySendError::Closed`. Log this at error level:

   ```rust
   if let Err(e) = udp_tx.try_send(...) {
       match e {
           TrySendError::Full(_) => udp_send_drops += 1,
           TrySendError::Closed(_) => {
               tracing::error!("UDP sender task has crashed, replication disabled");
               // Continue running -- the game loop should not crash just because
               // UDP sending failed. The server is still authoritative for its entities.
           }
       }
   }
   ```

**Tests:**
- `cargo build`
- Manual test: temporarily set channel size to 1, spawn 500 entities, verify warning logs appear
- Restore channel size to 64, verify no warnings under normal load

**Done when:** UDP sender channel drops are logged with rate limiting. Closed channel is logged at error level. No silent data loss.

---

## Summary

| WI | Title | Size | Depends on | Finding |
|----|-------|------|------------|---------|
| 030 | std::sync::Mutex on SharedInbound | small | -- | #003 |
| 031 | std::sync::Mutex on SharedCoordEvents | small | -- | #003 |
| 032 | std::sync::Mutex on MetricsCollector | small | -- | #003 |
| 033 | Remove unused tokio::sync::Mutex imports | small | 030, 031, 032 | #003 |
| 034 | Replace process::exit with loop break | small | -- | #009 |
| 035 | Create UDP sender task with channel | medium | 030 | #013 |
| 036 | Add try_send failure logging | small | 035 | #013 |

**Parallelism:** WI-030, WI-031, WI-032, and WI-034 are all independent and can be done in parallel (or even as a single commit since they are all small). WI-033 depends on the first three. WI-035 depends on WI-030 (to avoid mixing mutex styles). WI-036 depends on WI-035.

**Suggested commit groupings:**
- Commit 1: WI-030 + WI-031 + WI-032 + WI-033 (all mutex swaps in one pass)
- Commit 2: WI-034 (clean exit)
- Commit 3: WI-035 + WI-036 (UDP sender task)
