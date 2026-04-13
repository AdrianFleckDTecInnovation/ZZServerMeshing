# FULL-SNAPSHOT-CLONE: Full snapshot binary frame cloned N times for N clients

## Severity: medium

## Category: bottleneck

## Summary
The `prepare_client_snapshot` system encodes the full snapshot frame once, then clones the entire `Vec<u8>` for each client that needs a full snapshot. Since the full frame is identical for all recipients, this is unnecessary allocation and copying. On the periodic full snapshot tick (every 100 ticks), ALL clients receive a full snapshot, causing N identical clones of a potentially large buffer.

## Evidence
In `crates/sim-server/src/systems.rs:825-837`:
```rust
let full_frame = encode_binary_snapshot(tick.0, &entities);
snapshot.frame = Some(full_frame.clone()); // clone 1: backward compat resource

for (_client_id, handle) in registry.clients.iter_mut() {
    let frame = if send_full {
        // ...
        full_frame.clone() // clone 2..N+1: per-client
    } else {
        // delta path
    };
    handle.tx.try_send(frame); // sends owned Vec<u8>
}
```

Full snapshot size = 27 + E * 47 bytes. At 500 entities: 27 + 23,500 = 23,527 bytes.

On the periodic full snapshot tick with 10 clients: 10 clones of 23KB = 230KB of allocations in a single tick. At 20 clients: 460KB. This is pure memcpy work inside the 50ms budget.

Additionally, the `encode_binary_snapshot` itself allocates a `Vec` with `Vec::with_capacity`, which is the first allocation. Then the backward-compat `clone` is the second. Then each client clone is the third through N+2th.

## Impact
At current scale (1-3 clients, 60 entities, ~2.8KB frames), this is negligible. At target scale:

| Clients | Entities | Frame Size | Clones | Total memcpy |
|---------|----------|-----------|--------|-------------|
| 3       | 60       | 2.8 KB    | 4      | 11.3 KB     |
| 10      | 500      | 23.5 KB   | 11     | 258 KB      |
| 20      | 1000     | 47 KB     | 21     | 987 KB      |
| 50      | 2000     | 94 KB     | 51     | 4.8 MB      |

4.8MB of memcpy in one tick is significant on UNO Q hardware.

## Recommended Fix
Use `Arc<Vec<u8>>` or `Bytes` (from the `bytes` crate) for zero-copy sharing of the full snapshot frame:

```rust
use std::sync::Arc;

let full_frame = Arc::new(encode_binary_snapshot(tick.0, &entities));
snapshot.frame = Some((*full_frame).clone()); // backward compat (single clone)

// Change channel type from Sender<Vec<u8>> to Sender<Arc<Vec<u8>>>
// or use bytes::Bytes which is already reference-counted
```

This requires changing the `mpsc::Sender<Vec<u8>>` channel type to `mpsc::Sender<Arc<Vec<u8>>>` (or `bytes::Bytes`), and updating the WebSocket send path in `handle_client` to accept the shared type. The `tokio_tungstenite` `Message::Binary` accepts `Vec<u8>`, so you would need `Arc::try_unwrap` or `Bytes::into()` at the send point.

Alternatively, since delta frames are per-client (different content), the channel could accept an enum:
```rust
enum ClientFrame {
    Shared(Arc<Vec<u8>>),  // full snapshots
    Owned(Vec<u8>),        // delta frames
}
```

## Effort: small

## Related Files
- crates/sim-server/src/systems.rs:825-837 (full_frame.clone() per client)
- crates/sim-server/src/resources.rs:51 (channel type: Sender<Vec<u8>>)
- crates/sim-server/src/network.rs:138-142 (handle_client frame receiver)
