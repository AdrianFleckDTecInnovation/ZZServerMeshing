# ACK-UNUSED: Client tick ACKs are stored but never consulted for baseline decisions

## Severity: medium

## Category: protocol

## Summary
The client sends a binary ACK (0x03 frame with tick number) after every received frame, and the server stores it in an `AtomicU64`. However, the `prepare_client_snapshot` system never reads this value. The baseline advances optimistically on `try_send` (or even on drop -- see finding #018). The ACK mechanism is infrastructure without function, creating a false sense of reliability.

## Evidence
The client sends ACKs in `client/src/network.ts:164-170`:
```typescript
const ackBuf = new ArrayBuffer(9);
const ackView = new DataView(ackBuf);
ackView.setUint8(0, 0x03);
ackView.setUint32(1, frame.tick & 0xFFFFFFFF, true);
ackView.setUint32(5, Math.floor(frame.tick / 0x100000000), true);
this.ws.send(ackBuf);
```

The server stores the ACK in `crates/sim-server/src/network.rs:172-176`:
```rust
if data.len() == 9 && data[0] == 0x03 {
    let tick = u64::from_le_bytes(data[1..9].try_into().unwrap_or([0; 8]));
    acked_tick.store(tick, Ordering::Relaxed);
}
```

The `acked_tick` field on `ClientHandle` has `#[allow(dead_code)]` (resources.rs:167) with the comment "Currently used for diagnostics; future: conditional baseline confirmation." But it is not read anywhere in `prepare_client_snapshot` or any other system.

The `prepare_client_snapshot` system at `systems.rs:830-831` makes the full/delta decision based solely on `handle.baseline.is_none()` and `handle.ticks_since_full >= FULL_SNAPSHOT_INTERVAL`. The ACK tick is ignored.

## Impact
**Functional gap:** The server has no way to know whether the client actually received and processed a frame. The baseline advances purely on send-side success (or even on send failure, per #018). This means:

1. If a WebSocket frame is silently dropped by the network layer (browser tab background throttling, network hiccup after `try_send` succeeds), the baseline diverges permanently until the next periodic full snapshot.

2. The 5-second full snapshot interval is the sole recovery mechanism. There is no faster convergence path triggered by ACK absence.

3. The ACK infrastructure (9 bytes per frame from every client, atomic writes, Arc sharing) is pure overhead since nothing reads the value.

**Not a bug currently** -- WebSocket over TCP provides reliable delivery, so frames that pass `try_send` will eventually reach the client. But this assumption breaks if:
- WebSocket buffering causes the connection to be silently closed by the browser
- The client processes frames but fails to apply them (JS exception)
- Future transport changes (WebRTC data channels) remove TCP reliability

## Recommended Fix
Two options:

**Option A -- Remove dead code (if ACK-based confirmation is not planned soon):**
Remove the `acked_tick` field, the `AtomicU64` Arc, and the client-side ACK sending. This simplifies the code and removes the false safety net.

**Option B -- Wire ACKs into baseline management (recommended):**
Use the ACK to confirm baseline state. Track which tick's baseline was sent with each frame. Only advance the baseline when the ACK confirms receipt:

```rust
pub struct ClientHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub player_id: Option<EntityId>,
    pub acked_tick: Arc<AtomicU64>,
    pub baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub pending_baseline: Option<HashMap<EntityId, CompactEntityState>>,
    pub pending_tick: u64,
    pub ticks_since_full: u32,
}
```

On send: compute delta against `baseline`, store updated baseline in `pending_baseline`.
On ACK received (matching `pending_tick`): promote `pending_baseline` to `baseline`.
On ACK timeout (>10 ticks without ACK): force full snapshot.

This provides true end-to-end reliability. It is a medium effort change but significantly improves correctness.

## Effort: small (option A) / medium (option B)

## Related Files
- crates/sim-server/src/resources.rs:166-167 (acked_tick field, dead_code allow)
- crates/sim-server/src/network.rs:93 (AtomicU64 creation)
- crates/sim-server/src/network.rs:172-176 (ACK parsing and store)
- crates/sim-server/src/systems.rs:830-831 (full/delta decision ignores ACK)
- client/src/network.ts:164-170 (client ACK sending)
