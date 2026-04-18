# 003: Critical/High Issue Fixes

**Status:** Designed, ready for implementation
**Date:** 2026-04-13
**Audit source:** `docs/mesh-auditor/architecture-critique.md` sections 1, 2, 8

This document covers three issues from the architecture audit:

1. **Critical** -- Fire-and-forget authority transfer loses entities on UDP packet loss
2. **High** -- Single-datagram replication hits a hard wall at ~1,000 entities
3. **High** -- Player persistence transfer race condition allows duplicate login

---

## Issue 1: Two-Phase Authority Transfer

### Problem Statement

Authority transfers are fire-and-forget. In `process_transfers` (`systems.rs:585-658`), the source server serializes a `PeerMessage::AuthorityTransfer`, pushes it to `OutboundTransfers`, and immediately calls `despawn_and_remove` on the entity (line 649). The UDP packet is sent later in the game loop (line 194-198). If the packet is lost, the entity is permanently destroyed -- it no longer exists on the source, the target never received it, and shadow copies on other servers go stale after 5 seconds and are cleaned up.

The same pattern exists in `process_drain` (`systems.rs:665-734`), where every authoritative entity is transferred and despawned in one pass.

This is the single most dangerous flaw in the system. NPC loss is recoverable (respawn), but player entity loss during a rebalance would be catastrophic once persistence is implemented.

### Design

#### State Machine

Introduce a per-entity **transfer state** tracked as an ECS component. An entity in transfer continues to exist on the source server, continues to be simulated, and continues to accept inputs. It is simply flagged as "outbound" so the source knows to listen for an ACK or timeout.

```
Normal --> TransferOutbound --> [ACK received] --> Despawn
                             \-> [timeout, retry 1] --> TransferOutbound (retry)
                             \-> [timeout, retry 2] --> Abort (keep entity, log error)
```

On the receiving side:

```
[Transfer received] --> Accept entity --> Send TransferAck back
```

#### New Protocol Message

Add a new `PeerMessage` variant:

```rust
PeerMessage::TransferAck {
    entity_id: EntityId,
    tick: u64,
}
```

This is 24 bytes over UDP (bincode: 4-byte enum discriminant + 16-byte UUID + 8-byte tick). Negligible cost.

#### New ECS Component

```rust
/// Marker: this entity is being transferred to another server.
/// The source continues simulating it until the ACK arrives or timeout.
#[derive(Component, Debug)]
pub struct TransferOutbound {
    pub target_server: u16,
    pub target_addr: SocketAddr,
    pub sent_tick: u64,
    pub retry_count: u8,
    pub serialized_msg: Vec<u8>,  // cached for retry without re-serialization
}
```

#### New Inbound Event

```rust
pub struct InboundTransferAck {
    pub entity_id: EntityId,
    pub tick: u64,
}
```

Add `transfer_acks: Vec<InboundTransferAck>` to `InboundQueue`.

#### Modified Systems

**`process_transfers` (system 7):** Instead of despawning entities immediately after serializing the transfer message, the system now:

1. Builds the `EntityState` and serializes the `AuthorityTransfer` message (same as today).
2. Pushes the serialized bytes to `OutboundTransfers` (same as today).
3. Instead of calling `despawn_and_remove`, adds the `TransferOutbound` component to the entity and removes the `Authoritative` component.
4. The entity remains in the ECS world but is no longer authoritative. It is not included in replication output (the `prepare_replication` query filters `With<Authoritative>`).

**New system: `process_transfer_acks`** (runs after `drain_inbound_events`, before `process_transfers`):

1. Drains `InboundQueue.transfer_acks`.
2. For each ACK, looks up the entity in `EntityLookup`.
3. If the entity has a `TransferOutbound` component, despawns it. The transfer is complete.
4. If the entity does not exist or does not have `TransferOutbound`, logs a warning and ignores (idempotent -- duplicate ACK is harmless).

**New system: `process_transfer_timeouts`** (runs after `process_transfer_acks`):

1. Queries all entities with `TransferOutbound`.
2. For each, checks if `current_tick - sent_tick > TRANSFER_TIMEOUT_TICKS`.
3. If timeout reached and `retry_count < MAX_RETRIES` (1): re-push the cached `serialized_msg` to `OutboundTransfers`, increment `retry_count`, reset `sent_tick` to current tick.
4. If timeout reached and `retry_count >= MAX_RETRIES`: abort the transfer. Remove `TransferOutbound`, re-add `Authoritative`. Log an error. The entity stays on the source server. The coordinator will retry the rebalance later.

**`process_drain`:** Same two-phase treatment. Entities are flagged `TransferOutbound` instead of despawned. The drain is not "complete" until all outbound entities have been ACK'd or timed out. Only then does `ShutdownRequested` get set.

**`drain_inbound_events`:** Already handles `AuthorityTransfer` for the receiving side. After accepting a transfer (promoting shadow or spawning new entity), the system must now also push a `TransferAck` message to `OutboundTransfers` (or a new `OutboundAcks` resource) for the game loop to send back to the source.

#### Constants

```rust
const TRANSFER_TIMEOUT_TICKS: u64 = 10;  // 500ms at 20Hz
const TRANSFER_MAX_RETRIES: u8 = 1;      // one retry, then abort
```

500ms timeout is conservative for a LAN where RTT is <1ms. On WiFi between UNO Q boards, RTT is ~5-10ms. The 500ms timeout provides ample margin without holding entities in limbo too long.

#### Data Flow

```
Source server                    Target server
     |                                |
     |--- AuthorityTransfer --------->|
     |    (entity flagged Outbound,   |    (accept entity, promote/spawn)
     |     still simulating)          |
     |                                |
     |<-------- TransferAck ----------|
     |    (despawn entity)            |
     |                                |
```

If the `AuthorityTransfer` packet is lost:
```
Source server                    Target server
     |                                |
     |--- AuthorityTransfer --X       |
     |    (entity flagged Outbound)   |
     |    ... 500ms timeout ...       |
     |--- AuthorityTransfer --------->|  (retry)
     |                                |
     |<-------- TransferAck ----------|
     |    (despawn entity)            |
```

If the `TransferAck` packet is lost:
```
Source server                    Target server
     |                                |
     |--- AuthorityTransfer --------->|
     |                                |--- TransferAck --X
     |    ... 500ms timeout ...       |
     |--- AuthorityTransfer --------->|  (retry)
     |                                |--- TransferAck ---------->  (duplicate accept is idempotent)
     |    (despawn entity)            |
```

If both retries fail:
```
Source server                    Target server (unreachable)
     |                                X
     |--- AuthorityTransfer --X       |
     |    ... 500ms timeout ...       |
     |--- AuthorityTransfer --X       |  (retry)
     |    ... 500ms timeout ...       |
     |    ABORT: re-add Authoritative |
     |    (entity stays on source)    |
```

#### Handling the Outbound State

While an entity has `TransferOutbound`:

- **Simulation:** The entity does NOT have `Authoritative`, so `npc_behavior` and `physics_integration` will not apply to it (they query `With<Authoritative>`). This is intentional -- the entity is frozen in place during the brief transfer window. At 500ms max, this is imperceptible for NPCs.

- **Replication:** The entity is not in `prepare_replication` output (no `Authoritative` marker). Other peers stop receiving updates for it. Their shadows will become stale and eventually be cleaned up, but the target will start replicating the entity as soon as it accepts the transfer. The gap is bounded by the timeout.

- **Client visibility:** The entity is not in `prepare_client_snapshot` for the source server's clients (it queries all entities -- check this). Actually, `prepare_client_snapshot` queries both `Authoritative` and `Shadow` entities. A `TransferOutbound` entity is neither. It should still be visible to avoid a visual pop. The snapshot query should include `TransferOutbound` entities, or the `prepare_client_snapshot` system should be updated to also query entities with `TransferOutbound`. The simplest approach: keep `Authoritative` on the entity during the transfer, but add a `TransferOutbound` marker alongside it. The entity is still authoritative (the source is the source of truth until the ACK) but flagged for transfer tracking.

**Revised component approach:** Do NOT remove `Authoritative` during transfer. Add `TransferOutbound` as an additional marker. The entity continues to be simulated, replicated, and visible. The only change is that the source is listening for an ACK. When the ACK arrives, both `Authoritative` and `TransferOutbound` are removed, and the entity is despawned.

This means the entity is briefly authoritative on both the source and target (after the target accepts but before the source despawns). This is a small window (one UDP round-trip). The tick-based conflict resolution in `drain_inbound_events` handles this: the target's replication will have a higher tick than the source's shadow on other peers, so the target's state wins.

#### Idempotency on the Receiving Side

The target may receive the same `AuthorityTransfer` twice (retry). The second time:

- If the entity is already authoritative on the target (first transfer was accepted), the target's `drain_inbound_events` already handles this: "Ignoring duplicate transfer for {eid}" (line 96-103). It sends another `TransferAck` (idempotent).
- If the entity is a shadow (first transfer was lost, shadow still exists from replication), the target promotes it as before. Sends `TransferAck`.
- If the entity does not exist (first transfer lost, no shadow), the target spawns it as authoritative. Sends `TransferAck`.

All cases are safe and idempotent.

#### Impact on Tick Budget

- Per entity in transfer: one hash lookup per tick in `process_transfer_timeouts` to check the timeout. With at most ~10% of entities in transfer at once (rebalance cap), this is 10-100 lookups. Negligible.
- `TransferAck` messages: 24 bytes each, sent once per accepted transfer. Negligible bandwidth.
- Cached `serialized_msg` in the component: ~200-300 bytes per entity in transfer. With 100 entities in transfer, that is 20-30 KB. Negligible memory.

#### Failure Modes

| Scenario | Outcome |
|----------|---------|
| Single packet loss | Retry at 500ms succeeds. Entity transferred with 500ms delay. |
| Both packets lost | Abort after 1000ms. Entity stays on source. Coordinator retries rebalance later. |
| Target crashes during transfer | ACK never arrives. Source aborts, keeps entity. Coordinator detects crash, issues recovery. |
| Source crashes during transfer | Entity was still authoritative, so shadows exist. Recovery adopts from shadows. |
| ACK lost | Source retries transfer. Target handles duplicate (idempotent). |
| Network partition | Both retries fail. Source aborts, keeps entity. Entity is safe. |

---

## Issue 2: UDP Replication Chunking

### Problem Statement

In `game_loop.rs:169-189`, all authoritative entities are serialized into a single `PeerMessage::EntityStateUpdate` and sent as one UDP datagram. The code checks `bytes.len() <= 60_000` and silently drops the packet if it exceeds this size. At ~56 bytes per entity (bincode-serialized `EntityState`), plus bincode envelope overhead for the `PeerMessage` enum and Vec length prefix, the 60KB limit is hit at approximately 1,000 entities per server. At that point, replication to all peers stops entirely -- no entity data reaches any peer.

On a real network (WiFi between UNO Q boards), the MTU is typically 1500 bytes. UDP datagrams larger than the MTU are fragmented by IP. A single lost IP fragment causes the entire UDP datagram to be dropped. At 30KB (500 entities), the datagram is split into ~21 IP fragments. Losing any one of them drops all 500 entity updates. This makes large datagrams unreliable even below the 60KB hard limit.

### Design

#### Approach: Chunked Replication with Sequence-Based Reassembly

Replace the single large datagram with a series of smaller chunks, each fitting within a safe MTU. The receiver reassembles chunks from the same tick before processing.

#### New Protocol Message

Replace (or extend) `EntityStateUpdate` with chunked variants. The cleanest approach is a new message type that wraps the entity data:

```rust
PeerMessage::ChunkedEntityUpdate {
    tick: u64,
    server_id: ServerId,
    chunk_index: u16,     // 0-based index of this chunk
    total_chunks: u16,    // total number of chunks for this tick
    entities: Vec<EntityState>,  // subset of entities for this chunk
}
```

Each chunk is independently deserializable -- it contains a valid Vec of EntityState. No raw byte splitting or reassembly of partial entities. This avoids complex byte-level reassembly logic and means each chunk is usable on its own even if other chunks are lost.

The existing `EntityStateUpdate` variant is kept for backward compatibility during rollout (small payloads that fit in a single packet can still use the old format). Once all servers are updated, the old variant can be deprecated.

#### Chunk Size Calculation

Target: each chunk should fit within a safe UDP payload size.

- WiFi/LAN MTU: 1500 bytes (IP header: 20 bytes, UDP header: 8 bytes = 1472 bytes payload)
- Safe margin: 1400 bytes per chunk (accounts for potential IP options, VLAN tags, etc.)
- Bincode overhead per chunk: ~30 bytes (enum discriminant: 4, tick: 8, server_id: 2, chunk_index: 2, total_chunks: 2, Vec length prefix: 8, alignment: ~4)
- Available for entity data per chunk: ~1370 bytes
- Bincode entity size: ~53 bytes (UUID: 16, position: 12, velocity: 12, entity_type: 4 with bincode enum, authority_server: 2, last_input_seq: 4, alignment: ~3)
- Entities per chunk: ~25

But this is extremely conservative. On a local network (gigabit ethernet between UNO Q boards), we can safely use larger chunks:

| Chunk target | Entities per chunk | Chunks for 1,000 entities |
|-------------|-------------------|--------------------------|
| 1,400 bytes (MTU-safe) | ~25 | 40 |
| 8,000 bytes (compromise) | ~145 | 7 |
| 32,000 bytes (LAN only) | ~580 | 2 |

The recommended approach is a **configurable chunk size** with a sensible default:

```rust
const DEFAULT_CHUNK_SIZE: usize = 8_000;  // ~145 entities per chunk
```

8KB is a good default: small enough to survive IP fragmentation on typical LANs (even if fragmented into 6 IP packets, loss of one only drops ~145 entities instead of all 1,000), but large enough that the overhead of multiple chunks is low.

For the UNO Q boards on a dedicated gigabit LAN, 32KB chunks would also be fine. The value can be made a CLI flag later.

#### Sender Changes (`game_loop.rs`)

Replace the single-datagram send logic (lines 159-189) with:

```
1. Take entities from OutboundReplication (same as today)
2. If entity count fits in one chunk (serialized size <= CHUNK_SIZE):
     Send as PeerMessage::EntityStateUpdate (backward compatible, same as today)
3. Else:
     Split entities into groups of ENTITIES_PER_CHUNK
     For each group, serialize as PeerMessage::ChunkedEntityUpdate
     Send each chunk to each peer
```

The number of entities per chunk should be calculated once at startup based on `DEFAULT_CHUNK_SIZE` and a measurement of the bincode size of a sample entity. In practice, hard-coding `ENTITIES_PER_CHUNK = 100` (with 8KB chunks) is fine for the prototype.

#### Receiver Changes (`receiver.rs`)

The receiver needs a reassembly buffer per peer per tick:

```rust
struct ChunkReassembly {
    tick: u64,
    server_id: ServerId,
    total_chunks: u16,
    received: HashSet<u16>,       // chunk indices received
    entities: Vec<EntityState>,   // accumulated entities
}
```

Store as `HashMap<(ServerId, u64), ChunkReassembly>` in the receiver task.

When a `ChunkedEntityUpdate` arrives:

1. Look up or create the reassembly entry for `(server_id, tick)`.
2. If this chunk index was already received, skip (duplicate).
3. Add the chunk's entities to the accumulated list. Mark the chunk index as received.
4. If `received.len() == total_chunks`, the reassembly is complete. Push a single `PeerUpdate` to the `InboundQueue` and remove the reassembly entry.
5. If the tick advances (new tick from the same server), discard any incomplete reassembly for older ticks. This prevents memory leaks from permanently incomplete reassemblies.

When a `EntityStateUpdate` (unchunked) arrives, process it directly as today. No reassembly needed.

#### Stale Reassembly Cleanup

If some chunks for a tick are lost, the reassembly entry would linger forever. Clean up entries older than 2 ticks (100ms). The receiver should track the latest tick seen per server and discard entries that are more than 2 ticks behind.

This means: if 1 out of 7 chunks for a tick is lost, the remaining 6 chunks are discarded. The entities appear stale on the receiver (using the previous tick's data) until the next tick's complete set of chunks arrives. At 20Hz, this is a 50ms delay -- same as a single lost packet in the current design. No regression.

#### Impact on Tick Budget

- **Serialization:** Instead of one `bincode::serialize` call, we make N calls (one per chunk). With 7 chunks, this is 7x the serialization overhead. Each serialization is for ~100 entities (~5KB), which is sub-microsecond on modern hardware. Total: <10us. Negligible.
- **UDP sends:** Instead of 1 `send_to` per peer, we make N per peer. With 7 chunks and 2 peers, that is 14 `send_to` calls instead of 2. Each `send_to` is ~1us. Total: ~14us. Negligible.
- **Receiver reassembly:** HashMap lookup and insert per chunk. With 7 chunks per tick per peer, that is 14 operations per tick (2 peers). Negligible.
- **Memory:** One `ChunkReassembly` per peer per tick. With 2 peers and 1,000 entities, that is 2 reassembly buffers holding ~56KB of entity data. Trivially fits in memory.

#### Bandwidth Impact

Chunking adds per-chunk overhead (30 bytes per chunk for the header). With 7 chunks, that is 210 bytes extra per peer per tick. At 20Hz and 2 peers, that is 8.4 KB/s additional bandwidth. Negligible compared to the ~60KB/tick entity data.

#### Failure Modes

| Scenario | Outcome |
|----------|---------|
| One chunk lost | Entire tick discarded. Shadows use previous tick's data (50ms stale). |
| All chunks lost | Same as today's single-packet loss. Shadows stale by one tick. |
| Duplicate chunk | Duplicate detection via chunk index set. Ignored. |
| Out-of-order chunks | Reassembly buffer collects regardless of order. |
| Old-format server | Sends `EntityStateUpdate`, receiver handles directly. Compatible. |
| New-format server to old receiver | Old receiver cannot deserialize `ChunkedEntityUpdate`. The unknown enum variant will cause a bincode deserialization error, which is already logged and skipped. Replication breaks until the old server is updated. |

#### Scaling Analysis

| Entity count | Chunks (at 100/chunk) | UDP sends per peer | Total bytes per peer |
|-------------|----------------------|-------------------|---------------------|
| 100 | 1 (unchunked) | 1 | ~6 KB |
| 500 | 5 | 5 | ~30 KB |
| 1,000 | 10 | 10 | ~56 KB |
| 5,000 | 50 | 50 | ~280 KB |
| 10,000 | 100 | 100 | ~560 KB |

At 10,000 entities per server with 9 peers (10-server mesh), each server sends 100 * 9 = 900 UDP packets per tick = 18,000 packets/second. Each packet is ~5.6KB, totaling ~5.04 MB/s outbound. This is well within gigabit ethernet capacity but significant. Interest management (filtering which entities each peer needs) becomes important at this scale, but that is a separate design.

---

## Issue 3: Atomic Player Transfer for Persistence

### Problem Statement

The persistence design in `docs/player-persistence-design.md` describes an authority transfer flow for player entities:

> Before transferring a player entity, sim-server sends `PlayerCheckIn` to coordinator. Target server receives entity via UDP transfer. Target server sends `PlayerCheckOut` to coordinator.

Between the `PlayerCheckIn` and the `PlayerCheckOut`, the player's `checked_out_to` field is NULL. During this window, if the coordinator receives a new login for this account (from a different client or a reconnecting client), it would succeed -- the account appears available. A duplicate player entity could be spawned.

The window is small (time for the UDP transfer + target's processing + WebSocket message to coordinator), probably 10-50ms on LAN. But it is a correctness hole that could cause inventory duplication, state corruption, or other issues in any system built on top of persistence.

### Design

#### Atomic Transfer Message

Replace the check-in/check-out sequence with a single `PlayerTransfer` message:

```rust
// Sim-Server -> Coordinator
CoordClientMsg::PlayerTransfer {
    account_id: String,
    from_server: u16,
    to_server: u16,
}
```

The coordinator handles this atomically:

```sql
UPDATE players SET checked_out_to = :to_server
WHERE account_id = :account_id AND checked_out_to = :from_server;
```

If the WHERE clause matches (the account is currently checked out to `from_server`), the update succeeds and the account is now checked out to `to_server`. If it does not match (account was already checked in by crash recovery, or checked out to a different server due to a race), the update affects 0 rows and the coordinator returns an error.

This is a compare-and-swap operation on the `checked_out_to` field. It is atomic at the SQLite level (single UPDATE statement in a transaction). There is no window where the account is unassigned.

#### Updated Transfer Flow

```
Source server                Coordinator              Target server
     |                           |                          |
     |-- AuthorityTransfer ------|------------------------->|
     |   (two-phase, from        |                          |
     |    Issue 1 design)        |                          |
     |                           |                          |
     |<-- TransferAck -----------|--------------------------|
     |                           |                          |
     |-- PlayerTransfer -------->|                          |
     |   { account, from, to }   |                          |
     |                           |-- UPDATE (atomic) ------>|
     |                           |                          |
     |   (despawn entity)        |                          |
```

The `PlayerTransfer` message is sent after the `TransferAck` confirms the entity arrived safely. This means:

1. The entity is on the target (confirmed by ACK).
2. The coordinator atomically moves the check-out from source to target.
3. The source despawns.

At no point is the account unchecked. The only window is between the ACK and the `PlayerTransfer` message, during which the entity exists on the target but the coordinator still thinks it is on the source. If the source crashes during this window, the coordinator's crash recovery sees the account checked out to the (now dead) source. It will forcibly reassign based on surviving servers. The target already has the entity, so recovery will find it and the account will be corrected.

#### Source Server Responsibility

The source server is the one that sends the `PlayerTransfer` message. The target does not need to do anything special for persistence -- it just accepts the entity as it does today. This keeps the complexity on one side and means the target does not need to know about persistence at all.

If the source crashes between sending the `AuthorityTransfer` and sending `PlayerTransfer`, the entity is on the target but the coordinator thinks it is on the source. Crash recovery handles this:

1. Coordinator detects source crash.
2. Coordinator finds account checked out to dead source.
3. Coordinator sends `RecoverEntities` to survivors.
4. Target already has the entity (it accepted the transfer). It becomes authoritative.
5. Coordinator needs a mechanism to learn that the target now has this player. Two options:
   a. The target reports its player entities during the next heartbeat (add `player_accounts: Vec<String>` to the heartbeat).
   b. The coordinator proactively queries surviving servers for accounts from the dead server.

Option (a) is simpler. Add an optional `player_accounts` field to the heartbeat that lists account IDs of player entities currently held. The coordinator cross-references on crash recovery.

#### Startup Reconciliation

On coordinator startup, after connecting to SQLite:

```sql
SELECT account_id, checked_out_to FROM players WHERE checked_out_to IS NOT NULL;
```

For each account with a non-NULL `checked_out_to`:
1. Check if the corresponding `server_id` is in the registry (has a connected, healthy server).
2. If not, set `checked_out_to = NULL` (force check-in using the last flushed state).

This runs before the coordinator starts accepting `/assign` requests, so no race condition is possible during startup.

The window between coordinator startup and sim-server re-registration is important. Sim-servers may take a few seconds to reconnect. The reconciliation should run with a small delay (e.g., 5 seconds after startup) to give servers time to register. Alternatively, reconciliation can run immediately and then re-run whenever a server registers (checking if any of its checked-out accounts should be restored).

The simpler approach: run reconciliation immediately on startup (check in all accounts checked out to unregistered servers), and let the normal `PlayerFlush` mechanism re-establish correct state as servers register and begin flushing.

#### Updated Schema

Add a lease expiry column for future use (not implemented now, but the schema should include it):

```sql
ALTER TABLE players ADD COLUMN lease_expires_at TEXT;
```

This enables future lease-based protection against split-brain scenarios.

#### Protocol Additions

```rust
// Sim-Server -> Coordinator (new)
CoordClientMsg::PlayerTransfer {
    account_id: String,
    from_server: u16,
    to_server: u16,
}

// Coordinator -> Sim-Server (new, response to PlayerTransfer)
CoordServerMsg::PlayerTransferResult {
    account_id: String,
    success: bool,
    error: Option<String>,
}
```

The `PlayerTransferResult` is optional for the initial implementation -- the source server can assume success and proceed with despawn. Failure (0 rows affected) is logged by the coordinator and the account will self-heal via heartbeat reconciliation. Adding the response enables the source to handle failures gracefully in the future.

#### Impact on Existing Design

The persistence design document (`docs/player-persistence-design.md`) needs the following updates:

1. **"Authority transfer during drain" section:** Replace the check-in/check-out sequence with `PlayerTransfer`.
2. **"Protocol Additions" section:** Add `PlayerTransfer` to `CoordClientMsg`.
3. **"Codebase Changes Required" section:** Add startup reconciliation to the coordinator.
4. **"Data Loss Guarantees" table:** The "Graceful server drain" row remains "None" because the transfer is now atomic.

#### Failure Modes

| Scenario | Outcome |
|----------|---------|
| Normal transfer | Atomic update, no window for duplicate login. |
| Source crash before PlayerTransfer | Account stuck on dead source. Crash recovery detects and reconciles. |
| Target crash after accepting entity | Entity lost on target. Source already despawned. Crash recovery uses shadow data. Account corrected by heartbeat reconciliation. |
| Coordinator crash during PlayerTransfer | Message lost. Account remains on source in SQLite. Source retries on reconnect. |
| Duplicate login attempt during transfer | `checked_out_to` is always set (either source or target). Login attempt sees account in use and returns error. |
| Coordinator restart with stale data | Startup reconciliation cleans up accounts checked out to dead servers. |

---

## Schedule Changes Summary

The ECS schedule in `schedule.rs` needs two new systems:

```
drain_inbound_events
  -> process_transfer_acks        [NEW]
  -> process_transfer_timeouts    [NEW]
  -> drain_client_events
  -> process_client_inputs
  -> process_coord_events
  -> npc_behavior
  -> physics_integration
  -> clamp_positions
  -> process_transfers            [MODIFIED]
  -> process_drain                [MODIFIED]
  -> prepare_replication
  -> prepare_client_snapshot
  -> shadow_cleanup
  -> record_metrics
```

The two new systems run early in the tick, right after inbound events are drained, so that ACKs from the previous tick are processed before any new transfers are initiated.
