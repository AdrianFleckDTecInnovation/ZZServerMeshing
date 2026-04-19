# Work Items 020: Player Migration on Rebalance

Design: `docs/mesh-architect/designs/016-churn-resilience-plan.md` (Batch 4).
Audit findings: `docs/mesh-auditor/074-churn-audit-summary.md` (080 + 086 + 091).

This batch wires up the player-migration primitive: rebalance can move
Player entities between servers, the client reconnects transparently to
the new server, and player identity is preserved across the hop.

**Open-question resolutions:**
- Q1 (target URL source) — **source server constructs locally** from a
  new `PeerWsAddresses` resource populated by `PeerList` updates.
  `PeerInfo` gains a `tls: bool` field so the scheme is unambiguous.
- Q3 (client state preservation) — **new protocol message
  `ClientMessage::ClaimPlayer { player_id }`**, sent by the migrating
  client as the first message after connecting to the target. Query
  strings rejected (visible in logs/proxies; weaker engineering). The
  target server adopts the pre-existing entity instead of spawning a new
  one, so the client's player_id is stable across the migration.

Implement in order. Each WI compiles independently; they build on each
other.

---

### WI-200: Protocol additions

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/shared/src/protocol.rs`
- `crates/shared/src/lib.rs` (bump `PROTOCOL_VERSION`)
- `crates/coordinator/src/registry.rs` (PeerInfo construction site)

**Description:**

1. **`PeerInfo.tls: bool`**. At `crates/shared/src/protocol.rs:185-189`:
   ```rust
   pub struct PeerInfo {
       pub server_id: ServerId,
       pub udp_addr: String,
       pub ws_addr: String,
       pub tls: bool,
   }
   ```
   Populate it in `crates/coordinator/src/registry.rs:514-518` from
   `s.tls` (already tracked on `ServerEntry`). Update the test PeerInfo
   constructors in `protocol.rs:427-434`, `sim-server/game_loop.rs:1237-1240`,
   `sim-server/systems/coordinator.rs:179-187, 263-268, 292-295, 301-309`
   to set `tls: false` for existing tests.

2. **`ClientMessage::ClaimPlayer { player_id: EntityId }`**. Add variant
   to the `ClientMessage` enum in `protocol.rs`. JSON-tagged per the
   existing `#[serde(tag = "type")]` convention — so the wire form is
   `{"type":"ClaimPlayer","player_id":"<uuid>"}`. Add a
   `client_message_claim_player_json_roundtrip` test.

3. **No change to `ServerMessage::Reconnect`** — it already has
   `url: String`. Confirm the existing JSON roundtrip test still passes.

4. **`PROTOCOL_VERSION` bump.** Going from 2 → 3. Update
   `crates/shared/src/lib.rs`. Note in a code comment that this version
   represents the client-server JSON protocol (the UDP peer protocol
   `PROTOCOL_VERSION` remains a separate constant at `shared/src/lib.rs`
   — verify only ONE is called `PROTOCOL_VERSION` and either repurpose
   it or split into `CLIENT_PROTOCOL_VERSION` and
   `PEER_PROTOCOL_VERSION`).

   **Actually, check first:** `PROTOCOL_VERSION` as defined today might
   be used for both. If so, the cleanest move is to leave the constant
   at 2 (peer protocol is unchanged in this batch) and add a new
   constant `CLIENT_PROTOCOL_VERSION: u32 = 1` (or similar) that the
   client handshake uses. Don't conflate the two.

**Tests:**
- `peer_info_tls_field_json_roundtrip` — include `tls: true` and `false`.
- `client_message_claim_player_json_roundtrip`.
- Existing `peer_list_json_roundtrip` updated to include `tls` field.

**Done when:**
- `cargo build --workspace` succeeds.
- `cargo test -p shared` passes.
- `cargo test -p coordinator registry::tests` passes (the new `tls`
  field needs to flow through the PeerInfo construction).

---

### WI-201: `ClientFrame` becomes an enum with `Binary` and `Text` variants

**Size:** medium
**Depends on:** nothing (independent of WI-200)
**Files:**
- `crates/sim-server/src/resources.rs:11` — `pub type ClientFrame = Vec<u8>;`
- `crates/sim-server/src/network.rs` — per-client frame dispatch loop
- `crates/sim-server/src/systems/snapshot.rs` — frame producer
- Any test that constructs `ClientFrame` directly

**Description:**

Today, `ClientFrame = Vec<u8>`. All server→client frames are sent as
binary WebSocket messages. For WI-203 we need to send
`ServerMessage::Reconnect { url }` as a JSON text frame mid-session.
Extend `ClientFrame` to carry the WebSocket message type:

1. In `resources.rs:11`:
   ```rust
   pub enum ClientFrame {
       Binary(Vec<u8>),
       Text(String),
   }
   ```

2. Every existing producer that pushes bytes (search the codebase for
   `ClientFrame` and for `frame_tx.send(`) constructs
   `ClientFrame::Binary(bytes)`. Most producers are in
   `systems/snapshot.rs`.

3. In `network.rs`, the per-client send loop currently (likely) sends
   `Message::Binary(bytes)`. Update to dispatch:
   ```rust
   match client_frame {
       ClientFrame::Binary(b) => ws_sink.send(Message::Binary(b.into())).await,
       ClientFrame::Text(s) => ws_sink.send(Message::Text(s.into())).await,
   }
   ```

4. Update any `ClientFrame::from(bytes)` or `ClientFrame::default()`
   usages accordingly. `Default` might not make sense on an enum with
   two variants — if that breaks a test, pick one (likely
   `Binary(Vec::new())`) for test purposes.

**Tests:**
- Any existing test that constructs ClientFrame needs a mechanical
  update. Don't add new ones here — WI-203's acceptance tests cover
  Text-frame dispatch.

**Done when:**
- `cargo build -p sim-server` and `cargo test -p sim-server` succeed.
- Behavior unchanged for existing binary-only traffic (verified by
  running a local cluster; entities still render in the browser).

---

### WI-202: `PeerWsAddresses` resource + target-side player claim

**Size:** medium
**Depends on:** WI-200 (needs `PeerInfo.tls`), WI-201 (needs
`ClientFrame::Text`)
**Files:**
- `crates/sim-server/src/resources.rs` — new resource
- `crates/sim-server/src/systems/coordinator.rs` — populate on PeerList
- `crates/sim-server/src/resources.rs` — extend `ClientHandle` with
  `migrating: bool`, `awaiting_claim: bool` (or equivalent state)
- `crates/sim-server/src/network.rs` — connect flow waits for
  ClaimPlayer; close flow respects migration flag
- `crates/sim-server/src/systems/input.rs` — handle `ClientMessage::ClaimPlayer`

**Description:**

Target-side: when a migrating client connects, associate it with the
incoming authoritative entity rather than spawning a new player.

1. **Add `PeerWsAddresses` resource** in `resources.rs`:
   ```rust
   #[derive(Resource, Default)]
   pub struct PeerWsAddresses(pub HashMap<u16, String>);
   ```
   The value is the full URL including scheme: `"wss://host:port"` or
   `"ws://host:port"`. Populate alongside `PeerAddresses` in
   `systems/coordinator.rs` when processing `peer_list_updates`:
   ```rust
   let scheme = if peer.tls { "wss" } else { "ws" };
   let url = format!("{scheme}://{}", peer.ws_addr);
   world.resource_mut::<PeerWsAddresses>().0.insert(peer.server_id.0, url);
   ```

2. **Extend `ClientHandle`** in `resources.rs:214`:
   ```rust
   pub struct ClientHandle {
       ...existing...
       /// True if this client connection is migrating in from another
       /// server. Set at connect time when ClaimPlayer is received.
       /// Used to suppress the "spawn new player" flow.
       pub awaiting_claim: bool,
       /// True if this client is migrating OUT (source server sent
       /// Reconnect, expects client to disconnect). Close handler skips
       /// RemoveEntity when set. Reset at client_id scope — not
       /// persistent across reconnects.
       pub migrating_out: bool,
   }
   ```
   Both default to false.

3. **Modify `network.rs` connect flow** — replace the
   eager-spawn-player pattern:
   - Today: connect → spawn new Player entity → send PlayerAssigned.
   - New: connect → wait up to 500ms for the first ClientMessage:
     - If `ClaimPlayer { player_id }`: set `awaiting_claim = true`,
       record the claim, wait up to 2000ms (poll each tick via
       `EntityLookup`) for the entity to exist authoritatively.
       If found: associate client with it, send
       `PlayerAssigned { player_id }` (same UUID). If not found
       within 2s: log a warning, spawn a new player as fallback.
     - If any other message (or timeout after 500ms): spawn a new
       player as today. Queue the received message (if any) for
       processing after the spawn.

   Implementation note: the 500ms wait can be a `tokio::select!` on
   `frame_rx.recv()` vs `tokio::time::sleep`. The 2000ms claim-wait
   is a polling loop that re-checks `EntityLookup` each tick (20Hz →
   40 iterations max).

4. **Handle `ClientMessage::ClaimPlayer`** in `systems/input.rs`:
   the variant already exists in the enum after WI-200. Process it in
   the same switch as other client messages. Most of the claim work
   is in `network.rs`, not here; this handler is a no-op after the
   connect-time handshake completes. If it arrives later
   (out-of-order or duplicate), log at debug and ignore.

5. **Close handler respects `migrating_out`**: at `network.rs:270-280`,
   conditionally skip the RemoveEntity push:
   ```rust
   let migrating = world_ref.read()... // get from ClientHandle if migrating_out
   if !migrating {
       q.client_inputs.push((client_id, ClientMessage::RemoveEntity { entity_id: player_id }));
   } else {
       tracing::info!(client_id, "Client migrating out — skipping RemoveEntity on close");
   }
   q.unregister_clients.push(client_id);
   ```
   The `migrating_out` flag is set by the source server in WI-203 when
   it sends Reconnect. Reading from `ClientHandle` requires either a
   channel or shared state since `network.rs` doesn't have ECS World
   access directly — pass the flag via a small `Arc<AtomicBool>` stored
   in `ClientHandle` (or a per-client oneshot).

   **Simplest approach:** `ClientHandle.migrating_out:
   Arc<AtomicBool>`. The source-side migration code in WI-203 flips
   it; the close handler reads it.

**Tests:**
- `coord_peer_list_populates_ws_urls`: PeerList with tls=true and
  tls=false produces correct `wss://` and `ws://` urls.
- `claim_player_adopts_existing_entity`: target has a Player entity
  that arrived via AuthorityTransfer; a client connects and sends
  ClaimPlayer with the matching id; client is associated without
  spawning a new entity. Verify EntityOwnership and player_id on the
  ClientHandle.
- `claim_player_timeout_falls_back_to_spawn`: target has no such
  entity; client sends ClaimPlayer with a random UUID; after
  ~2s, a new player is spawned and PlayerAssigned is sent with the
  NEW UUID (client must adapt). Log a warn.
- `claim_player_suppresses_removeentity_on_close`: source server flow
  (tested in isolation): a client with `migrating_out=true` closes;
  verify no RemoveEntity message is enqueued.

**Done when:**
- All three unit tests pass.
- `cargo test -p sim-server` passes.
- Local cluster still connects/disconnects clients normally for
  non-migration flows (smoke test via `./start.sh` + browser).

---

### WI-203: Source-side player migration (`process_transfers`)

**Size:** medium
**Depends on:** WI-200 + WI-201 + WI-202
**Files:**
- `crates/sim-server/src/systems/transfers.rs` — extend `process_transfers`
- `crates/sim-server/src/systems/input.rs` (already handles `RemoveEntity` during transfer — verify it still works)

**Description:**

Today `process_transfers` at `transfers.rs:175-191` filters to NPCs only:
```rust
if kind.0 == EntityType::Npc && world.get::<TransferOutbound>(entity).is_none() {
    npc_candidates.push(...);
}
```

1. **Collect both NPC and Player candidates.** Change to:
   ```rust
   let mut candidates: Vec<(Entity, EntityId, EntityType, f32)> = Vec::new();
   let mut query = world.query_filtered::<(Entity, &NetworkId, &Position, &EntityTypeComp), With<Authoritative>>();
   for (entity, nid, pos, kind) in query.iter(&world) {
       total_auth += 1;
       if world.get::<TransferOutbound>(entity).is_none() {
           candidates.push((entity, nid.0, kind.0, pos.0.length_squared()));
       }
   }
   ```

2. **Sort: NPCs first (by descending distance), then Players (by
   descending distance).** NPCs are cheaper to migrate (no client
   handshake), so prefer them. Players come second. Use a tuple key
   like `(is_player_as_u8, -distance)`:
   ```rust
   candidates.sort_by(|a, b| {
       let a_is_player = matches!(a.2, EntityType::Player) as u8;
       let b_is_player = matches!(b.2, EntityType::Player) as u8;
       a_is_player.cmp(&b_is_player)  // NPCs (0) before Players (1)
           .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
   });
   ```

3. **For each migration candidate**, if it's a Player, find the
   client handle with matching `player_id`, send `Reconnect`, and flip
   `migrating_out`:
   ```rust
   if matches!(kind, EntityType::Player) {
       let target_url = match world.resource::<PeerWsAddresses>().0.get(&target_server.0) {
           Some(u) => u.clone(),
           None => {
               tracing::warn!("no ws url for target {target_server}, skipping player migration");
               continue;  // Don't migrate this entity
           }
       };
       let reconnect_json = match serde_json::to_string(&ServerMessage::Reconnect { url: target_url }) {
           Ok(j) => j,
           Err(_) => continue,
       };
       // Find client by player_id
       let mut registry = world.resource_mut::<ClientRegistry>();
       let client_entry = registry.clients
           .iter_mut()
           .find(|(_, h)| h.player_id == Some(*eid));
       if let Some((_cid, handle)) = client_entry {
           if let Err(e) = handle.tx.try_send(ClientFrame::Text(reconnect_json)) {
               tracing::warn!(?e, "failed to send Reconnect to migrating client");
               continue;
           }
           handle.migrating_out.store(true, std::sync::atomic::Ordering::Release);
       } else {
           tracing::warn!(entity = %eid, "Player has no client — skipping migration");
           continue;
       }
   }
   ```

4. **Then send the AuthorityTransfer UDP message** (identical to the
   NPC path). Mark entity `TransferOutbound`. The rest of the flow
   (ACK, despawn on ack, etc.) is unchanged.

5. **Ordering matters:** Reconnect MUST be sent BEFORE the
   AuthorityTransfer so the client has time to close its WS. If the
   AuthorityTransfer is in the outbound UDP queue, `try_send` on
   ClientFrame is synchronous from the ECS system's perspective, so
   ordering within one tick is guaranteed. Document this with a
   comment.

**Tests:**
- `process_transfers_includes_players_when_no_ws_url_skips`: player
  entity with no PeerWsAddresses entry for target → no migration.
- `process_transfers_player_sends_reconnect_first`: set up a client
  with player_id mapped, PeerWsAddresses populated; run
  `process_transfers`; assert ClientFrame::Text(reconnect JSON) was
  enqueued AND migrating_out is true AND TransferOutbound is set.
- `process_transfers_player_no_client_skips`: player entity exists
  but no client has that player_id; assert it is NOT migrated (the
  client migration handshake can't work without a client).
- `process_transfers_prefers_npcs_over_players`: mixed candidate
  list; verify NPCs are migrated first until the transfer budget
  is exhausted.

**Done when:**
- Unit tests pass.
- `cargo test -p sim-server` passes.

---

### WI-204: Client sends `ClaimPlayer` on server-directed reconnect

**Size:** small
**Depends on:** WI-200
**Files:**
- `client/src/network.ts`

**Description:**

Batch 2 introduced `serverDirectedUrl: string | null` on `NetworkClient`.
Extend the Reconnect handler to ALSO remember the current `playerId`,
and have the new WS connection send `ClaimPlayer` as its first message.

1. Add a `pendingClaim: string | null` field to `NetworkClient`
   (initialize to null).

2. In the Reconnect handler at `network.ts:272-278`:
   ```typescript
   } else if (msg.type === 'Reconnect') {
     console.log('Server requesting reconnection to:', msg.url);
     this.serverDirectedUrl = msg.url;
     this.pendingClaim = this.playerId;  // Snapshot player_id before close
     this.url = msg.url;
     if (this.ws) {
       this.ws.close();  // triggers reconnect via onclose
     }
   }
   ```

3. In `connect()` (`network.ts:100`), after `this.ws.onopen`, if
   `this.pendingClaim` is set, send the claim message immediately on
   open and clear the pending:
   ```typescript
   this.ws.onopen = () => {
     console.log('Connected to server');
     this.connected = true;
     this.reconnectDelay = NETCODE.RECONNECT_DELAY_MS;
     if (this.pendingClaim) {
       console.log(`Claiming player ${this.pendingClaim.substring(0, 8)}`);
       this.send({ type: 'ClaimPlayer', player_id: this.pendingClaim });
       this.pendingClaim = null;
     }
   };
   ```

4. Keep `this.playerId` set to the pre-migration value until the new
   server sends `PlayerAssigned`. The target server will send the same
   UUID if the claim succeeds. If it sends a different UUID (fallback
   to spawn), the client adapts via the existing
   PlayerAssigned handler at `network.ts:268-271` which overwrites
   `this.playerId`.

5. The `PredictionManager` state (input seq counter, pending inputs)
   SHOULD be preserved across the migration. The current
   `PredictionManager` is a plain object that persists across
   `connect()` cycles (it's only constructed once in `NetworkClient`'s
   constructor). Verify — if the next tick's ACK arrives with
   `ackedSeq` lower than `this.prediction.seq`, the reconcile path
   will still work because it drops "already acknowledged" inputs. The
   server's `LastInputSeq` component is carried over via
   `AuthorityTransfer.full_state`, so the target server knows the seq
   and confirms it back. Document this with a code comment.

**Tests:**
- No Vitest. Verification via the integration scenarios in WI-206.

**Done when:**
- `cd client && npx tsc --noEmit` passes.
- Manual test on local cluster works (see WI-206 for full checklist).

---

### WI-205: Scale rebalance threshold for small meshes

**Size:** small
**Depends on:** nothing
**Files:**
- `crates/coordinator/src/registry.rs:410-415`

**Description:**

Current threshold at `registry.rs:412-415`:
```rust
let diff = overloaded.weighted_load as i32 - underloaded.weighted_load as i32;
if diff < 10 || (diff as f32) < avg * 0.2 {
    return false;
}
```

Replace with a size-aware floor:
```rust
let diff = overloaded.weighted_load as i32 - underloaded.weighted_load as i32;
let floor = if avg < 50.0 {
    (avg * 0.3).max(2.0) as i32
} else {
    10
};
if diff < floor || (diff as f32) < avg * 0.2 {
    return false;
}
```

At `avg = 5.0`, floor = `max(1.5, 2.0) = 2` — so a 2-server, 3-vs-0
weighted difference triggers rebalance.
At `avg = 20.0`, floor = `max(6.0, 2.0) = 6`.
At `avg = 50.0`, we flip to the 10-unit floor.
At `avg ≥ 50.0`, current behavior preserved.

**Tests:**
- `rebalance_small_mesh_triggers_at_low_threshold`: 2-server registry,
  S1 = 6 weighted, S2 = 0 weighted (avg = 3). Verify
  `try_rebalance` returns true.
- `rebalance_small_mesh_below_floor_no_fire`: 2-server registry, S1 =
  3 weighted, S2 = 2 weighted (diff = 1, floor = max(1.5, 2) = 2).
  Verify `try_rebalance` returns false.
- **Keep** the existing `rebalance_below_minimum_threshold` test at
  `registry.rs:655` but note its inputs now test the large-mesh branch
  — update it to use avg ≥ 50 if it currently uses smaller values.

**Done when:**
- `cargo test -p coordinator registry::tests` passes.

---

### WI-206: Board verification

**Size:** small
**Depends on:** WI-200 through WI-205
**Files:** none (verification only)

**Description:**

Deploy via `./start-unoq.sh`. Run the following scenarios:

1. **Player migration under load:**
   - Open two browser tabs pointing at the coordinator. Each tab
     connects to a different sim-server (load-balanced by `/assign`).
     Wait for both players to be visible.
   - Force an imbalance: open more tabs until one server has 2-3
     players and another has 0.
   - Observe `/dashboard` — the coordinator should issue a rebalance
     directive within 5s (enabled by WI-205 threshold).
   - Affected tab's console shows `Server requesting reconnection to:
     wss://...` and `Claiming player abc12345...`.
   - Player entity on the new server — verify same UUID (check debug
     overlay or diag entity list).
   - Camera doesn't reset, WASD still works, no visible jump.

2. **Claim-timeout fallback path:**
   - Manually craft a ClaimPlayer message with a bogus UUID (via
     browser console: `network.send({type:'ClaimPlayer', player_id:
     '00000000-0000-0000-0000-000000000000'})` on a fresh tab).
   - Verify the server logs a warn after ~2s and spawns a new
     player. Client receives PlayerAssigned with the new UUID.

3. **Reconnect-url TLS scheme correctness:**
   - The boards run TLS. Confirm `Reconnect { url }` values are
     `wss://...` not `ws://...` in the browser console.

4. **Non-migrating clients unaffected:**
   - Fresh browser connect. Verify the normal spawn-player flow still
     works (no 500ms added latency for the non-migration case — the
     server's 500ms claim-wait only applies if no other message arrives,
     but a fresh client sends no message until WASD is pressed; so
     the spawn happens after the 500ms timeout). Acceptable UX.

   **If 500ms spawn-delay is unacceptable**, alternative UI: server
   spawns the player immediately on connect BUT marks the entity
   "provisional". If a ClaimPlayer arrives in the first 500ms,
   despawn the provisional entity and adopt the claim instead.
   Pro: zero latency for non-migration. Con: one extra spawn/despawn
   cycle for edge case. Decide during implementation.

5. **Source server's client WS close doesn't remove the entity:**
   - During scenario 1, on the source server, check logs for
     `"Client migrating out — skipping RemoveEntity on close"`.

**Done when:**
- Scenarios 1, 3, 4, 5 pass with evidence in `worklogs/mesh-dev/`.
- Scenario 2 (claim-timeout) verified via unit test OR noted as
  covered by the timeout code path.

---

## Open questions (resolved — listed for record)

- **Q1 (target URL source):** source constructs from `PeerWsAddresses`,
  populated from `PeerInfo.tls` + `PeerInfo.ws_addr`. No coordinator
  involvement needed in `RebalanceDirective`.
- **Q3 (client state preservation):** `ClientMessage::ClaimPlayer`
  protocol message. Target adopts entity by UUID, with 2s wait and
  spawn-fallback.

## Out of scope

- Persistent player state (inventory, stats, scores). That's the
  player-persistence design, out of scope per plan Section 4.
- Migration of multi-entity groups (e.g., a squad). Today each Player
  has one entity; multi-entity is future.
- Active input forwarding during the ~1s migration window. If the
  player presses WASD mid-migration, the input is either (a) sent to
  the old WS after `ws.close()` (silently dropped by send() guard), or
  (b) queued client-side and sent on the new WS. Either behavior is
  acceptable — the player stutters for ~200ms, not catastrophic. If
  smooth input is required, that's a follow-up design.
