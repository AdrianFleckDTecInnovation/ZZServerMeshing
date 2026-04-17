# 003: Critical/High Issue Fixes -- Work Items

**Design:** `designs/003-critical-high-fixes.md`
**Total work items:** 14
**Estimated total effort:** 18-27 hours

Work items are grouped by issue and ordered by dependency. Within each issue, items are sequential unless stated otherwise.

---

## Issue 1: Two-Phase Authority Transfer (Critical)

### WI-001: Add TransferAck to PeerMessage and InboundQueue

**Size:** small
**Depends on:** none
**Files:**
- `crates/shared/src/protocol.rs`
- `crates/sim-server/src/resources.rs`

**Description:**
Add a new `PeerMessage::TransferAck { entity_id: EntityId, tick: u64 }` variant to the `PeerMessage` enum in `protocol.rs`.

Add a new `InboundTransferAck` struct and a `transfer_acks: Vec<InboundTransferAck>` field to `InboundQueue` in `resources.rs`:

```rust
pub struct InboundTransferAck {
    pub entity_id: EntityId,
    pub tick: u64,
}
```

Update the `InboundQueue::default()` derivation -- the new Vec will be empty by default via `#[derive(Default)]`, so no manual change is needed beyond adding the field.

**Tests:**
- Unit test in `protocol.rs`: `TransferAck` bincode roundtrip (serialize + deserialize, verify fields).
- Unit test in `resources.rs`: `InboundQueue` default has empty `transfer_acks`.

**Done when:** `cargo test -p shared` and `cargo test -p sim-server` pass. The new variant serializes and deserializes correctly.

---

### WI-002: Add TransferOutbound component

**Size:** small
**Depends on:** none
**Files:**
- `crates/sim-server/src/components.rs`

**Description:**
Add a new ECS component:

```rust
#[derive(Component, Debug)]
pub struct TransferOutbound {
    pub target_server: u16,
    pub target_addr: std::net::SocketAddr,
    pub sent_tick: u64,
    pub retry_count: u8,
    pub serialized_msg: Vec<u8>,
}
```

Add two constants at the top of `systems.rs` (or in a new `constants` section near the existing `SHADOW_STALE_TICKS`):

```rust
const TRANSFER_TIMEOUT_TICKS: u64 = 10;  // 500ms at 20Hz
const TRANSFER_MAX_RETRIES: u8 = 1;
```

**Tests:**
- Unit test: spawn an entity with `TransferOutbound` component alongside `Authoritative` and `NetworkId`. Verify the component can be queried and its fields read back.

**Done when:** `cargo test -p sim-server` passes. Component can be added to and queried from ECS entities.

---

### WI-003: Handle TransferAck in UDP receiver

**Size:** small
**Depends on:** WI-001
**Files:**
- `crates/sim-server/src/receiver.rs`
- `crates/sim-server/src/game_loop.rs` (inbound swap section, lines 91-116)

**Description:**
In `receiver.rs`, add a match arm for `PeerMessage::TransferAck { entity_id, tick }` that pushes an `InboundTransferAck` to the `inbound.queue.transfer_acks` Vec.

In `game_loop.rs`, in the Phase 1 inbound swap (lines 91-116), add `transfer_acks` to the list of fields swapped from the async `SharedInbound` into the ECS `InboundQueue`. Follow the same pattern as the other fields: `let transfer_acks = std::mem::take(&mut iq.transfer_acks);` then assign to `ecs_queue.transfer_acks`.

**Tests:**
- No new unit tests needed (receiver is an async loop, tested via integration/scene tests).
- Verify `cargo build` succeeds with no warnings about unmatched enum arms.

**Done when:** `cargo build -p sim-server` compiles cleanly. The new `TransferAck` variant is handled in the receiver and swapped into the ECS world each tick.

---

### WI-004: Send TransferAck from transfer receiver

**Size:** medium
**Depends on:** WI-001, WI-003
**Files:**
- `crates/sim-server/src/systems.rs` (`drain_inbound_events` function, authority transfer processing section, lines 82-148)
- `crates/sim-server/src/resources.rs`

**Description:**
After successfully accepting an authority transfer (both the "promoted shadow" and "new entity" paths in `drain_inbound_events`), send a `TransferAck` back to the source server.

To do this, the system needs access to the source server's UDP address. Look up `transfer.from_server` in `PeerAddresses`. If found, serialize `PeerMessage::TransferAck { entity_id: transfer.entity_id, tick: current_tick }` and push to `OutboundTransfers.messages`.

If the source server is not in `PeerAddresses` (source may have crashed), skip the ACK -- the source's timeout will handle it.

Also handle duplicate transfers: if the entity already exists as authoritative on the target (the "Ignoring duplicate transfer" path at line 96-103), STILL send a `TransferAck`. The source needs the ACK even if this is a retry and the target already has the entity.

**Tests:**
- Unit test: set up a world with server_id=2, add server 1 to PeerAddresses. Push an `InboundTransfer` (from_server=1, to_server=2) into the InboundQueue. Run `drain_inbound_events`. Verify `OutboundTransfers.messages` contains one entry with the correct target address and a deserializable `PeerMessage::TransferAck`.
- Unit test: same setup but from_server=3 is NOT in PeerAddresses. Verify no ACK is pushed (graceful skip).
- Unit test: send a duplicate transfer for an entity that is already authoritative. Verify the ACK is still sent.

**Done when:** `cargo test -p sim-server` passes. Accepted transfers generate ACK messages in `OutboundTransfers`.

---

### WI-005: Implement process_transfer_acks system

**Size:** medium
**Depends on:** WI-001, WI-002, WI-004
**Files:**
- `crates/sim-server/src/systems.rs` (new system function)
- `crates/sim-server/src/schedule.rs` (add to schedule)

**Description:**
Add a new system `process_transfer_acks` that runs after `drain_inbound_events` and before `drain_client_events`.

The system:
1. Takes all entries from `InboundQueue.transfer_acks` using `std::mem::take`.
2. For each ACK, looks up the entity in `EntityLookup`.
3. If the entity exists and has a `TransferOutbound` component, calls `despawn_and_remove`.
4. If the entity does not exist or does not have `TransferOutbound`, logs a debug message and ignores.

Update `schedule.rs` to add the new system:
```rust
systems::process_transfer_acks.after(systems::drain_inbound_events),
systems::drain_client_events.after(systems::process_transfer_acks),
```

**Tests:**
- Unit test: set up a world, spawn an entity with `Authoritative` + `TransferOutbound` + `NetworkId`. Push a matching `InboundTransferAck` into the queue. Run `process_transfer_acks`. Verify the entity is despawned and removed from `EntityLookup`.
- Unit test: push an ACK for a nonexistent entity. Verify no panic.
- Unit test: push an ACK for an entity WITHOUT `TransferOutbound` (e.g., a normal authoritative entity). Verify the entity is NOT despawned.
- Schedule test: verify the updated schedule builds and runs without panic.

**Done when:** `cargo test -p sim-server` passes. ACK'd transfers are cleaned up correctly.

---

### WI-006: Implement process_transfer_timeouts system

**Size:** medium
**Depends on:** WI-002, WI-005
**Files:**
- `crates/sim-server/src/systems.rs` (new system function)
- `crates/sim-server/src/schedule.rs`

**Description:**
Add a new system `process_transfer_timeouts` that runs after `process_transfer_acks` and before `drain_client_events`.

The system:
1. Reads the current tick from `TickCounter`.
2. Queries all entities with `TransferOutbound` and `NetworkId`.
3. For each, checks if `current_tick - sent_tick > TRANSFER_TIMEOUT_TICKS` (10 ticks = 500ms).
4. If timed out and `retry_count < TRANSFER_MAX_RETRIES` (1):
   - Push a copy of `serialized_msg` to `OutboundTransfers.messages` with the stored `target_addr`.
   - Increment `retry_count`.
   - Set `sent_tick = current_tick`.
   - Log at warn level: "Retrying transfer for entity {eid} to S{target} (attempt {retry_count})".
5. If timed out and `retry_count >= TRANSFER_MAX_RETRIES`:
   - Remove the `TransferOutbound` component. The entity keeps `Authoritative` -- it stays on this server.
   - Log at error level: "Transfer for entity {eid} to S{target} failed after {retry_count} retries, keeping entity".

Update `schedule.rs`:
```rust
systems::process_transfer_timeouts.after(systems::process_transfer_acks),
systems::drain_client_events.after(systems::process_transfer_timeouts),
```

**Tests:**
- Unit test: spawn entity with `TransferOutbound { sent_tick: 1, retry_count: 0 }`. Set tick to 12 (past timeout). Run system. Verify `OutboundTransfers` has one entry and the component's `retry_count` is now 1.
- Unit test: spawn entity with `TransferOutbound { sent_tick: 1, retry_count: 1 }`. Set tick to 22 (past second timeout). Run system. Verify `TransferOutbound` is removed, entity still has `Authoritative`.
- Unit test: spawn entity with `TransferOutbound { sent_tick: 5 }`. Set tick to 10 (NOT past timeout). Run system. Verify no change.

**Done when:** `cargo test -p sim-server` passes. Timed-out transfers retry once then abort.

---

### WI-007: Modify process_transfers for two-phase

**Size:** medium
**Depends on:** WI-002, WI-005, WI-006
**Files:**
- `crates/sim-server/src/systems.rs` (`process_transfers` function, lines 585-658)

**Description:**
Modify `process_transfers` to use the two-phase protocol instead of fire-and-forget.

Current code (lines 640-649):
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes));
    transferred += 1;
}
despawn_and_remove(world, *entity, *eid);
```

New code:
```rust
if let Ok(bytes) = bincode::serialize(&msg) {
    world.resource_mut::<OutboundTransfers>().messages.push((target_addr, bytes.clone()));
    // Add TransferOutbound marker instead of despawning
    world.entity_mut(*entity).insert(TransferOutbound {
        target_server,
        target_addr,
        sent_tick: tick,
        retry_count: 0,
        serialized_msg: bytes,
    });
    transferred += 1;
}
// DO NOT call despawn_and_remove here
```

The entity keeps its `Authoritative` component. It will be despawned when `process_transfer_acks` receives the ACK.

Note: entities with `TransferOutbound` should be excluded from future transfer selections. Update the NPC candidate collection loop (line 608-613) to skip entities that already have `TransferOutbound`:

```rust
if kind.0 == EntityType::Npc && world.get::<TransferOutbound>(entity).is_none() {
    npc_candidates.push((entity, nid.0, pos.0.length_squared()));
}
```

**Tests:**
- Unit test: set up world with 10 NPCs and a peer address for server 2. Set `PendingTransfers` to target server 2. Run `process_transfers`. Verify transferred entities have `TransferOutbound` component and are NOT despawned. Verify they are still in `EntityLookup`.
- Unit test: verify transferred entities still appear in replication output (they still have `Authoritative`).
- Schedule test: full tick cycle with transfer -- entity should remain until ACK.

**Done when:** `cargo test -p sim-server` passes. Transfers add outbound markers instead of despawning. Existing `transferred_entity_not_in_replication` test will need updating (entities now remain, so replication count stays the same until ACK).

---

### WI-008: Modify process_drain for two-phase

**Size:** medium
**Depends on:** WI-007
**Files:**
- `crates/sim-server/src/systems.rs` (`process_drain` function, lines 665-734)

**Description:**
Apply the same two-phase treatment to `process_drain`. Replace `despawn_and_remove` with `TransferOutbound` insertion, same as WI-007.

The drain completion check (lines 730-733) must be updated. Currently it checks if no authoritative entities remain. With two-phase transfers, it should check if no authoritative entities remain that do NOT have `TransferOutbound`:

```rust
// Check if all non-transfer entities are gone
let remaining = world
    .query_filtered::<(), (With<Authoritative>, Without<TransferOutbound>)>()
    .iter(&world)
    .count();
let pending_transfers = world
    .query_filtered::<(), With<TransferOutbound>>()
    .iter(&world)
    .count();
if remaining == 0 && pending_transfers == 0 {
    world.resource_mut::<ShutdownRequested>().0 = true;
}
```

The drain only completes when ALL entities have been ACK'd (or timed out and aborted). If some transfers fail (target unreachable), the entities are kept and the drain stalls. This is the safe behavior -- a drain that cannot complete should not shutdown.

Add a drain timeout: if the drain has been active for more than 30 seconds (600 ticks), log an error and set `ShutdownRequested` anyway. The remaining entities will be recovered from shadows by the other servers.

Store drain start tick in `DrainState`:
```rust
pub struct DrainState {
    pub draining: bool,
    pub targets: Option<Vec<u16>>,
    pub started_tick: Option<u64>,  // NEW
}
```

**Tests:**
- Unit test: set up a draining world with 3 entities and a target peer. Run `process_drain`. Verify entities have `TransferOutbound` and are not despawned. Verify `ShutdownRequested` is false.
- Unit test: simulate ACKs for all 3 entities. Run `process_transfer_acks`. Verify all despawned. Run `process_drain` again. Verify `ShutdownRequested` is true.
- Unit test: drain timeout -- set started_tick far in the past, verify shutdown is requested even with pending transfers.
- Schedule test: `coord_drain_and_shutdown_in_schedule` test needs updating to account for two-phase (drain + ACK + shutdown across multiple ticks).

**Done when:** `cargo test -p sim-server` passes. Drain uses two-phase transfers and waits for ACKs before shutdown.

---

## Issue 2: UDP Replication Chunking (High)

### WI-009: Add ChunkedEntityUpdate to PeerMessage

**Size:** small
**Depends on:** none
**Files:**
- `crates/shared/src/protocol.rs`

**Description:**
Add a new variant to `PeerMessage`:

```rust
PeerMessage::ChunkedEntityUpdate {
    tick: u64,
    server_id: ServerId,
    chunk_index: u16,
    total_chunks: u16,
    entities: Vec<EntityState>,
}
```

**Tests:**
- Unit test: bincode roundtrip for `ChunkedEntityUpdate` with 10 entities.
- Unit test: bincode roundtrip for `ChunkedEntityUpdate` with 0 entities (edge case).
- Unit test: verify `chunk_index` and `total_chunks` survive roundtrip.

**Done when:** `cargo test -p shared` passes.

---

### WI-010: Implement chunked sending in game loop

**Size:** medium
**Depends on:** WI-009
**Files:**
- `crates/sim-server/src/game_loop.rs` (Phase 4, lines 157-206)

**Description:**
Replace the single-datagram replication logic with chunked sending.

Add a constant:
```rust
const ENTITIES_PER_CHUNK: usize = 100;
```

Replace the Phase 4 replication code (lines 159-189):

```rust
if let Some(entities) = &repl_data {
    let peer_addrs: Vec<_> = world.resource::<PeerAddresses>().0.values().copied().collect();
    if !peer_addrs.is_empty() {
        let server_id_u16 = world.resource::<ServerConfig>().server_id;

        if entities.len() <= ENTITIES_PER_CHUNK {
            // Small payload: use unchunked format (backward compatible)
            let msg = PeerMessage::EntityStateUpdate {
                tick: repl_tick,
                server_id: server_id_u16,
                entities: entities.clone(),
            };
            if let Ok(bytes) = bincode::serialize(&msg) {
                for addr in &peer_addrs {
                    if let Err(e) = udp_socket.send_to(&bytes, addr).await {
                        tracing::warn!("Failed to send to peer {addr}: {e}");
                    }
                }
            }
        } else {
            // Large payload: chunk it
            let total_chunks = ((entities.len() + ENTITIES_PER_CHUNK - 1) / ENTITIES_PER_CHUNK) as u16;
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
                        if let Err(e) = udp_socket.send_to(&bytes, addr).await {
                            tracing::warn!("Failed to send chunk {}/{} to {addr}: {e}", i + 1, total_chunks);
                        }
                    }
                }
            }
        }
    }
}
```

Remove the old `bytes.len() <= 60_000` check entirely. The chunking approach handles arbitrarily large entity counts.

**Tests:**
- No unit tests (the game loop is async and tested via integration tests).
- Verify `cargo build -p sim-server` compiles cleanly.
- Manual test: spawn >100 entities on one server, verify replication still works (other servers see shadows).

**Done when:** `cargo build -p sim-server` compiles. Replication uses chunking for large payloads.

---

### WI-011: Implement chunk reassembly in receiver

**Size:** large
**Depends on:** WI-009, WI-010
**Files:**
- `crates/sim-server/src/receiver.rs`

**Description:**
Add chunk reassembly logic to the UDP receiver.

Add a reassembly data structure (local to the receiver task):

```rust
use std::collections::{HashMap, HashSet};

struct ChunkReassembly {
    total_chunks: u16,
    received: HashSet<u16>,
    entities: Vec<EntityState>,
}

// Key: (server_id as u16, tick)
let mut reassembly: HashMap<(u16, u64), ChunkReassembly> = HashMap::new();
// Track latest tick per server for stale cleanup
let mut latest_tick: HashMap<u16, u64> = HashMap::new();
```

Add a match arm for `PeerMessage::ChunkedEntityUpdate`:

```rust
PeerMessage::ChunkedEntityUpdate { tick, server_id, chunk_index, total_chunks, entities } => {
    let key = (server_id.0, tick);

    // Update latest tick for this server
    let prev_latest = latest_tick.entry(server_id.0).or_insert(0);
    if tick > *prev_latest {
        *prev_latest = tick;
        // Clean up stale reassemblies (more than 2 ticks behind)
        reassembly.retain(|&(sid, t), _| sid != server_id.0 || tick - t <= 2);
    }

    let entry = reassembly.entry(key).or_insert_with(|| ChunkReassembly {
        total_chunks,
        received: HashSet::new(),
        entities: Vec::new(),
    });

    if entry.received.contains(&chunk_index) {
        continue; // duplicate chunk
    }

    entry.received.insert(chunk_index);
    entry.entities.extend(entities);

    if entry.received.len() == entry.total_chunks as usize {
        // Complete! Push to inbound queue
        let complete = reassembly.remove(&key).unwrap();
        let mut q = inbound.queue.lock().await;
        q.peer_updates.push(PeerUpdate {
            tick,
            server_id,
            entities: complete.entities,
        });
    }
}
```

The existing `EntityStateUpdate` match arm remains unchanged -- unchunked packets are processed directly as before.

**Tests:**
- This is async code, so direct unit tests are impractical. Test via integration tests.
- Scene test: spawn 200+ entities on one server. Verify all entities are visible as shadows on other servers (replication works through chunking).
- Scene test: verify that entity count on shadows matches authoritative count even above 100 entities per server.

**Done when:** `cargo build -p sim-server` compiles. Integration tests pass. Replication works for >1,000 entities per server.

---

### WI-012: Add chunking integration test

**Size:** small
**Depends on:** WI-010, WI-011
**Files:**
- New scene test or addition to existing `spatial-hotspot` test scenario

**Description:**
Add a test that specifically verifies chunked replication works:

1. Start a 2-server cluster.
2. Spawn 200 entities on server 1 (forcing >1 chunk).
3. Wait 2 seconds for replication.
4. Query server 2's metrics -- verify shadow count >= 190 (allowing for initial tick latency).
5. Spawn 1,100 entities on server 1 (forcing >10 chunks, past the old 60KB limit).
6. Wait 2 seconds.
7. Verify server 2's shadow count >= 1,090.

This test explicitly verifies the old 60KB limit is broken.

**Tests:** This IS the test.

**Done when:** Test passes against a live cluster.

---

## Issue 3: Atomic Player Transfer for Persistence (High)

### WI-013: Add PlayerTransfer to protocol

**Size:** small
**Depends on:** none
**Files:**
- `crates/shared/src/protocol.rs`

**Description:**
Add a new variant to `CoordClientMsg`:

```rust
CoordClientMsg::PlayerTransfer {
    account_id: String,
    from_server: u16,
    to_server: u16,
}
```

Add a new variant to `CoordServerMsg`:

```rust
CoordServerMsg::PlayerTransferResult {
    account_id: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
```

**Tests:**
- Unit test: JSON roundtrip for `PlayerTransfer`.
- Unit test: JSON roundtrip for `PlayerTransferResult` with `success: true` and `error: None`.
- Unit test: JSON roundtrip for `PlayerTransferResult` with `success: false` and an error message.

**Done when:** `cargo test -p shared` passes.

---

### WI-014: Update persistence design document

**Size:** small
**Depends on:** WI-013
**Files:**
- `docs/player-persistence-design.md`

**Description:**
Update the persistence design document with the following changes:

1. **"Authority transfer during drain" section:** Replace the check-in/check-out flow with the atomic `PlayerTransfer` flow:
   - Source server completes two-phase entity transfer (gets ACK from target).
   - Source server sends `PlayerTransfer { account_id, from_server, to_server }` to coordinator.
   - Coordinator atomically updates `checked_out_to` from `from_server` to `to_server` via `UPDATE ... WHERE checked_out_to = :from_server`.
   - If the WHERE clause does not match (account already reassigned by crash recovery), coordinator logs an error. No duplicate login is possible.

2. **"Crash recovery" section:** Add startup reconciliation:
   - On coordinator startup, query `SELECT account_id, checked_out_to FROM players WHERE checked_out_to IS NOT NULL`.
   - For each result, if the `checked_out_to` server is not registered, set `checked_out_to = NULL`.
   - This runs before accepting `/assign` requests.

3. **"Protocol Additions" section:** Add `PlayerTransfer` and `PlayerTransferResult`.

4. **"Codebase Changes Required" section:** Add coordinator startup reconciliation to the coordinator changes list.

**Tests:** None (documentation only).

**Done when:** The design document accurately reflects the atomic transfer and startup reconciliation mechanisms.

---

## Dependency Graph

```
WI-001 (TransferAck protocol) ----+
                                  |
WI-002 (TransferOutbound comp) ---+--> WI-004 (Send ACK) --> WI-005 (process_transfer_acks)
                                  |                                    |
WI-003 (Receiver ACK handler) ---+                                    v
                                                              WI-006 (process_transfer_timeouts)
                                                                       |
                                                                       v
                                                              WI-007 (Modify process_transfers)
                                                                       |
                                                                       v
                                                              WI-008 (Modify process_drain)

WI-009 (ChunkedEntityUpdate) --> WI-010 (Chunked sending)
                                         |
                                         v
                                 WI-011 (Chunk reassembly) --> WI-012 (Integration test)

WI-013 (PlayerTransfer protocol) --> WI-014 (Update design doc)
```

Issues 1, 2, and 3 are independent of each other and can be worked in parallel. Within each issue, the items must be sequential (each depends on the one before it).

## Implementation Order (recommended)

Start with Issue 1 (authority transfer) because it is Critical severity and affects existing functionality. Then Issue 2 (chunking) to remove the entity ceiling. Issue 3 (persistence protocol) can be done at any time since persistence is not yet implemented.

1. WI-001 + WI-002 + WI-009 + WI-013 (all independent, protocol/component additions)
2. WI-003 (receiver handler for ACKs)
3. WI-004 (send ACKs from receiver side)
4. WI-005 (process_transfer_acks system)
5. WI-006 (process_transfer_timeouts system)
6. WI-007 (modify process_transfers)
7. WI-008 (modify process_drain)
8. WI-010 (chunked sending)
9. WI-011 (chunk reassembly)
10. WI-012 (chunking integration test)
11. WI-014 (update persistence design doc)
