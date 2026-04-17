# 002: Delta Compression for Server-to-Client Snapshots

## Problem Statement

Every tick (20Hz), each sim-server encodes ALL entities (authoritative + shadows) into a binary frame and broadcasts it identically to every connected client. With N entities at 47 bytes each plus a 27-byte header:

| Entities | Frame size | Per-client bandwidth (20Hz) |
|----------|-----------|---------------------------|
| 100      | 4.7 KB    | 94 KB/s                   |
| 300      | 14.1 KB   | 282 KB/s                  |
| 500      | 23.5 KB   | 470 KB/s                  |
| 1000     | 47.0 KB   | 940 KB/s                  |

On the UNO Q boards sharing a WiFi network, 3 servers each sending 23 KB to 2 clients = ~280 KB/s of airtime. This leaves little headroom for the UDP peer replication traffic and grows linearly with entity count.

In practice, most entities are stationary or moving predictably. On any given tick, only ~10-20% of entities change meaningfully. Sending the other 80-90% is wasted bandwidth.

### Previous Attempt -- Why It Failed

Delta compression was attempted before and broke client interpolation. The root cause was the global `lastSnapshotTime` in `world-view.ts`. When a delta frame arrived containing only 5 changed entities out of 300, `lastSnapshotTime` was reset for ALL entities. The 295 unchanged entities had their interpolation alpha reset to 0, snapping them back to `previousPosition` and causing visible warping/jitter.

The fix: per-entity interpolation timestamps. Each entity tracks its own "last update time" so that receiving a delta frame only resets interpolation for the entities that actually changed.

## Design

### Architecture Overview

```
Game Loop (tick N)
  |
  v
prepare_client_snapshot system
  |
  +-- Query all auth + shadow entities -> Vec<EntityState>
  |
  +-- For each connected client:
  |     |
  |     +-- Have ACK'd baseline? Compare with current.
  |     |     Yes: Compute delta (new/changed/removed). Encode 0x02 frame.
  |     |     No:  Encode full 0x01 frame.
  |     |
  |     +-- Every FULL_SNAPSHOT_INTERVAL ticks: force full 0x01 frame.
  |     |
  |     +-- Store current state as "pending baseline" for this client.
  |
  v
Per-client frame channel (replaces broadcast)
  |
  v
handle_client task: send frame, receive ACK
  |
  v
Client receives frame, applies to WorldView, sends ACK(tick) back
```

### Key Design Decisions

**1. Per-client channels replace the broadcast channel.**

The current architecture uses a single `broadcast::Sender<Vec<u8>>` in the game loop. Every client receives the identical frame. Delta compression requires per-client frames (each client has a different baseline). The broadcast channel must be replaced with per-client `mpsc` channels.

A `ClientRegistry` resource holds a map of `client_id -> mpsc::Sender<Vec<u8>>`. The game loop (or a new system) iterates over clients, computes each client's frame, and sends it through the per-client channel. When a client connects, its channel is registered; when it disconnects, it is removed.

**2. ACK is a lightweight binary message: 9 bytes.**

Format: `[0x03:u8][tick:u64 LE]` -- a single byte frame type plus the tick number. The client sends this as a binary WebSocket frame after processing each snapshot. The server parses it in the `handle_client` task and writes the ACK'd tick into shared state (an `AtomicU64` or a channel).

**3. Baseline tracking is tick-based, not per-entity hash.**

The server stores, for each client, the full entity snapshot at the last ACK'd tick as a `HashMap<EntityId, EntityState>`. When computing a delta, it compares each current entity against this map. This avoids per-field hashing and is simple to reason about.

**4. Change detection uses position + velocity + authority comparison.**

An entity is "changed" if any of: position differs by > 0.01 units on any axis, velocity differs by > 0.01 units/s, authority_server changed, entity_type changed, or last_input_seq changed. This avoids sending entities that moved by floating-point noise.

**5. Periodic full snapshots as a safety net.**

Every `FULL_SNAPSHOT_INTERVAL` ticks (default: 100, i.e., every 5 seconds), the server sends a full snapshot regardless of deltas. This handles lost ACKs, client state corruption, and provides a guaranteed recovery point. The interval is chosen to balance bandwidth savings against recovery latency.

**6. Per-entity interpolation timestamps on the client.**

Each `EntityData` gains a `lastUpdateTime: number` field. When a delta frame arrives, only entities present in the delta's update list get their `lastUpdateTime` reset. Unchanged entities continue interpolating with their existing timer. Full snapshots reset all entities.

### Data Flows

**Server -> Client (delta frame, type 0x02):**
```
[type:u8=0x02][tick:u64 LE][update_count:u32 LE][removal_count:u32 LE][metrics:14 bytes]
[updated entities x N: 47 bytes each]
[removed UUIDs x M: 16 bytes each]
```

This format already exists in `encoding.rs` and `binary-protocol.ts`. No wire format changes needed.

**Client -> Server (tick ACK, type 0x03):**
```
[type:u8=0x03][tick:u64 LE]
```

9 bytes, sent as a binary WebSocket frame. New message type. The `0x03` frame type does not conflict with the existing `0x01` (full) and `0x02` (delta) types because those are server-to-client only.

**Server-internal (per-client state):**
```rust
struct ClientState {
    client_id: u64,
    player_id: Option<EntityId>,
    tx: mpsc::Sender<Vec<u8>>,
    acked_tick: Arc<AtomicU64>,
    baseline: Option<HashMap<EntityId, EntityState>>,
    ticks_since_full: u32,
}
```

## Protocol Changes

### New Binary Frame Type: Tick ACK (0x03)

**Direction:** Client -> Server
**Format:** `[0x03:u8][tick:u64 LE]` (9 bytes total)
**Semantics:** "I have successfully processed the snapshot for tick T. You may use tick T's state as my baseline for future deltas."

This is a binary WebSocket frame, not JSON. The `handle_client` function already distinguishes binary vs text frames -- binary frames from the client are currently ignored. This adds handling for incoming binary frames.

### No Changes to Existing Frame Types

The full snapshot (0x01) and delta (0x02) wire formats are unchanged. The `encode_binary_snapshot` and `encode_binary_delta` functions in `encoding.rs` remain as-is. The `decodeBinaryFrame` function in `binary-protocol.ts` already handles both frame types.

### No Changes to ClientMessage Enum

The ACK is a raw binary frame, not a JSON `ClientMessage` variant. This keeps the hot-path ACK processing out of the JSON parsing pipeline and avoids modifying the protocol enum (which would require handling in multiple match arms).

## Server-Side Implementation

### New Resource: ClientRegistry

```rust
// resources.rs
#[derive(Resource, Default)]
pub struct ClientRegistry {
    pub clients: HashMap<u64, ClientHandle>,
    pub next_id: u64,
}

pub struct ClientHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub player_id: Option<EntityId>,
    pub acked_tick: Arc<AtomicU64>,
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub ticks_since_full: u32,
}
```

`CompactEntityState` is a lightweight struct for baseline comparison:

```rust
pub struct CompactEntityState {
    pub position: Vec3,
    pub velocity: Vec3,
    pub entity_type: EntityType,
    pub authority_server: ServerId,
    pub last_input_seq: u32,
}
```

This is 33 bytes vs ~60 bytes for a full `EntityState` (no UUID needed in the baseline map since it is keyed by `EntityId`).

### Modified System: prepare_client_snapshot

Currently builds a single `Vec<u8>` frame. Modified to:

1. Query all entities into `Vec<EntityState>` (same as today).
2. For each client in `ClientRegistry`:
   a. Check `ticks_since_full >= FULL_SNAPSHOT_INTERVAL` or `baseline.is_none()` or `acked_tick == 0`. If so, encode full snapshot, update baseline, reset counter.
   b. Otherwise, compare current entities against baseline. Compute updates/removals. Encode delta frame. Store current state as pending baseline.
   c. Send the encoded frame through the client's `tx` channel.

### Modified handle_client Function (network.rs)

- On connect: register client in `ClientRegistry` via a `RegisterClient` event pushed to `SharedInbound`.
- Replace `broadcast_rx.recv()` with `client_rx.recv()` from a per-client `mpsc::Receiver`.
- Parse incoming binary frames: if byte 0 is `0x03` and length is 9, extract tick and write to `acked_tick`.
- On disconnect: push `UnregisterClient` event.

### Modified Game Loop (game_loop.rs)

- Remove the `broadcast_tx` parameter entirely.
- Phase 5 (broadcast client frame) becomes: iterate `ClientRegistry`, extract per-client frames from the system output (or have the system write directly to the client channels).

### Performance Cost Analysis

**Per-tick overhead of delta computation (500 entities, 2 clients):**

For each client:
- HashMap lookup per entity: ~500 lookups, O(1) each. ~5-10 us.
- Vec3 comparison per entity: 3 float subtractions + absolute value check. ~500 * ~5ns = ~2.5 us.
- Encoding changed entities: ~50 entities * 47 bytes = ~2.3 KB. ~5 us.
- HashMap clone for new baseline: ~500 entries * 33 bytes = ~16 KB. ~10 us.

Total per client: ~20-30 us. For 2 clients: ~40-60 us.

This is within the 50ms tick budget. The savings in encoding (not writing 450 unchanged entities * 47 bytes = 21 KB per client) more than compensate.

**Memory overhead:**
- Per client baseline map: 500 entities * (16 byte key + 33 byte value + ~16 byte HashMap overhead) = ~32 KB per client.
- For 10 clients: ~320 KB. Well within the 4GB on the UNO Q boards.

## Client-Side Implementation

### Per-Entity Interpolation Timestamps

`EntityData` gains:
```typescript
lastUpdateTime: number;   // performance.now() when this entity was last updated by the server
```

**In `applySnapshot` (full frame):** All entities get `lastUpdateTime = now`.

**In `applyDelta` (delta frame):** Only entities in the `updates` array get `lastUpdateTime = now`. Entities not in the delta keep their existing `lastUpdateTime`.

**In `interpolate`:** Each entity computes its own alpha:
```typescript
const timeSinceUpdate = now - entity.lastUpdateTime;
const alpha = Math.min(timeSinceUpdate / this.measuredIntervalMs, 1.2);
```

The global `lastSnapshotTime` is removed. The `measuredIntervalMs` remains global since it tracks the overall network rate (useful for all entities).

### Tick ACK Sending

After processing any binary frame (full or delta), the client sends a 9-byte binary ACK:
```typescript
const ackBuf = new ArrayBuffer(9);
const ackView = new DataView(ackBuf);
ackView.setUint8(0, 0x03);
// Write tick as u64 LE
ackView.setUint32(1, tick & 0xFFFFFFFF, true);
ackView.setUint32(5, Math.floor(tick / 0x100000000), true);
ws.send(ackBuf);
```

This is called from the `onmessage` handler in `network.ts` after applying the frame.

### No Changes to Binary Protocol Decoder

The `decodeBinaryFrame` function in `binary-protocol.ts` already handles both 0x01 and 0x02 frame types correctly. No modifications needed.

## Failure Modes

### Lost ACK (client -> server)

The client sends ACK(tick=100) but it is lost. The server never updates the baseline. On the next tick, the server computes deltas against the stale baseline (e.g., tick=50). The delta will be larger than necessary (includes changes from tick 50 to 101 instead of 100 to 101) but functionally correct. The periodic full snapshot at tick 150 resets everything.

**Impact:** Slightly larger deltas until the next ACK succeeds. No data loss. No visual artifacts.

### Lost Delta Frame (server -> client)

The server sends delta(tick=101) but the WebSocket frame is lost. The client's state is now stale -- it has tick=100's entities but thinks it is up to date. On the next frame (tick=102), the delta is computed against the client's ACK'd baseline (tick=100), so it includes all changes from 100 to 102. The client skips tick 101 but applies 102 correctly.

**Impact:** One tick of staleness, barely noticeable at 20Hz (50ms). Interpolation covers the gap.

### Client Reconnect / State Corruption

If the client detects inconsistency (e.g., entity references that do not exist, or wildly wrong positions), it can send a `RequestSnapshot` message (already exists in the `ClientMessage` enum). The server responds with a forced full snapshot on the next tick.

**Impact:** One tick of recovery. The `RequestSnapshot` path already exists but is unused.

### Server Crash During Delta

The client's connection closes. It reconnects to a new server (via coordinator `/assign`). The new server has no baseline for this client and sends a full snapshot. Clean recovery.

### High Client Count (many baselines)

Each client baseline is ~32 KB (500 entities). 100 clients = 3.2 MB. On a 4 GB UNO Q board, this is fine. At 1000 clients per server we would reconsider, but that is far beyond current scale.

### Client Cannot Keep Up (ACK lag)

If the client is slow and its ACK falls behind by many ticks, the delta grows to include all changes since the baseline -- potentially as large as a full snapshot. The `ticks_since_full` counter will eventually force a full snapshot anyway, which resets the baseline. No unbounded growth.

## Testing Plan

### Unit Tests (Rust)

1. **Delta computation produces correct updates/removals.** Create a baseline with 5 entities. Change 2, remove 1, add 1. Verify delta output.
2. **Unchanged entities produce empty delta.** Same baseline and current. Delta should have 0 updates, 0 removals.
3. **Change detection threshold.** Entity moved by 0.001 units (below threshold) -- no update. Entity moved by 0.02 units -- update included.
4. **Full snapshot interval.** After FULL_SNAPSHOT_INTERVAL ticks without a forced full, the next frame is a full snapshot.
5. **Baseline update on ACK.** Verify baseline is not updated until ACK is received. Pre-ACK deltas use the old baseline.
6. **Per-client frame encoding.** Two clients with different baselines get different frames.

### Unit Tests (TypeScript)

1. **Per-entity interpolation.** Apply delta with 2 of 5 entities. Verify only the 2 updated entities have reset `lastUpdateTime`. Other 3 entities continue interpolating from their previous positions.
2. **Full snapshot resets all timestamps.** After full snapshot, all entities have the same `lastUpdateTime`.
3. **ACK encoding.** Verify 9-byte binary ACK format matches expected layout.
4. **Mixed frame types.** Sequence: full -> delta -> delta -> full. Verify entity state is consistent throughout.

### Integration Tests (Scene Tests)

1. **Bandwidth reduction.** Spawn 200 stationary NPCs. After initial full snapshot, subsequent frames should be delta type with 0 updates (or very few from NPC random movement). Measure total bytes received over 10 seconds. Compare against full-snapshot baseline.
2. **No visual artifacts.** Spawn 50 NPCs, move 5 continuously. Verify no position jumps, no flicker, no entity disappearances. Run for 30 seconds.
3. **Client reconnect.** Disconnect and reconnect. Verify first frame after reconnect is a full snapshot. Verify subsequent frames are deltas.
4. **ACK loss simulation.** Introduce artificial ACK drop (e.g., drop every other ACK). Verify deltas grow larger but remain functionally correct. Verify periodic full snapshots recover baseline.

### UNO Q Board Tests

1. **Real WiFi bandwidth.** Deploy to boards. Spawn 300 entities. Monitor actual bandwidth per client with tcpdump. Compare against pre-delta baseline.
2. **Tick budget impact.** Compare tick phase timings (from /metrics endpoint) before and after delta compression. Delta computation should add < 5ms to the tick.
3. **Multi-client delta independence.** Two clients on different servers. Each sees proper deltas from its own server. Verify frame types and sizes.

## Migration

### Backward Compatibility

The 0x01 and 0x02 frame types are already defined and handled by the client. Old clients that do not send ACKs will never have a baseline, so the server will always send full snapshots to them. This is identical to the current behavior. No breaking change.

### Rollout Strategy

1. Implement per-entity interpolation timestamps (client-only change). Deploy. Verify no regressions. This is safe and improves interpolation quality even without deltas.
2. Implement per-client channels and ClientRegistry (server-side plumbing). Deploy. Still sending full snapshots only -- just through per-client channels instead of broadcast. Verify no regressions.
3. Implement ACK parsing and baseline tracking. Deploy. Still sending full snapshots but now tracking baselines. Verify ACKs are received and baselines are built.
4. Enable delta computation. Deploy. Now sending deltas. Monitor bandwidth and tick budget on boards. Verify no artifacts.
5. Tune change detection threshold and full snapshot interval based on real-world data.

Each step is independently deployable and reversible.
