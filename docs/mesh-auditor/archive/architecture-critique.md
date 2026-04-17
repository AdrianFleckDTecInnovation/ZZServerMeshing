# Architecture Critique: Server Meshing System

**Auditor:** mesh-auditor (Opus)
**Date:** 2026-04-13
**Scope:** Full system -- coordinator, sim-server, client, protocol, and planned persistence layer
**Codebase revision:** `b71779d` (dev branch)

This document is a single cohesive analysis of the server meshing architecture. It is not a catalog of individual bugs (those live in `docs/mesh-auditor/001-027`), but a structural assessment of each major subsystem: what works, what breaks, where the walls are, and what to do about them.

---

## Table of Contents

1. [Entity Authority Model](#1-entity-authority-model)
2. [UDP Replication](#2-udp-replication)
3. [Coordinator as Single Point of Failure](#3-coordinator-as-single-point-of-failure)
4. [Crash Recovery](#4-crash-recovery)
5. [Rebalance Algorithm](#5-rebalance-algorithm)
6. [Client Connection Model](#6-client-connection-model)
7. [Binary Protocol](#7-binary-protocol)
8. [Player Persistence Design](#8-player-persistence-design)

---

## 1. Entity Authority Model

### Current Design

Every entity has exactly one authoritative server at any time, tracked by `EntityState.authority_server` (a `ServerId(u16)` in `shared/entity.rs:41`). The sim-server that owns an entity simulates it, replicates its state to peers via UDP, and broadcasts it to its connected clients. Other servers hold read-only "shadow copies" (`Shadow` component in `components.rs:31`) with a `last_seen_tick` for staleness tracking.

Authority transfers are initiated by the coordinator via `RebalanceDirective` (`protocol.rs:119`) and executed by the source server sending `PeerMessage::AuthorityTransfer` over UDP (`systems.rs:632-644`). The source despawns the entity immediately after serializing the transfer message (`systems.rs:649`). The target accepts the entity by either promoting an existing shadow or spawning a new authoritative entity (`systems.rs:82-148`).

### Strengths

**Single-writer eliminates write conflicts.** Because exactly one server simulates any given entity, there are no concurrent-write conflicts on entity state. This is the most important invariant in the system and it holds cleanly. The code enforces it: `drain_inbound_events` explicitly skips peer updates for entities it owns (`systems.rs:48-50`), and transfer acceptance validates that the target matches `server_id` (`systems.rs:83-90`).

**Transfer is atomic on the source side.** The source server builds the full `EntityState`, serializes the transfer message, and despawns in a single system call (`process_transfers` at `systems.rs:585-658`). There is no window where the entity exists on both servers from the source's perspective.

**Forwarded input works.** When a client sends a `MoveInput` for an entity owned by another server, the receiving server looks up the authority from the shadow's `AuthorityServer` component and forwards via UDP (`systems.rs:338-368`). This is correct and handles the common case of a player whose authority transfers while they are connected to a different server.

### Pitfalls and Failure Modes

**Fire-and-forget authority transfer (critical).** The transfer is sent as a single UDP packet with no acknowledgment (`game_loop.rs:194-198`). If this packet is lost, the entity is deleted from the source (line 649) and never arrives at the target. The entity is gone. The shadow copies on other servers will eventually be cleaned up by `shadow_cleanup` (100 ticks / 5 seconds). This is the single most dangerous flaw in the system. It was documented in audit finding #002 and remains unfixed.

The severity depends on entity type. Losing an NPC is recoverable (respawn). Losing a player entity mid-game is not. With the planned persistence design, a lost player entity would be recovered from the last coordinator flush (up to 5 seconds stale), but the client would experience a hard disconnect and respawn.

**Split-brain during transfer is a real window.** Between the source despawning the entity and the target receiving the transfer packet, neither server is authoritative. If a client sends a `MoveInput` during this window, it hits the forwarding path, which looks up the authority from the shadow's `AuthorityServer` component. But the shadow still points to the old server (the source), which no longer has the entity. The input is forwarded to the source, which ignores it because it no longer owns the entity. The input is silently lost.

This window is small (one UDP round-trip, typically sub-millisecond on LAN, a few ms on a real network) but grows with packet loss or congestion. On the UNO Q boards over WiFi, this window could be 10-50ms.

**No transfer timeout or retry.** If the target server is unreachable (peer address stale, process crashed between the coordinator issuing the directive and the transfer executing), the transfer fires into the void. The source server does check that the target is in the peer list (`systems.rs:595-601`), but the peer list can be stale -- the coordinator broadcasts it on topology changes, but there is no heartbeat-level freshness check on the peer addresses.

### Scalability Limits

The single-authority model scales well in theory -- each entity is only simulated once. The practical limit is the authority transfer mechanism. At high entity counts with frequent rebalancing, the number of fire-and-forget transfers increases, and each one is a dice roll against UDP reliability. With 3 servers and hundreds of entities, the expected loss rate is low. With 10+ servers and thousands of entities under active rebalancing, lost transfers become a statistical certainty.

### Recommendations

1. **Add an ACK to authority transfers.** The target should send a `PeerMessage::TransferAck { entity_id, tick }` back to the source. The source should not despawn the entity until the ACK arrives, or after a timeout (e.g., 500ms). On timeout, re-send the transfer once, then fail loudly and keep the entity.

2. **Use a two-phase protocol for player entities.** Source sends transfer, waits for ACK, then despawns. If no ACK within 1 second, abort and keep the entity. Player entities are too valuable to gamble on UDP.

3. **Add a transfer-in-progress state.** While an entity is pending transfer, it should still be simulated on the source but flagged as "outbound." This eliminates the split-brain window -- the source continues to process inputs until the target confirms receipt.

---

## 2. UDP Replication

### Current Design

Every tick, each sim-server serializes all its authoritative entities into a single `PeerMessage::EntityStateUpdate` via bincode and sends it as one UDP datagram to every peer (`game_loop.rs:159-189`). The receiver deserializes the packet and pushes entity updates into the inbound queue (`receiver.rs:32-42`), which are drained on the next tick to update or create shadow entities (`systems.rs:42-78`).

Shadow entities track `last_seen_tick` for conflict resolution: stale packets (tick lower than the shadow's last seen tick) are dropped (`systems.rs:54-55`). Shadows older than `SHADOW_STALE_TICKS` (100 ticks / 5 seconds) are cleaned up (`systems.rs:890-904`).

### Strengths

**Simple and effective for the prototype scale.** One packet per peer per tick is easy to reason about. Bincode is fast (zero-copy deserialization for fixed-size types). The tick-based conflict resolution is correct for the single-writer model -- there is never a legitimate reason for a shadow to go backward in time.

**Stale packet rejection works.** The `last_seen_tick` check in `drain_inbound_events` (`systems.rs:54-55`) correctly handles out-of-order UDP delivery. This is important because UDP provides no ordering guarantees.

### Pitfalls and Failure Modes

**No packet loss detection or recovery.** If a replication packet is lost, the shadow entities on the receiving server remain at their previous positions until the next tick's packet arrives. At 20Hz, this means a maximum of 50ms of stale data per lost packet, which is acceptable for visual smoothness. But it also means that entity creation and removal events can be lost.

If a new entity's first replication packet is lost, it simply appears one tick late. That is fine. But if the last replication packet before an entity is removed is lost, the shadow lingers until `SHADOW_STALE_TICKS` (5 seconds) elapses. During this time, the shadow is visible to clients connected to the receiving server as a frozen ghost entity. This is a cosmetic issue, not a correctness issue, but it is noticeable.

**Single-datagram limit of ~60KB.** The code explicitly checks `bytes.len() <= 60_000` (`game_loop.rs:176`) and drops the packet if it exceeds this threshold, logging a warning. Each entity is approximately 56 bytes in bincode (16-byte UUID + 12 position + 12 velocity + 1 type + 2 authority + 4 seq + bincode envelope overhead). At ~56 bytes per entity plus message framing, the 60KB limit is hit at approximately 1,000 entities per server. At that point, replication silently stops for all peers. This is a hard wall.

**O(peers) duplication of the same bytes.** The replication packet is serialized once but sent to each peer individually via `udp_socket.send_to` in a loop (`game_loop.rs:177-179`). With 3 servers (2 peers each), this is negligible. With 10 servers (9 peers each), the cost is 9 copies of a potentially large packet, and each `send_to` is an async operation awaited serially.

**Replication blocks the game loop.** The UDP sends happen inside the tick loop (`game_loop.rs:157-206`) using `await`. Each `send_to` is non-blocking at the OS level (UDP send is typically instant), but the tokio task yields at each `.await` point. This is fine for 2 peers but becomes a concern at scale. This was flagged in audit finding #013.

**No backpressure.** A slow or struggling peer receives the full replication firehose regardless of its ability to process it. If the peer's receive buffer fills up, packets are silently dropped by the OS. There is no mechanism to detect this or reduce replication rate.

### Scalability Limits

| Entity count | Packet size (approx) | Status |
|-------------|---------------------|--------|
| 100 | ~6 KB | Fine |
| 500 | ~30 KB | Fine |
| 1,000 | ~56 KB | At the 60KB limit |
| 1,071 | ~60 KB | Hits the hard cap, replication stops |
| 2,000+ | N/A | Requires chunking |

The entity-per-server limit is approximately 1,000 before replication fails entirely. With 3 servers each owning ~333 entities, the total mesh can hold ~1,000 entities. With 10 servers each owning ~100 entities, the per-server limit is not the bottleneck -- total bandwidth is (10 servers * 9 peers * ~6KB * 20Hz = ~10.8 MB/s total network traffic).

### Recommendations

1. **Add UDP packet chunking.** When the serialized payload exceeds a threshold (e.g., 1400 bytes for safe MTU fit, or 60KB for loopback/LAN), split into multiple packets with a sequence number and chunk index. The receiver reassembles. This is the minimum fix to break through the 1,000-entity wall.

2. **Move UDP sends off the game loop.** Serialize the replication data in the ECS schedule, then hand the bytes to a dedicated send task via a channel. The game loop should never await network I/O.

3. **Add per-peer replication filtering.** Do not send all entities to all peers. Each peer only needs shadows for entities it might interact with or that its clients might see. At minimum, skip entities that the peer already has as authoritative (they already know their own state). At best, use spatial filtering.

4. **Add per-peer health tracking.** Monitor packet loss per peer (via sequence numbers) and reduce replication rate to struggling peers.

---

## 3. Coordinator as Single Point of Failure

### Current Design

The coordinator (`crates/coordinator/`) is a single process that serves several functions:

- **Server registry:** Sim-servers register on connect (`registry.rs:31-57`), send heartbeats every second (`coordinator_client.rs:50-65`), and are removed on disconnect or heartbeat timeout (`main.rs:222-253`).
- **Peer discovery:** On any topology change, the coordinator broadcasts an updated `PeerList` to all servers (`registry.rs:234-260`).
- **Health monitoring:** Detects dead servers via 5-second heartbeat timeout (`registry.rs:71-80`).
- **Crash recovery orchestration:** Broadcasts `RecoverEntities` to survivors with a sorted list for deterministic entity adoption (`registry.rs:103-118`).
- **Load balancing:** Runs rebalance checks every 5 seconds (`main.rs:254-257`).
- **Client assignment:** HTTP endpoint `/assign` returns the least-loaded server (`main.rs:525-546`).
- **Dashboard:** Serves an HTML dashboard at `/dashboard` (`main.rs:562-569`).

The coordinator is stateless -- all its state is derived from connected sim-servers. It can be restarted and sim-servers will reconnect automatically (`coordinator_client.rs:19`).

### Strengths

**Stateless design is resilient.** The coordinator holds no persistent data. All state is reconstructed from sim-server registrations and heartbeats. A coordinator restart causes a brief disruption (servers lose peer lists, rebalancing pauses, `/assign` returns 503) but no data loss.

**Auto-reconnect is robust.** The coordinator client on each sim-server wraps the entire connection lifecycle in a `loop` (`coordinator_client.rs:19`) with a 2-second retry on failure. The coordinator does not need to be up before sim-servers start.

**Peer list broadcast is correct.** Each server receives a peer list excluding itself (`registry.rs:248-253`). The peer list includes IP, UDP port, and WS port, which is sufficient for direct communication.

### Pitfalls and Failure Modes

**During coordinator downtime, no new topology changes are propagated.** If a sim-server joins or dies while the coordinator is down, the other servers do not learn about it. They continue sending to stale peer addresses. A sim-server that dies during coordinator downtime has its entities stranded -- no `RecoverEntities` is broadcast, no recovery happens. The shadows on other servers go stale after 5 seconds and are silently deleted.

**Client assignment fails entirely during coordinator downtime.** The `/assign` endpoint returns 503 ("no servers available") because the registry is empty after a restart until servers re-register. New clients cannot join. Existing clients are unaffected (they are connected directly to sim-servers).

**No authentication on the coordinator WebSocket.** Any process that connects to port 7870 can register as a sim-server with any `server_id`. A malicious or misconfigured process could register as S1 and receive peer lists, rebalance directives, and recovery messages. In the current deployment model (trusted LAN / embedded hardware), this is acceptable. In any multi-tenant or internet-facing deployment, it is not.

**Duplicate registration silently ignored.** If a sim-server crashes and restarts quickly (same `server_id`), the coordinator rejects the registration: "S{server_id} already registered -- ignoring duplicate" (`registry.rs:40-42`). The old entry remains with a dead `reply_tx` channel. The new server never receives peer lists or directives until the old entry times out (5 seconds). During this window, the server is isolated.

**Heartbeat-based crash detection is slow.** The 5-second timeout means a crashed server is not detected for up to 5 seconds (plus the rebalance interval, which is also 5 seconds). In the worst case, 10 seconds pass between a crash and recovery initiation. During this time, the crashed server's entities are frozen on other servers as shadows.

### Scalability Limits

The coordinator is a single-threaded event loop using `tokio::select!` over an `UnboundedReceiver` and a 5-second interval (`main.rs:207-259`). With 3-10 servers sending heartbeats every second, the load is negligible. With 100 servers, the coordinator processes ~100 heartbeats/second plus occasional topology changes and rebalance checks. This is still well within a single-threaded event loop's capacity.

The real scalability concern is the `/assign` endpoint. It acquires a read lock on the registry (`main.rs:527`) for each HTTP request. Under load (many clients connecting simultaneously), this serializes client assignment. The hand-rolled HTTP server (`handle_http_request` at `main.rs:483-622`) reads only 1024 bytes from each connection and does not handle pipelined requests or keep-alive, which is correct for the prototype but would need replacement at scale.

### Recommendations

1. **Add a grace period for re-registration.** When a `Register` arrives for an already-registered `server_id`, unregister the old entry and accept the new one. The old connection is dead (its `reply_tx` will fail on the next send). This eliminates the 5-second isolation window after a fast restart.

2. **Add a coordinator heartbeat visible to clients.** The `/assign` endpoint should return a `coordinator_uptime` or `last_topology_change` timestamp so clients can detect stale coordinator state.

3. **Consider coordinator redundancy for production.** The stateless design means a hot standby is trivial: run two coordinators, sim-servers connect to both, each maintains its own registry. Client assignment uses either. No consensus protocol needed because the coordinator's state is derived, not authoritative.

4. **Add basic authentication.** A shared secret (passed via environment variable) used as a bearer token on the WebSocket connection would prevent accidental mis-registration.

---

## 4. Crash Recovery

### Current Design

When the coordinator detects a dead server (heartbeat timeout or WebSocket disconnect), it:

1. Unregisters the dead server (`main.rs:228-229`)
2. Broadcasts an updated `PeerList` (dead server removed) (`main.rs:230`)
3. Broadcasts `RecoverEntities { dead_server_id, survivors }` to all surviving servers (`main.rs:232-233`)

Each survivor then:

1. Finds all shadow entities whose `AuthorityServer` matches the dead server (`systems.rs:499-504`)
2. Uses a deterministic hash (`nid.0.0.as_u128() as usize % survivor_count == my_index`) to decide which shadows it adopts (`systems.rs:502`)
3. Promotes adopted shadows from `Shadow` to `Authoritative` and sets `auth.0 = server_id` (`systems.rs:509-514`)

### Strengths

**Deterministic partitioning avoids coordination.** The UUID-based hash modulo survivor count ensures each shadow is adopted by exactly one survivor without any additional coordination. No leader election, no distributed consensus, no second round of messages. This is elegant.

**Shadows provide the recovery data.** Every server already maintains shadow copies of all entities from all peers. This means recovery data is pre-replicated to N-1 locations. The only overhead is shadow memory usage, which exists regardless of recovery.

**Immediate recovery.** Once the `RecoverEntities` message is processed (one tick), the entities are live on their new servers. There is no replay, no state reconstruction, no database query. Recovery latency is bounded by the coordinator's detection time (up to 5 seconds) plus one tick (50ms).

### Pitfalls and Failure Modes

**Shadow data can be up to 50ms stale.** Shadows are updated once per tick via UDP replication. When a server crashes mid-tick, its last replication may not have been sent yet. Survivors adopt the shadow state from the last received replication, which is at most one tick old. For position data at typical entity speeds (2-5 units/sec), this is 0.1-0.25 units of position error. Negligible.

**Shadow data can be up to 5 seconds stale if replication was lost.** If the dead server's replication packets were being dropped (e.g., network partition that preceded the crash), the shadows could be significantly stale. A server that was isolated for 4 seconds before crashing has shadows that are 4 seconds old. At 5 units/sec, that is 20 world units of position error. Noticeable but recoverable -- the entity snaps to its stale position and resumes from there.

**Player input state is partially lost.** The shadow's `last_input_seq` is replicated, but unprocessed inputs buffered on the dead server are lost. If a player was sending inputs at 20Hz and the server crashes, the last 1-2 inputs (50-100ms of movement) are lost. The client's prediction system will detect the mismatch and reconcile, causing a small position correction.

**Double-crash exposes a gap.** If two servers crash simultaneously (or within one rebalance interval), the first crash triggers recovery and distributes entities to survivors. If a survivor that just adopted entities also crashes before the next rebalance, those adopted entities have no shadows on the remaining servers (they were just promoted, not yet replicated). They are lost.

The probability of double-crash within a 50ms window is extremely low. Within a 5-second window (rebalance interval), it is plausible in a degraded environment (e.g., power supply issue affecting multiple UNO Q boards).

**Recovery message can be lost or delayed.** The `RecoverEntities` message is sent over WebSocket (JSON), which is TCP-based and reliable. But if the coordinator crashes after detecting a dead server but before broadcasting recovery, no recovery happens. The shadows go stale and are eventually cleaned up. The entities are lost.

**No verification that recovery completed.** After broadcasting `RecoverEntities`, the coordinator has no way to verify that survivors actually adopted the entities. There is no `RecoveryComplete` message. The coordinator trusts that the message was delivered and processed.

### Scalability Limits

Recovery cost is O(shadows_from_dead_server) per surviving server -- each survivor scans all its shadow entities to find those from the dead server. With S shadows per server and K survivors, total work is O(S * K). At 1,000 shadows per server and 10 survivors, this is 10,000 hash comparisons per recovery event. Trivial.

The real limit is the stale shadow window. With more entities moving faster, the 50ms staleness of shadows becomes more noticeable. With complex entity state (inventory, health, buffs), 50ms of data loss could be meaningful.

### Recommendations

1. **Add a `RecoveryComplete` report.** Each survivor reports how many entities it adopted. The coordinator logs and verifies the total matches the dead server's last reported entity count.

2. **Consider a brief "recovery hold" on shadow cleanup.** After a crash, increase `SHADOW_STALE_TICKS` for the dead server's entities for 30 seconds, giving time for the new authority to replicate its adopted entities back to peers.

3. **For player entities, combine with persistence flush.** If the shadow is missing or too stale, fall back to the coordinator's SQLite data (planned persistence). This provides a second tier of recovery with up to 5 seconds of data loss instead of total loss.

---

## 5. Rebalance Algorithm

### Current Design

The coordinator runs `try_rebalance()` (`registry.rs:122-180`) every 5 seconds with a 5-second cooldown. The algorithm:

1. Collects entity counts from all sim-servers (from heartbeat data)
2. Finds the most overloaded and most underloaded servers
3. Checks if the imbalance exceeds thresholds: `diff >= 10` AND `diff >= avg * 0.2`
4. If so, sends a `RebalanceDirective` to the overloaded server with `entity_ids: Vec::new()` (empty -- server picks which entities to transfer) and `target_server` (the underloaded server)

The sim-server's `process_transfers` (`systems.rs:585-658`) then:

1. Collects all authoritative NPC entities (players are excluded from rebalancing)
2. Sorts by distance from origin (descending -- transfer peripheral entities first)
3. Transfers up to 10% of total authoritative entities

### Strengths

**Players are excluded from rebalancing.** Only NPCs are transferred during rebalance (`systems.rs:611-613`). This prevents player entities from being disrupted by load balancing. Players only transfer during explicit drain operations.

**Spatial awareness in transfer selection.** Sorting by distance from origin and transferring peripheral entities first (`systems.rs:616`) is a reasonable heuristic. It tends to keep spatially clustered entities together on the same server, reducing cross-server interactions.

**Conservative transfer cap.** Transferring at most 10% of entities per rebalance cycle (`systems.rs:619`) prevents oscillation. Even if the algorithm runs every 5 seconds, it converges gradually.

### Pitfalls and Failure Modes

**Only pairwise rebalancing.** The algorithm only considers the single most overloaded and single most underloaded server. With 3+ servers, it cannot perform multi-target rebalancing. If S1 has 100 entities, S2 has 50, and S3 has 10, the algorithm transfers from S1 to S3, but it would be more efficient to also transfer from S2 to S3 simultaneously.

**`entity_ids` field is always empty.** The `RebalanceDirective.entity_ids` field exists in the protocol (`protocol.rs:120`) but is always sent as an empty Vec (`registry.rs:171`). The sim-server ignores it entirely (`systems.rs:587`). This is a dead code path. The protocol suggests the coordinator should specify which entities to transfer, but it never does.

**No hysteresis.** An entity transferred from S1 to S3 could be transferred back on the next rebalance cycle if S3 becomes overloaded. The 5-second cooldown and 10% cap provide some protection, but there is no per-entity transfer cooldown. An entity could bounce between servers indefinitely if the load pattern oscillates.

**Rebalance frequency is fixed.** The 5-second interval and 5-second cooldown are hardcoded. Under rapid load changes (e.g., a burst spawn of 500 entities on one server), it takes 5 seconds to notice and 5 more seconds before the next rebalance. With a 10% transfer cap, clearing 500 entities takes approximately 5 cycles (25 seconds) if the imbalance remains above the threshold.

**Rebalance during crash recovery can conflict.** If a server crashes and recovery distributes entities unevenly (due to the hash-based partitioning), the rebalance algorithm will try to fix the imbalance. But this means entities that were just recovered are immediately transferred again, potentially before they are stable on their new server.

**Last-writer-wins on directives.** If the coordinator sends two `RebalanceDirective` messages before the sim-server processes the first one, only the last one survives (`systems.rs:468-471` stores into `PendingTransfers.transfers` which is an `Option` -- the second write replaces the first). Flagged in audit finding #012.

### Scalability Limits

The rebalance algorithm is O(N) where N is the number of servers (finding max/min). The transfer selection on the sim-server is O(E log E) where E is the entity count (sorting). Both are fast.

The throughput limit is 10% of entities per 5-second cycle. With 1,000 entities on one server, it takes about 25 seconds to fully rebalance (5 cycles * 100 entities/cycle). This is adequate for gradual load changes but slow for sudden spikes.

### Recommendations

1. **Add per-entity transfer cooldown.** Track `last_transferred_tick` per entity (or use a HashSet of recently-transferred entity IDs with a 30-second TTL). Skip recently-transferred entities during transfer selection.

2. **Multi-target rebalancing.** When the imbalance involves multiple overloaded servers, issue directives to each one targeting different underloaded servers. This is a straightforward extension of the current algorithm.

3. **Increase transfer cap during recovery.** After a crash, allow up to 50% transfer for 30 seconds to quickly rebalance the recovery-induced imbalance.

4. **Wire up the `entity_ids` field.** Either use it for coordinator-directed transfers or remove it from the protocol to avoid confusion.

---

## 6. Client Connection Model

### Current Design

Clients connect to the coordinator's `/assign` endpoint (`coordinator/main.rs:525-546`) to get a WebSocket URL for the least-loaded sim-server. They connect directly to that sim-server, which spawns a player entity and begins sending binary snapshot frames every tick.

Each client connection is handled by a dedicated tokio task (`network.rs:79-204`) with a per-client mpsc channel (`PER_CLIENT_CHANNEL_SIZE = 8` in `resources.rs:152`). The game loop pushes frames to each client's channel; the client task reads from the channel and writes to the WebSocket.

On disconnect, the client task pushes a `RemoveEntity` message and an `unregister_clients` event to the inbound queue (`network.rs:192-198`).

### Strengths

**Per-client channels decouple the game loop from network I/O.** The game loop never touches a WebSocket. It pushes frames to bounded mpsc channels, and dedicated per-client tasks handle the actual network writes. This means a slow client cannot block the game loop -- the channel absorbs up to 8 frames (400ms at 20Hz), and after that, frames are dropped with a warning (`systems.rs:846-849`).

**Delta compression reduces per-client bandwidth.** Each client has its own baseline and receives delta frames (`systems.rs:856-882`). With 500 entities where only 50 change per tick, the delta frame is ~10x smaller than a full snapshot. Full snapshots are sent periodically (every 100 ticks / 5 seconds) as a convergence guarantee.

**Clean disconnect handling.** On WebSocket close, the client task sends both a `RemoveEntity` (to despawn the player) and an `unregister_clients` event (to clean up the ClientRegistry). The entity removal is forwarded to the authority server if the player was transferred (`systems.rs:373-419`).

### Pitfalls and Failure Modes

**No reconnect support.** If the WebSocket connection drops, the client gets a new connection, a new player entity, and starts from scratch. There is no session resumption. The old player entity is removed on the old connection's disconnect, and a new one is spawned on the new connection. The planned persistence design addresses this.

**`Reconnect` message not implemented on the server.** The protocol defines `ServerMessage::Reconnect { url }` (`protocol.rs:24-26`), but I found no code that sends it. When a player's authority is transferred to another server, the client remains connected to the original server. The original server continues to broadcast the entity (now as a shadow), but inputs must be forwarded. The client does not know to reconnect to the new authority. This works but adds latency (forwarded input = extra hop).

**Client assignment during instability.** If a server crashes and the coordinator has not yet processed the disconnect (up to 5 seconds), `/assign` may send new clients to the dead server. The client will fail to connect, get no server, and be stuck. There is no retry-with-different-server logic on the client side.

**No rate limiting on client input.** A client can send `MoveInput` messages at any rate. At 60 FPS with movement keys held, that is 60 messages per second per client. Each one acquires the inbound queue lock (`network.rs:163-165`), is deserialized from JSON, and processed. With 50 clients sending at 60 Hz, that is 3,000 messages per second. The JSON parsing and lock contention become measurable. Flagged in audit finding #011.

**Per-client delta computation is O(N * C).** Each tick, the server computes a delta for each client against that client's baseline. With N entities and C clients, the cost is O(N * C) per tick. All clients that have the same baseline (common case after a full snapshot) could share a single delta computation. This optimization is not implemented. Flagged in audit finding #020.

### Scalability Limits

| Clients | Per-tick frame cost | Channel memory |
|---------|-------------------|----------------|
| 1 | 1 delta computation | 8 frames |
| 10 | 10 delta computations | 80 frames |
| 50 | 50 delta computations | 400 frames |
| 100 | 100 delta computations | 800 frames |

The per-client baseline HashMap uses approximately 48 bytes per entity (EntityId + CompactEntityState). With 500 entities and 50 clients, that is 50 * 500 * 48 = 1.2 MB of baseline memory. Manageable.

The real bottleneck is the O(N * C) delta computation inside the ECS schedule. At 500 entities and 50 clients, that is 25,000 comparisons per tick, which at 20Hz means 500,000 comparisons per second. On desktop hardware, this is sub-millisecond. On a UNO Q board, it is measurable.

### Recommendations

1. **Share delta computation across clients with identical baselines.** Track a "generation number" on baselines. When multiple clients have the same generation (because they all received the same full snapshot at the same tick), compute the delta once and share it.

2. **Add client-side reconnect logic.** When the WebSocket drops, the client should retry `/assign` and reconnect. With persistence, it should include its `account_id` to resume as the same player.

3. **Implement the `Reconnect` message.** When a player's authority transfers, send `Reconnect { url: new_server_ws_url }` to the client. The client disconnects from the old server and connects to the new one. This eliminates the forwarded input hop.

4. **Add rate limiting on client input.** Process at most N `MoveInput` messages per client per tick. Drop excess messages silently.

---

## 7. Binary Protocol

### Current Design

Server-to-client communication uses a custom binary protocol over WebSocket binary frames (`encoding.rs`). Two frame types:

- **Full snapshot** (0x01): 27-byte header + 47 bytes per entity
  - Header: `[type:u8][tick:u64 LE][entity_count:u32 LE][metrics:14 bytes]`
  - Entity: `[uuid:16][pos:3xf32 LE][vel:3xf32 LE][type:u8][authority:u16 LE][last_input_seq:u32 LE]`

- **Delta** (0x02): 31-byte header + 47 bytes per updated entity + 16 bytes per removed entity UUID
  - Header: `[type:u8][tick:u64 LE][update_count:u32 LE][removal_count:u32 LE][metrics:14 bytes]`

Client-to-server communication uses JSON over WebSocket text frames (serde-tagged enums).

Client-to-server also supports a binary tick ACK: `[type:0x03][tick:u64 LE]` (9 bytes, `network.rs:173-178`).

Server-to-server communication uses bincode over UDP.

### Strengths

**Fixed-size entity encoding is fast.** No variable-length fields, no delimiters, no escaping. The encoder (`encoding.rs:8-28`) is a straight sequence of `extend_from_slice` calls. The decoder (`binary-protocol.ts:46-63`) uses `DataView.getFloat32/getUint16/getUint32` at computed offsets. Both are cache-friendly and branch-free.

**Endianness is explicit.** Little-endian is used consistently on both sides. Rust uses `.to_le_bytes()`, TypeScript uses `DataView.get*(offset, true)` (the `true` flag means little-endian). This avoids the classic endianness mismatch bug.

**47-byte entity size is compact.** UUID (16) + position (12) + velocity (12) + type (1) + authority (2) + seq (4) = 47 bytes. No wasted padding, no alignment waste.

### Pitfalls and Failure Modes

**No protocol version.** There is no version byte in the frame header. If the entity format changes (e.g., adding a health field), old clients will misparse new frames. The only recovery is for the client to detect that entity data looks wrong and request a full page reload.

**No frame length validation on the client.** The client decoder (`binary-protocol.ts:91-105`) reads `entityCount` from the header and then blindly reads `entityCount * 47` bytes. If the server sends a truncated frame (e.g., due to a buffer bug or interrupted write), the decoder will read past the buffer boundary. JavaScript's `DataView.getFloat32` throws a `RangeError` when reading out of bounds, which would be caught as a decoding failure. But there is no pre-check: `if (buffer.byteLength < 27 + entityCount * 47) return null`.

**u64 tick as two u32 in JavaScript.** The tick is encoded as a u64 LE on the server. JavaScript does not have native u64 support, so the client decodes it as `tickLow + tickHigh * 0x100000000` (`binary-protocol.ts:93-95`). This is correct for ticks up to 2^53 (Number.MAX_SAFE_INTEGER), which at 20Hz is ~14 million years. Not a practical concern, but the comment-free `0x100000000` magic number could confuse future maintainers.

**14-byte metrics placeholder is always zero.** The header contains a 14-byte metrics section (`encoding.rs:41`, `encoding.rs:63`) that is always written as zeros. The client decodes it (`binary-protocol.ts:65-72`) into a `GatewayMetrics` struct with `snapshotsPerSec`, `bandwidthKBps`, `rebalanceEvents`, and `serverCount`, all of which are always zero. This is dead space. Either populate it or remove it.

**Entity count uses u32, limiting to ~4 billion.** Not a practical concern.

**The 47-byte entity size means frame size = 27 + 47N.** At 1,000 entities, the frame is 47,027 bytes. WebSocket frames have no practical size limit (the protocol supports up to 2^63 bytes), but the browser must buffer the entire frame before the `onmessage` handler fires. At 1,000 entities * 20Hz, the client receives ~940 KB/s of binary data. This is manageable.

### Backwards Compatibility

There is none. Any change to entity size breaks all clients immediately. The 14-byte metrics header provides a small extension point, but it is already structurally defined. Adding a field to entities requires:

1. Changing `ENTITY_SIZE` in both Rust and TypeScript
2. Updating the encoder and decoder
3. Coordinating the deployment of server and client simultaneously

### Scalability Limits

| Entities | Full snapshot size | At 20Hz |
|----------|-------------------|---------|
| 100 | 4.7 KB | 94 KB/s |
| 500 | 22.0 KB | 440 KB/s |
| 1,000 | 47.0 KB | 940 KB/s |
| 5,000 | 235 KB | 4.7 MB/s |

With delta compression, the effective bandwidth depends on the change rate. With 10% of entities changing per tick, the delta frame is ~10x smaller. At 500 entities with 50 changing: 2.2 KB per delta frame, 44 KB/s per client. This is well within browser capabilities.

### Recommendations

1. **Add a version byte.** Use the first byte as a compound: high nibble = protocol version, low nibble = frame type. Or use byte 0 as version and byte 1 as frame type. This costs 1 byte per frame and enables backwards-compatible evolution.

2. **Add frame length validation on the client.** Before decoding entities, check `buffer.byteLength >= headerSize + entityCount * ENTITY_SIZE`. Return null on mismatch.

3. **Populate or remove the metrics placeholder.** If server-side metrics will be sent to the client (useful for the debug overlay), populate it. If not, remove the 14 bytes from the header to save ~280 bytes/s per client at 20Hz.

---

## 8. Player Persistence Design

### Design Overview (from `docs/player-persistence-design.md`)

The planned persistence model uses the coordinator as the player data authority, backed by SQLite. The protocol is check-out/check-in:

- **Login:** Client sends `account_id` (from localStorage) to `/assign`. Coordinator looks up or creates the account in SQLite, picks a target server, marks the account as "checked out," and returns a spawn position + server URL.
- **Playing:** Sim-server flushes dirty player state to coordinator every 5 seconds via `PlayerFlush`.
- **Disconnect:** Sim-server sends `PlayerCheckIn` with final state before deleting the entity.
- **Authority transfer:** Source sends `PlayerCheckIn`, target sends `PlayerCheckOut` after receiving the entity.
- **Crash recovery:** Coordinator knows which accounts were checked out to the dead server. Survivors adopt shadows; coordinator updates `checked_out_to`.

### Strengths

**Leverages existing infrastructure.** The coordinator already has a WebSocket connection to every sim-server and a long-lived process lifetime. Adding SQLite and a few message types is minimal new infrastructure. No Redis, no Postgres, no external dependency.

**Clean separation of hot path and critical path.** Position updates (frequent, tolerant of loss) are batched and async. Login/logout/gameplay events (rare, must not be lost) are synchronous. This is the right architecture.

**Recovery has two tiers.** Shadow-based recovery provides sub-second failover with minimal data loss. SQLite-based recovery provides a fallback if shadows are unavailable, with up to 5 seconds of data loss. Two tiers is better than one.

**SQLite on the coordinator is the right choice.** At the scale of this prototype (3-10 servers, 50 players), SQLite in WAL mode handles the write volume trivially. The upgrade path (swap backend to Postgres) is explicitly called out.

### Pitfalls and Failure Modes

**The `checked_out_to` field is a distributed lock without a release mechanism on coordinator crash.** If the coordinator crashes while a player is checked out, the `checked_out_to` field is left set in SQLite. When the coordinator restarts, it sees the account as checked out to a server that may or may not still be running. If the server is still running, the player continues playing normally -- the coordinator just needs to re-establish the check-out when the server re-registers. If the server also crashed, the coordinator needs to detect this (via heartbeat timeout) and forcibly check the player back in.

The design document acknowledges this: "Coordinator crash -- None, SQLite WAL survives process crash." But it does not address the stale `checked_out_to` problem. The coordinator must, on startup, scan for accounts checked out to servers that are not currently registered, and forcibly check them in using the last flushed state.

**The 5-second flush interval creates a data loss window that varies by data type.** Position is flushed every 5 seconds -- losing 5 seconds of position is acceptable (the design doc says so, and I agree). But the `gameplay_json` blob (health, inventory, etc.) is described as "event-driven, synchronous" in the write frequency table. If the sim-server processes a gameplay event (player picks up item, takes damage) and then crashes before the synchronous write completes, that event is lost. The design should clarify: are gameplay events flushed immediately (within the same tick), or queued for the next flush cycle?

**Authority transfer during drain has a race condition.** The design says:

> Before transferring a player entity, sim-server sends `PlayerCheckIn` to coordinator.
> Target server receives entity via UDP transfer as today.
> Target server sends `PlayerCheckOut` to coordinator to claim ownership.

Between the check-in and the check-out, the player is "checked in" (not assigned to any server). If the coordinator receives a new login for this account during this window, it would spawn a duplicate player. The window is small (the time between sending check-in and the target sending check-out), but it exists.

The fix is to not check in during transfer. Instead, use a `PlayerTransfer { account_id, from_server, to_server }` message that atomically updates `checked_out_to` from one server to another, validated by the coordinator.

**`account_id` in localStorage is fragile.** localStorage can be cleared by the user, by the browser (storage pressure), or by opening in incognito mode. If the `account_id` is lost, the player starts as a new account. Their old account remains checked out until the server disconnects it (or the 5-second heartbeat timeout). This is not a bug per se, but the design should handle it: if a new `account_id` is presented while the old one is still checked out, do not create a duplicate -- return an error or generate a new account.

**No account authentication.** Anyone can claim any `account_id`. If player A knows player B's `account_id` (e.g., from a shared URL or localStorage inspection), they can log in as player B. This is fine for the prototype but must be addressed before any multi-player deployment.

**Coordinator becomes a bottleneck for player flush.** With 50 players flushing every 5 seconds, the coordinator receives 10 `PlayerFlush` messages per second, each requiring a SQLite write. At 1ms per write, this is 10ms/s -- negligible. But if the flush interval is reduced to 1 second (as suggested in the design doc for tighter guarantees), it becomes 50 writes/s = 50ms/s. Still fine for SQLite in WAL mode, but it means the coordinator's WebSocket handler must process these messages efficiently. The current `run_registry` function (`main.rs:198-259`) uses a single `tokio::select!` loop -- all coordinator message processing is serialized. A burst of flush messages could delay heartbeat processing.

### The `checked_out_to` as Distributed Lock

This deserves special attention because it is the core correctness mechanism.

The `checked_out_to` field functions as a distributed lease. The coordinator grants a lease (check-out) to a sim-server, and the sim-server holds it until check-in (disconnect), transfer, or crash recovery. This is a well-known pattern with well-known failure modes:

1. **Lease holder crashes without release.** Handled by the coordinator detecting the dead server and forcibly releasing the lease. Correct.

2. **Network partition between coordinator and sim-server.** The sim-server thinks it holds the lease and continues simulating. The coordinator thinks the server is dead (heartbeat timeout) and releases the lease, allowing another server or a new login to claim the account. Now two servers (or a server and a new login) hold the same account. This is the classic split-brain problem with leases.

   Mitigation: The heartbeat timeout (5 seconds) is long enough that transient network issues do not trigger false positives. A true network partition lasting longer than 5 seconds will cause the coordinator to mark the server as dead and initiate recovery, which includes updating `checked_out_to`. The sim-server, now isolated, continues simulating a ghost player. When the partition heals, the sim-server reconnects to the coordinator and re-registers. The coordinator needs to detect that this server has entities for accounts that have been reassigned, and instruct the server to drop them.

3. **Coordinator processes check-out for server A, then server A crashes before acknowledging.** The account is marked as checked out to A, but A never created the entity. The coordinator will eventually detect A as dead and forcibly check the account back in. The player experiences a login failure followed by a 5-second wait before they can retry.

### Recommendations

1. **Startup reconciliation.** On coordinator startup, scan `players` table for accounts where `checked_out_to IS NOT NULL`. For each, check if the corresponding server is registered. If not, set `checked_out_to = NULL` using the last flushed state.

2. **Atomic transfer message.** Replace the check-in/check-out sequence during transfers with a single `PlayerTransfer { account_id, from_server, to_server }` message. The coordinator validates `checked_out_to == from_server` and atomically updates to `to_server`. This eliminates the race window.

3. **Lease expiry.** Add a `lease_expires_at` timestamp to the `players` table. The sim-server must renew the lease periodically (via the heartbeat or flush). If the coordinator detects an expired lease, it forcibly checks the account in. This handles the split-brain case -- the isolated server's lease expires, and it must re-acquire it when connectivity resumes.

4. **Handle stale `account_id` gracefully.** If a login request arrives for an account that is already checked out, return an error with a message like "account already in use." The client should handle this by generating a new `account_id`.

---

## Cross-Cutting Concerns

### Async-Sync Bridge

The sim-server uses a dual-buffer pattern to bridge async I/O (WebSocket, UDP, coordinator) with the synchronous ECS game loop. Async tasks push events into `SharedInbound` and `SharedCoordEvents` (protected by `tokio::Mutex`). The game loop acquires these locks at the start of each tick (`game_loop.rs:91-129`) and swaps the contents into ECS resources using `std::mem::take`.

This pattern has two known issues:

1. **Lock contention.** The game loop holds the inbound lock for the duration of the swap. If a UDP packet arrives during the swap, the receiver task blocks on the lock. At 20Hz with sub-millisecond swap times, the contention window is tiny. But the lock is a `tokio::Mutex`, which means the game loop (an async function) yields while acquiring it. This is correct but means the game loop participates in the tokio scheduler during lock acquisition, adding unpredictable latency. Flagged in audit finding #003.

2. **No data loss on the current design.** The `std::mem::take` pattern atomically replaces the buffer contents with an empty buffer. Events accumulated since the last tick are moved into the ECS in one shot. No events are overwritten or lost (this was the concern in audit finding #001, which applied to the pre-ECS design and has been mitigated by the swap pattern).

### Error Handling

The codebase consistently logs errors and continues rather than panicking. UDP send failures are logged as warnings (`game_loop.rs:179`). WebSocket send failures break the client loop (`network.rs:142`). Bincode deserialization failures are logged and skipped (`receiver.rs:26-28`). JSON parse failures on client input are logged and skipped (`network.rs:167-168`).

This is the right approach for a game server -- availability is more important than strict correctness. A single bad packet should not crash the server. But it does mean that errors can accumulate silently. The metrics system tracks timing but not error counts. Adding an error counter to the metrics would help with operational visibility.

### Test Coverage

The codebase has thorough unit tests for:
- Protocol serialization roundtrips (JSON and bincode)
- ECS component spawning and querying
- Schedule integration (multi-system tick cycles)
- Delta compression logic
- Binary encoding
- Registry operations (rebalance, recovery, peer list)

Missing test coverage:
- Network integration (actual WebSocket/UDP connections)
- Multi-server interaction (authority transfer between two running servers)
- Client reconnection and error recovery
- Concurrent access patterns (multiple async tasks pushing to the inbound queue)
- Crash recovery end-to-end (coordinator detects crash, broadcasts recovery, survivor adopts)

The `test.sh` and `test-scene.sh` scripts cover some of these at the integration level, which is appropriate. The missing unit-level coverage is a concern for regression safety.

---

## Summary of Priorities

| Area | Most Urgent Issue | Severity |
|------|------------------|----------|
| Authority model | Fire-and-forget transfer over UDP | Critical |
| UDP replication | 60KB hard cap at ~1000 entities | High |
| Coordinator | Stale registration after fast restart | Medium |
| Crash recovery | No completion verification | Medium |
| Rebalance | No hysteresis / per-entity cooldown | Medium |
| Client model | No reconnect support | Medium |
| Binary protocol | No version byte | Low (until first schema change) |
| Persistence | `checked_out_to` race during transfer | High (once implemented) |

The single most impactful fix is adding acknowledgments to authority transfers. Everything else is optimization or hardening. The system works at its current scale. These are the walls it will hit as it grows.
