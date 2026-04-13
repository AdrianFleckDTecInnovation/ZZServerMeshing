# 002: Delta Compression -- Work Items

Design: `designs/002-delta-compression.md`

Total work items: 10
Estimated total: 15-22 hours

---

### WI-001: Per-entity interpolation timestamps (client)
**Size:** medium (1-2 hours)
**Depends on:** none
**Files:**
- `client/src/world-view.ts`

**Description:**
Add a `lastUpdateTime: number` field to `EntityData`. This replaces the global `lastSnapshotTime` for interpolation, fixing the root cause of the previous delta compression failure.

Changes to `EntityData`:
- Add `lastUpdateTime: number` field, initialized to `performance.now()` on creation.

Changes to `applySnapshot`:
- Set `entity.lastUpdateTime = now` for every entity in the snapshot (new and existing).
- Remove the global `this.lastSnapshotTime = now` line.

Changes to `applyDelta`:
- Set `entity.lastUpdateTime = now` for entities in the `updates` array only.
- Do NOT touch `lastUpdateTime` for entities not in the delta.

Changes to `interpolate`:
- Replace the global `timeSinceSnapshot` calculation with per-entity calculation:
  ```typescript
  const timeSinceUpdate = now - entity.lastUpdateTime;
  const alpha = Math.min(timeSinceUpdate / this.measuredIntervalMs, 1.2);
  ```
- Remove `this.lastSnapshotTime` field entirely.
- Keep `prevSnapshotTime` and `measuredIntervalMs` -- these track overall network cadence and are still useful globally.

Preserve the warmup logic: during the first `WARMUP_SNAPSHOTS` frames, snap all entities to their positions without interpolation (set `lastUpdateTime` far in the past or skip interpolation via the existing `snapshotCount` check).

**Tests:**
1. Unit test: create WorldView, apply snapshot with 3 entities, then apply delta with only 1 entity updated. Call `interpolate`. Verify the 1 updated entity interpolates from its new `previousPosition` to `currentPosition`, while the other 2 entities have fully converged (alpha >= 1.0) and sit at their `currentPosition`.
2. Unit test: verify that after a full snapshot, all entities have the same `lastUpdateTime` (within 1ms tolerance).
3. Unit test: apply two deltas 50ms apart, each updating a different entity. Verify each entity's `lastUpdateTime` reflects when IT was last updated, not when the last frame arrived.

**Done when:**
- `lastSnapshotTime` is removed from WorldView.
- Each entity has its own `lastUpdateTime`.
- Interpolation uses per-entity timestamps.
- Full snapshots work identically to before (all entities update).
- Delta frames only reset interpolation for changed entities.
- No position jumps or warping visible when running with the current (full snapshot only) server.

---

### WI-002: ClientRegistry resource and per-client channels (server)
**Size:** large (3-4 hours)
**Depends on:** none (can be done in parallel with WI-001)
**Files:**
- `crates/sim-server/src/resources.rs` (add ClientRegistry, ClientHandle)
- `crates/sim-server/src/network.rs` (replace broadcast with per-client mpsc)
- `crates/sim-server/src/game_loop.rs` (replace broadcast_tx with ClientRegistry)
- `crates/sim-server/src/main.rs` (remove broadcast channel creation)

**Description:**
Replace the single `broadcast::Sender<Vec<u8>>` with a per-client channel architecture. This is the plumbing prerequisite for delta compression -- each client needs its own frame.

**New types in `resources.rs`:**
```rust
use std::sync::atomic::AtomicU64;

#[derive(Resource, Default)]
pub struct ClientRegistry {
    pub clients: HashMap<u64, ClientHandle>,
    next_id: u64,
}

impl ClientRegistry {
    pub fn register(&mut self, tx: mpsc::Sender<Vec<u8>>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.clients.insert(id, ClientHandle {
            tx,
            player_id: None,
            acked_tick: Arc::new(AtomicU64::new(0)),
            baseline: None,
            ticks_since_full: 0,
        });
        id
    }

    pub fn unregister(&mut self, id: u64) {
        self.clients.remove(&id);
    }
}

pub struct ClientHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub player_id: Option<EntityId>,
    pub acked_tick: Arc<AtomicU64>,
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub ticks_since_full: u32,
}
```

**Add to InboundQueue:**
```rust
pub register_clients: Vec<(u64, mpsc::Sender<Vec<u8>>, Arc<AtomicU64>)>,
pub unregister_clients: Vec<u64>,
pub client_player_assignments: Vec<(u64, EntityId)>,
```

**Changes to `network.rs`:**
- `run_websocket_server` signature changes: replace `broadcast_tx: broadcast::Sender<Vec<u8>>` with `client_registry: Arc<tokio::sync::Mutex<ClientRegistryAsync>>` (a lightweight async-side registry that wraps the channel creation and communicates with the ECS registry via the inbound queue).
- Alternatively (simpler): pass in a channel through which the `handle_client` task can request registration. Use a `mpsc::Sender<ClientRegistrationEvent>` where the event contains the new client's `mpsc::Sender<Vec<u8>>`.
- The simplest approach: `handle_client` creates a `mpsc::channel(8)`, pushes a `RegisterClient { client_id, tx, acked_tick }` event to the inbound queue, and receives frames from the `rx` end. This avoids shared mutable state in the network module.
- On disconnect: push `UnregisterClient { client_id }` to the inbound queue.

**Changes to `game_loop.rs`:**
- Add `ClientRegistry` as an ECS resource, initialized in `run_game_loop_ecs`.
- Remove the `broadcast_tx` parameter.
- Phase 1 (swap inbound data): also drain `register_clients` and `unregister_clients` from the inbound queue into the ECS `ClientRegistry`.
- Phase 5 (broadcast client frame): iterate `ClientRegistry.clients`, try_send the frame to each client's `tx`. If the channel is full (client lagged), log a warning and skip.

**Changes to `main.rs`:**
- Remove `broadcast::channel` creation.
- Wire up the new registration mechanism.

**In this work item, ALL clients still receive the same full snapshot frame.** The per-client delta logic comes in WI-005. This work item only changes the delivery mechanism from broadcast to per-client channels, ensuring functional parity.

**Tests:**
1. Unit test: `ClientRegistry::register` returns unique IDs and stores handles. `ClientRegistry::unregister` removes them.
2. Integration test: connect 2 clients, verify both receive frames via their per-client channels. Disconnect 1, verify the other still receives. Reconnect, verify the new client gets frames.
3. Verify no regressions: run `./test.sh` -- all existing scenarios pass (spawn-burst, mesh-verify, etc).

**Done when:**
- `broadcast::Sender<Vec<u8>>` is removed from the codebase.
- Each connected client has its own `mpsc` channel.
- The game loop sends the same full snapshot frame to each client individually.
- All existing tests pass unchanged.
- No functional difference from the user's perspective.

---

### WI-003: Add drain_client_events system
**Size:** small (30-60 min)
**Depends on:** WI-002
**Files:**
- `crates/sim-server/src/systems.rs` (new system)
- `crates/sim-server/src/schedule.rs` (add to schedule)
- `crates/sim-server/src/resources.rs` (InboundQueue fields)

**Description:**
Add a system that drains client registration/unregistration events from the InboundQueue into the ECS ClientRegistry resource. This runs at the start of the tick, after `drain_inbound_events`.

The system:
1. Takes `ResMut<InboundQueue>` and `ResMut<ClientRegistry>`.
2. Drains `inbound.register_clients` -- for each, calls `registry.register_from_event(...)` (or directly inserts into the HashMap).
3. Drains `inbound.unregister_clients` -- for each, calls `registry.unregister(id)`.
4. Drains `inbound.client_player_assignments` -- for each, updates the client's `player_id`.

Add this system to the schedule after `drain_inbound_events` and before `process_client_inputs`.

**Tests:**
1. Unit test: push a registration event, run the system, verify the client appears in `ClientRegistry`.
2. Unit test: register then unregister, verify the client is removed.
3. Unit test: register a client then assign a player_id, verify `client.player_id` is set.

**Done when:**
- Client registration events flow from async network tasks into the ECS world.
- `ClientRegistry` is populated correctly each tick.

---

### WI-004: ACK parsing in handle_client (server)
**Size:** small (30-60 min)
**Depends on:** WI-002
**Files:**
- `crates/sim-server/src/network.rs`

**Description:**
Handle incoming binary frames from clients. Currently, binary frames from clients are silently ignored (only `Message::Text` is processed). Add a handler for `Message::Binary` that parses tick ACKs.

In the `handle_client` function's `tokio::select!` loop, add handling for the binary case in the WebSocket receive arm:

```rust
Some(Ok(Message::Binary(data))) => {
    if data.len() == 9 && data[0] == 0x03 {
        let tick = u64::from_le_bytes(data[1..9].try_into().unwrap());
        acked_tick.store(tick, Ordering::Relaxed);
    }
    // Ignore other binary messages from client
}
```

The `acked_tick` is the `Arc<AtomicU64>` shared between the `handle_client` task and the `ClientHandle` in the ECS registry. It is created in `handle_client` when the client connects and passed to the ECS via the registration event.

**Tests:**
1. Unit test: construct a 9-byte ACK buffer with known tick value. Parse it. Verify tick is correctly extracted.
2. Unit test: construct a binary message that is NOT 9 bytes or does not start with 0x03. Verify it is ignored (no panic, no state change).
3. Unit test: verify AtomicU64 is updated when a valid ACK is received.

**Done when:**
- Binary WebSocket frames from clients are parsed.
- Valid 0x03 frames update the client's `acked_tick`.
- Invalid or unexpected binary frames are silently ignored.
- No changes to JSON message handling.

---

### WI-005: ACK sending from client
**Size:** small (30-60 min)
**Depends on:** WI-001
**Files:**
- `client/src/network.ts`

**Description:**
After processing each binary frame (full or delta), send a 9-byte tick ACK back to the server.

In `NetworkClient`'s `onmessage` handler, after the frame has been applied to the WorldView and prediction reconciliation is complete:

```typescript
// Send tick ACK
if (this.ws && this.connected) {
    const ackBuf = new ArrayBuffer(9);
    const ackView = new DataView(ackBuf);
    ackView.setUint8(0, 0x03);
    ackView.setUint32(1, frame.tick & 0xFFFFFFFF, true);
    ackView.setUint32(5, Math.floor(frame.tick / 0x100000000), true);
    this.ws.send(ackBuf);
}
```

Place this code at the end of the binary frame processing block, after `diag.applied++` and after prediction reconciliation.

**Tests:**
1. Unit test: create an ACK buffer for tick 12345. Verify byte layout matches `[0x03, tick_lo_bytes..., tick_hi_bytes...]`.
2. Verify in integration test that the server logs ACK receipt (add temporary tracing::debug in the ACK parser from WI-004).

**Done when:**
- Client sends a binary ACK after every processed binary frame.
- ACK format is `[0x03:u8][tick:u64 LE]`, 9 bytes.
- ACKs are sent for both full and delta frames.

---

### WI-006: CompactEntityState and delta computation logic
**Size:** medium (2-3 hours)
**Depends on:** none (pure logic, can be done early)
**Files:**
- `crates/sim-server/src/delta.rs` (new file)
- `crates/sim-server/src/lib.rs` or `mod.rs` (add module)

**Description:**
Implement the delta computation as a standalone module with no ECS dependencies (pure functions operating on maps and vecs). This makes it thoroughly unit-testable.

**Types:**
```rust
/// Compact entity state for baseline comparison.
/// No EntityId field -- the baseline map is keyed by EntityId.
#[derive(Clone, Debug)]
pub struct CompactEntityState {
    pub position: Vec3,
    pub velocity: Vec3,
    pub entity_type: EntityType,
    pub authority_server: ServerId,
    pub last_input_seq: u32,
}

impl From<&EntityState> for CompactEntityState {
    fn from(e: &EntityState) -> Self { ... }
}

pub struct DeltaResult {
    pub updates: Vec<EntityState>,    // new or changed entities
    pub removals: Vec<EntityId>,       // entities in baseline but not in current
}
```

**Functions:**
```rust
/// Threshold for considering a float "changed" (avoids floating-point noise).
const CHANGE_THRESHOLD: f32 = 0.01;

/// Compare current entity set against a baseline. Returns the delta.
pub fn compute_delta(
    current: &[EntityState],
    baseline: &HashMap<EntityId, CompactEntityState>,
) -> DeltaResult {
    let mut updates = Vec::new();
    let mut current_ids = HashSet::with_capacity(current.len());

    for entity in current {
        current_ids.insert(entity.id);
        match baseline.get(&entity.id) {
            None => {
                // New entity
                updates.push(entity.clone());
            }
            Some(base) => {
                if entity_changed(entity, base) {
                    updates.push(entity.clone());
                }
            }
        }
    }

    let removals: Vec<EntityId> = baseline.keys()
        .filter(|id| !current_ids.contains(id))
        .copied()
        .collect();

    DeltaResult { updates, removals }
}

/// Build a baseline map from a full entity snapshot.
pub fn build_baseline(entities: &[EntityState]) -> HashMap<EntityId, CompactEntityState> {
    entities.iter()
        .map(|e| (e.id, CompactEntityState::from(e)))
        .collect()
}

fn entity_changed(current: &EntityState, baseline: &CompactEntityState) -> bool {
    vec3_changed(current.position, baseline.position)
        || vec3_changed(current.velocity, baseline.velocity)
        || current.entity_type != baseline.entity_type
        || current.authority_server != baseline.authority_server
        || current.last_input_seq != baseline.last_input_seq
}

fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    (a.x - b.x).abs() > CHANGE_THRESHOLD
        || (a.y - b.y).abs() > CHANGE_THRESHOLD
        || (a.z - b.z).abs() > CHANGE_THRESHOLD
}
```

**Tests:**
1. Empty baseline, 5 current entities: delta has 5 updates, 0 removals.
2. Identical baseline and current (5 entities, same positions): delta has 0 updates, 0 removals.
3. 5 entities baseline, 5 entities current, 2 moved: delta has 2 updates, 0 removals.
4. 5 entities baseline, 4 entities current (1 removed): delta has 0 updates, 1 removal.
5. 5 entities baseline, 6 entities current (1 added): delta has 1 update, 0 removals.
6. Entity moved by 0.005 (below threshold): not in delta. Moved by 0.02: in delta.
7. Only velocity changed: entity is in delta.
8. Only authority_server changed: entity is in delta.
9. Only last_input_seq changed: entity is in delta.
10. entity_type changed (Player -> Npc): entity is in delta.
11. `build_baseline` round-trips: build baseline from entities, compare same entities against it, get empty delta.

**Done when:**
- `delta.rs` module exists with `compute_delta`, `build_baseline`, and helper functions.
- All comparison logic is covered by unit tests.
- Module has no ECS dependencies -- operates on `EntityState` and `HashMap` only.

---

### WI-007: Integrate delta computation into prepare_client_snapshot
**Size:** large (3-4 hours)
**Depends on:** WI-002, WI-003, WI-004, WI-005, WI-006
**Files:**
- `crates/sim-server/src/systems.rs` (modify prepare_client_snapshot)
- `crates/sim-server/src/resources.rs` (add FULL_SNAPSHOT_INTERVAL constant)
- `crates/sim-server/src/encoding.rs` (no changes, but referenced)

**Description:**
Modify `prepare_client_snapshot` to produce per-client frames using delta compression.

The system signature changes to also take `ResMut<ClientRegistry>` and `Res<TickCounter>`.

**Algorithm per tick:**

1. Query all entities into `current_entities: Vec<EntityState>` (unchanged from today).
2. For each client in `ClientRegistry.clients`:
   a. Read `client.acked_tick` from the AtomicU64.
   b. If `client.acked_tick > 0` and the baseline's tick matches the acked tick, the baseline is confirmed. Otherwise, the baseline is stale (ACK not yet received for the pending baseline).
   c. Decide frame type:
      - If `client.baseline.is_none()` OR `client.ticks_since_full >= FULL_SNAPSHOT_INTERVAL`:
        - Encode full snapshot: `encode_binary_snapshot(tick, &current_entities)`
        - Set `client.baseline = Some(build_baseline(&current_entities))`
        - Set `client.ticks_since_full = 0`
      - Else:
        - Compute delta: `compute_delta(&current_entities, client.baseline.as_ref().unwrap())`
        - If delta has 0 updates and 0 removals, still send a minimal delta frame (so the client gets the tick number and can ACK it).
        - Encode delta: `encode_binary_delta(tick, &delta.updates, &delta.removals)`
        - Update baseline: merge changes into the existing baseline (apply updates, remove removals) rather than rebuilding from scratch.
   d. `client.ticks_since_full += 1`
   e. Send frame via `client.tx.try_send(frame)`. If channel full, log warning.

**Baseline update strategy:**

When a delta is sent, update the baseline incrementally:
```rust
for entity in &delta.updates {
    client.baseline.as_mut().unwrap()
        .insert(entity.id, CompactEntityState::from(entity));
}
for id in &delta.removals {
    client.baseline.as_mut().unwrap().remove(id);
}
```

This is O(changed entities) instead of O(all entities) for a full rebuild.

**Important: baseline tracks what the server SENT, not what the client ACK'd.**

The server updates its baseline immediately after sending a delta. It does not wait for the client to ACK. The ACK is used only to confirm that the client received the frame, enabling the server to avoid resending a full snapshot. If an ACK is never received, the `ticks_since_full` counter will eventually trigger a full snapshot, which resets everything.

This is simpler than tracking separate "sent" and "confirmed" baselines. The trade-off is that if a delta frame is lost (WebSocket dropped it), the server's baseline is ahead of the client's actual state. But the periodic full snapshot corrects this, and WebSocket frames are reliable (TCP underneath).

**Remove the ClientSnapshot resource** since frames are no longer written to a single shared resource. Instead, they are sent directly to per-client channels. Remove the `client_frame` extraction from the game loop and the corresponding Phase 5 code.

**Tests:**
1. Unit test: setup world with 3 entities and 1 client (in ClientRegistry). Run `prepare_client_snapshot`. Verify the client's channel has 1 frame. Verify it is a full snapshot (0x01) since there is no baseline yet.
2. Unit test: same setup, but after first run set `client.baseline` manually. Run again. Verify the frame is a delta (0x02).
3. Unit test: force full snapshot by setting `ticks_since_full = FULL_SNAPSHOT_INTERVAL`. Run. Verify full snapshot sent.
4. Unit test: 2 clients with different baselines. Verify they receive different frames.
5. Integration test: connect a client via WebSocket. Verify first frame is 0x01. Wait for several ticks. Verify subsequent frames are 0x02 (once ACK plumbing is working).
6. Run `./test.sh` -- all existing scenarios pass.

**Done when:**
- `prepare_client_snapshot` produces per-client frames.
- Clients with no baseline get full snapshots.
- Clients with a baseline get delta frames.
- Periodic full snapshots are sent every FULL_SNAPSHOT_INTERVAL ticks.
- Baselines are updated correctly after each sent frame.
- The `ClientSnapshot` resource and broadcast channel are removed.

---

### WI-008: Remove global lastSnapshotTime and update warmup logic
**Size:** small (30-60 min)
**Depends on:** WI-001, WI-007
**Files:**
- `client/src/world-view.ts`
- `client/src/network.ts` (verify diag still works)

**Description:**
Clean up residual global snapshot timing state from WorldView now that per-entity timestamps are in place.

- Remove `private lastSnapshotTime` field.
- Remove `private prevSnapshotTime` field if it is only used for `lastSnapshotTime` updates. Keep it if still needed for `measuredIntervalMs` computation.
- Update `measuredIntervalMs` computation to use the per-entity update times or a separate "last frame received" timer (for overall rate tracking, not interpolation).
- Update the warmup logic: during the first `WARMUP_SNAPSHOTS` frames, set `lastUpdateTime` to a value that causes `alpha >= 1.0` (e.g., `performance.now() - 1000`) so entities snap to position without interpolation. Or keep the `snapshotCount` guard in `interpolate` that skips interpolation during warmup.
- Verify `measuredHz` (the debug overlay network rate display) still works. It uses `countFrame()` and `lastHzTime` which are independent.
- Verify `applyDelta` no longer has the comment "Does NOT reset lastSnapshotTime" since the field no longer exists.

**Tests:**
1. Verify `interpolate` produces correct results with per-entity timestamps.
2. Verify warmup behavior: first 3 snapshots snap entities to position.
3. Verify `measuredHz` display still works in the debug overlay.

**Done when:**
- No global snapshot time fields remain.
- All interpolation is per-entity.
- Warmup still works.
- Debug overlay still shows correct Hz.

---

### WI-009: Bandwidth metrics and delta diagnostics
**Size:** small (30-60 min)
**Depends on:** WI-007
**Files:**
- `client/src/network.ts` (diag updates)
- `crates/sim-server/src/metrics_server.rs` (add delta stats)

**Description:**
Add observability for delta compression effectiveness.

**Client-side (network.ts diagnostics):**
- Track `bytesReceived` per frame in the diag object.
- Track `avgFullFrameSize` and `avgDeltaFrameSize`.
- In the periodic 5-second console log, add: `delta_ratio=X.X%` (percentage of frames that were deltas) and `avg_delta_size=NNN bytes` vs `avg_full_size=NNN bytes`.

**Server-side (metrics endpoint):**
- Add to the metrics output:
  - `delta_frames_sent: u64` (total delta frames sent across all clients)
  - `full_frames_sent: u64` (total full frames sent)
  - `avg_delta_entities: f32` (average number of updated entities per delta frame)
  - `connected_clients: usize` (number of registered clients)

**Tests:**
1. Verify diag object accumulates byte counts correctly.
2. Verify metrics endpoint includes new fields.

**Done when:**
- The `/metrics` endpoint shows delta compression effectiveness.
- The client console log shows bandwidth statistics.
- Operators can see at a glance whether delta compression is working and how much bandwidth it saves.

---

### WI-010: RequestSnapshot forced full sync
**Size:** small (30-60 min)
**Depends on:** WI-007
**Files:**
- `crates/sim-server/src/systems.rs` (handle RequestSnapshot in client input processing)
- `crates/sim-server/src/resources.rs` (add field to InboundQueue or ClientRegistry)

**Description:**
When the server receives a `ClientMessage::RequestSnapshot` from a client, force the next frame for that client to be a full snapshot by clearing its baseline.

Currently `ClientMessage::RequestSnapshot` is defined in the protocol but not handled by the server (no match arm for it in `process_client_inputs`). Add handling:

1. In `process_client_inputs`, when `RequestSnapshot` is received, push a "force full snapshot" marker for the requesting client.
2. In `prepare_client_snapshot`, check for this marker. If set, clear the client's baseline so the next frame is a full snapshot.

The challenge is identifying WHICH client sent the `RequestSnapshot`. Currently, all client inputs are pushed into the same `InboundQueue` with no client ID. This requires adding client ID tracking to client inputs.

**Approach:** When `handle_client` pushes a `ClientMessage` to the inbound queue, tag it with the client's ID. Add a wrapper:
```rust
pub struct TaggedClientInput {
    pub client_id: u64,
    pub message: ClientMessage,
}
```

Or simpler: add a separate `force_full_snapshot_clients: Vec<u64>` field to `InboundQueue`. When `RequestSnapshot` is received in `handle_client`, push the client ID there directly instead of going through the `ClientMessage` pipeline.

**Tests:**
1. Unit test: set `force_full_snapshot` for a client that has a baseline. Run `prepare_client_snapshot`. Verify it sends a full snapshot.
2. Unit test: verify baseline is rebuilt after the forced full snapshot.

**Done when:**
- `RequestSnapshot` triggers a full snapshot for the requesting client on the next tick.
- The client can recover from state corruption by sending `RequestSnapshot`.
- Other clients are unaffected (they still get deltas).

---

## Execution Order

```
Phase 1: Foundation (can be done in parallel)
  WI-001  Per-entity interpolation timestamps (client)     [no deps]
  WI-002  ClientRegistry and per-client channels (server)   [no deps]
  WI-006  Delta computation module (server, pure logic)     [no deps]

Phase 2: Plumbing
  WI-003  drain_client_events system                        [after WI-002]
  WI-004  ACK parsing in handle_client                      [after WI-002]
  WI-005  ACK sending from client                           [after WI-001]

Phase 3: Integration
  WI-007  Integrate delta into prepare_client_snapshot      [after WI-002..006]
  WI-008  Remove global lastSnapshotTime cleanup            [after WI-001, WI-007]

Phase 4: Polish
  WI-009  Bandwidth metrics and diagnostics                 [after WI-007]
  WI-010  RequestSnapshot forced full sync                  [after WI-007]
```

The critical path is: WI-002 -> WI-003 -> WI-007.
WI-001, WI-006 can be done first and independently.
WI-004, WI-005 are small and can be done alongside WI-003.
WI-008, WI-009, WI-010 are polish items after the core is working.

## Verification Milestones

**After Phase 1:** Client interpolation works with per-entity timestamps. Server sends identical frames through per-client channels. Delta computation logic passes all unit tests. No behavioral change visible to users.

**After Phase 2:** Client sends ACKs. Server parses ACKs and updates acked_tick. Registration/unregistration flows through ECS. Still full snapshots only.

**After Phase 3:** Delta compression is live. First frame is full snapshot, subsequent frames are deltas. Periodic full snapshots every 5 seconds. Bandwidth drops measurably on the UNO Q boards.

**After Phase 4:** Full observability, graceful recovery via RequestSnapshot, clean interpolation code.

## Key Constants

| Constant | Value | Location | Rationale |
|----------|-------|----------|-----------|
| `FULL_SNAPSHOT_INTERVAL` | 100 ticks (5s) | `resources.rs` | Recovery from lost state. 5s is the same as shadow stale timeout. |
| `CHANGE_THRESHOLD` | 0.01 | `delta.rs` | Avoids floating-point noise. 0.01 world units is sub-pixel at typical zoom. |
| `PER_CLIENT_CHANNEL_SIZE` | 8 | `network.rs` | Buffered frames per client. 8 frames = 400ms at 20Hz. |
| `ACK_FRAME_TYPE` | 0x03 | `network.rs`, `network.ts` | Distinct from 0x01 (full) and 0x02 (delta). |
