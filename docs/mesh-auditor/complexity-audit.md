# Complexity & Simplification Audit

A code quality and DX audit focused on unnecessary complexity, dead code, duplication, and awkward abstractions across the server meshing codebase.

---

## 1. Dead Code

### 1.1 `ClientFrame::Shared` variant is never constructed

**Files:** `crates/sim-server/src/resources.rs:16-19`, `crates/sim-server/src/resources.rs:27-29`

The `ClientFrame::Shared(Arc<Vec<u8>>)` variant and its `as_bytes()` method both carry `#[allow(dead_code)]`. The comment says "retained for backwards compatibility" but AoI filtering now always produces per-client `Owned` frames. No code path constructs `ClientFrame::Shared`. The `into_vec()` method handles it, but it is unreachable.

**Simplification:** Remove the `Shared` variant entirely. `ClientFrame` becomes a simple `Vec<u8>` wrapper or can be replaced with plain `Vec<u8>` in the channel. This removes `Arc` allocation logic and the `into_vec` match arm.

---

### 1.2 `TimingAccumulator` resource is unused

**Files:** `crates/sim-server/src/resources.rs:208-218`

`TimingAccumulator` is annotated `#[allow(dead_code)]` and is initialized in the ECS world (`schedule.rs:63`) but never read or written by any system. Phase timing is handled entirely by the `MetricsCollector` in `game_loop.rs`. The comment says "reserved for future per-system timing" but the current approach bypasses it completely.

**Simplification:** Remove `TimingAccumulator` from `resources.rs` and its `init_resource` call in `schedule.rs:63` and `game_loop.rs:191`.

---

### 1.3 `PeerUpdate.server_id` is unused

**Files:** `crates/sim-server/src/resources.rs:285-289`

The `server_id` field on `PeerUpdate` carries `#[allow(dead_code)]`. It is set by the receiver but never read by any consumer. `drain_inbound_events` in `systems.rs` uses `update.tick` and `update.entities` but ignores `server_id`.

**Simplification:** Remove the `server_id` field from `PeerUpdate` and the corresponding field assignment in `receiver.rs:149`.

---

### 1.4 `InboundTransfer.tick` is unused

**Files:** `crates/sim-server/src/resources.rs:291-298`

The `tick` field on `InboundTransfer` carries `#[allow(dead_code)]`. It is set by the receiver but `drain_inbound_events` uses `current_tick` from the ECS `TickCounter` resource instead. The received tick is never consulted.

**Simplification:** Remove the `tick` field from `InboundTransfer` and the corresponding field assignment in `receiver.rs:163`.

---

### 1.5 `encoding.rs` module-level `#[allow(dead_code)]`

**Files:** `crates/sim-server/src/main.rs:9`

The entire `encoding` module is declared with `#[allow(dead_code)]`, yet all three public functions (`encode_entity`, `encode_binary_snapshot`, `encode_binary_delta`) are used by `systems.rs` via `crate::encoding::`. The allow attribute is unnecessary.

**Simplification:** Remove `#[allow(dead_code)]` from `mod encoding;` in `main.rs:9`.

---

### 1.6 Test-only wrapper functions in `network.rs`

**Files:** `crates/sim-server/src/network.rs:276-292`

Three `#[cfg(test)]` functions (`encode_entity`, `encode_binary_snapshot`, `encode_binary_delta`) exist solely to delegate to `crate::encoding::*`. The tests in `network.rs` could import from `crate::encoding` directly.

**Simplification:** Replace the wrapper functions with direct `use crate::encoding::*` in the test module.

---

### 1.7 `ClientSnapshot` resource is a vestigial layer

**Files:** `crates/sim-server/src/resources.rs:156-160`

`ClientSnapshot` holds `Option<Vec<u8>>` for a single global binary frame. With the move to per-client frames via `ClientRegistry` channels, the only consumer of this resource is the `prepare_client_snapshot` system which writes to it AND to per-client channels. The field is read in schedule tests to verify frame content, but in production the per-client channels are the actual delivery mechanism. The global frame is built, stored in the resource, and never sent to anyone.

**Simplification:** The global snapshot frame can be removed. `prepare_client_snapshot` already pushes per-client frames directly. Only keep the global frame if tests need it, and even then they could query the client channels instead.

---

### 1.8 `FULL_REPL_INTERVAL` constant is shadowed by `ReplicationConfig`

**Files:** `crates/sim-server/src/resources.rs:102-107`

The constant `FULL_REPL_INTERVAL = 600` is defined as a module-level constant and used only as the default for `ReplicationConfig.full_interval`. The runtime value always comes from the resource. The constant adds an extra indirection without value.

**Simplification:** Inline the default value `600` in `ReplicationConfig::default()` and remove the standalone constant.

---

## 2. Over-Engineering

### 2.1 `repl_backpressure` boolean exists but has no effect

**Files:** `crates/sim-server/src/game_loop.rs:234`, `crates/sim-server/src/game_loop.rs:457-464`

The `repl_backpressure` boolean is declared, toggled, and logged, but the comment block at line 457 explicitly says "No boolean backpressure gate needed -- phase spreading already limits bandwidth." The variable is set but never checked in any conditional that controls replication behavior.

**Simplification:** Remove `repl_backpressure` and `repl_drops_this_tick` variables. They create false complexity -- a reader sees backpressure machinery and expects it to do something.

---

### 2.2 `full_repl_interval` and `full_snapshot` CLI flags for a disabled feature

**Files:** `crates/sim-server/src/main.rs:64-73`

Two CLI arguments (`--full-repl-interval`, `--full-snapshot`) control periodic full peer snapshots, but `--full-snapshot` defaults to `false` and the code comment says "disabled by default because the cold tier at 5Hz provides sufficient crash recovery coverage." These are configuration knobs for a feature that is off and has been superseded.

**Simplification:** Remove both CLI flags and the `full_snapshot_enabled` / `full_interval` fields from `ReplicationConfig`. The cold tier at 5Hz is the permanent solution. If full snapshots are ever needed again, they can be re-added.

---

### 2.3 `GatewayMetrics` in the client binary protocol is a dead struct

**Files:** `client/src/world-view.ts:24-29`, `client/src/binary-protocol.ts:62-69`

`GatewayMetrics` has fields `snapshotsPerSec`, `bandwidthKBps`, `rebalanceEvents`, `serverCount`. These are decoded from the 14-byte metrics placeholder in the binary frame header. But the server always writes `[0u8; 14]` for metrics (`encoding.rs:40, 63`). The client reads zeros and stores them, but the values are never displayed or used.

**Simplification:** Remove `GatewayMetrics`, `readMetrics()`, and the `_metrics` field from `WorldSnapshot`. The 14 bytes in the header are permanently zeros. If server-side metrics are ever embedded in frames, the struct can be re-added.

---

### 2.4 `CompactEntityState` is a redundant copy of `EntityState` minus the ID

**Files:** `crates/sim-server/src/delta.rs:26-45`

`CompactEntityState` stores position, velocity, entity_type, authority_server, and last_input_seq -- every field of `EntityState` except `id`. The baseline `HashMap<EntityId, CompactEntityState>` could instead be `HashMap<EntityId, EntityState>` with the same memory cost (16 bytes per UUID is negligible vs 5 fields). The "compact" abstraction saves nothing and adds a conversion step.

**Simplification:** Use `EntityState` directly in the baseline map. Remove `CompactEntityState` and its `From` impl. Change `entity_changed` to take two `&EntityState`.

---

### 2.5 `ServerMessage::WorldSnapshot` and `ServerMessage::EntityDelta` are never sent

**Files:** `crates/shared/src/protocol.rs:7-17`

These `ServerMessage` variants exist for JSON-over-WebSocket communication, but the server exclusively sends binary frames via `encode_binary_snapshot` / `encode_binary_delta`. The only `ServerMessage` variants actually sent are `PlayerAssigned` and `Reconnect`. The snapshot/delta variants are serialization artifacts from before binary encoding was implemented.

**Simplification:** Remove `WorldSnapshot` and `EntityDelta` from `ServerMessage`. They are tested via JSON roundtrip tests but never used in production. The binary protocol is the real format.

---

### 2.6 `ClientRegistry.next_id` field is maintained but never used as a generator

**Files:** `crates/sim-server/src/resources.rs:233-258`

`ClientRegistry` tracks `next_id` and updates it in `register_from_event`, but client IDs are generated externally by `NEXT_CLIENT_ID` in `network.rs:15`. The `next_id` field exists for an older design where the registry itself generated IDs. Now it is bumped but never called as an ID source.

**Simplification:** Remove the `next_id` field from `ClientRegistry`.

---

## 3. Copy-Paste / Duplication

### 3.1 `build_tls_acceptor` is duplicated between coordinator and sim-server

**Files:** `crates/coordinator/src/main.rs:320-350`, `crates/sim-server/src/main.rs:232-262`

These two functions are character-for-character identical: read PEM cert, read PEM key, build `rustls::ServerConfig`, return `tokio_rustls::TlsAcceptor`. Both crates already depend on `rustls`, `tokio-rustls`, and `rustls-pemfile`.

**Simplification:** Move `build_tls_acceptor` into `shared/` as a utility, or create a small `tls` module in `shared/`. Both crates gain the dependency via `shared = { path = "../shared" }` which they already use. This eliminates 30 duplicated lines and ensures both services handle TLS identically.

---

### 3.2 `TICK_DURATION` is computed twice with the same expression

**Files:** `crates/sim-server/src/systems.rs:16`, `crates/sim-server/src/game_loop.rs:20`

Both files have `const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);`. This is the same computation from the same source constant.

**Simplification:** Define `TICK_DURATION` once in `shared/src/tick.rs` alongside `TICK_DURATION_MS`. Both sim-server files import it. This also benefits the client, which currently hardcodes 50ms.

---

### 3.3 Player speed constant 5.0 is hardcoded in three places

**Files:** `crates/sim-server/src/systems.rs:237,571`, `client/src/config.ts:33`

Player speed is `5.0` in the Rust input processing (both local MoveInput at line 571 and forwarded input at line 237) and `5.0` in the client prediction config. NPC speed `2.0` is similarly hardcoded at lines 238 and 572. These are not defined as named constants in the Rust code.

**Simplification:** Define `PLAYER_SPEED` and `NPC_SPEED` constants in `systems.rs` (or `shared/`) and reference them. The client already uses `GAMEPLAY.PLAYER_SPEED`. The risk is that someone changes the Rust value and forgets the client or vice versa.

---

### 3.4 Inbound queue field-by-field swap is repeated across 10+ fields

**Files:** `crates/sim-server/src/game_loop.rs:250-272`

The game loop manually `std::mem::take`s each of the 10 fields from the shared `InboundQueue` mutex and assigns them to the ECS resource. This is repeated for `CoordEventQueue` (4 fields). The pattern is:
```rust
let field = std::mem::take(&mut iq.field);
ecs_queue.field = field;
```
for every field.

**Simplification:** Implement `std::mem::swap` on the entire struct: `std::mem::swap(&mut *iq, &mut *ecs_queue)`. Since `InboundQueue` implements `Default`, the shared buffer gets a fresh default and the ECS resource gets all the queued data in one operation. Same for `CoordEventQueue`.

---

### 3.5 Entity spawning + ownership tracking repeated in `SpawnEntity` and `SpawnPlayer`

**Files:** `crates/sim-server/src/systems.rs:492-530`

Both `SpawnEntity` and `SpawnPlayer` handlers call `spawn_from_entity_state_direct` then `world.resource_mut::<EntityOwnership>().owners.insert(eid, client_id)`. The only difference is how the `EntityState` is constructed. A helper function could reduce the repetition.

**Simplification:** Extract a `spawn_and_track_ownership(world, state, client_id)` helper used by both handlers.

---

### 3.6 `serialize_versioned` + `OutboundForwards` push pattern repeated 5 times

**Files:** `crates/sim-server/src/systems.rs:219-222, 553-557, 607-611, 659-663, 688-692`

The pattern of creating a `PeerMessage`, calling `serialize_versioned`, and pushing the result to `OutboundForwards.messages` or `OutboundTransfers.messages` is repeated identically at least 5 times. Each instance is 4-5 lines.

**Simplification:** Create a helper: `fn enqueue_peer_message(world: &mut World, addr: SocketAddr, msg: PeerMessage, outbound: &str)` that handles serialization and the push. Or provide `enqueue_forward(world, addr, msg)` and `enqueue_transfer(world, addr, msg)`.

---

## 4. Leaky Abstractions

### 4.1 `shared/spatial.rs` contains `PeerBounds` which is sim-server-only

**Files:** `crates/shared/src/spatial.rs:57-106`

`PeerBounds` (AABB for peer replication filtering) and `PEER_AOI_RADIUS` / `PEER_AOI_RADIUS_SQ` are only used by `crates/sim-server/src/systems.rs`. They encode sim-server replication logic (peer interest management) as shared types. The coordinator and client never use them.

**Simplification:** Move `PeerBounds`, `PEER_AOI_RADIUS`, and `PEER_AOI_RADIUS_SQ` to `crates/sim-server/`. Keep `shared/spatial.rs` focused on truly shared geometry (`CELL_SIZE`, `WORLD_EXTENT`, `AOI_RADIUS`, `clamp_to_world`, `in_aoi`).

---

### 4.2 `protocol.rs` mixes type-safe and stringly-typed IDs

**Files:** `crates/shared/src/protocol.rs:34-55`, `crates/shared/src/protocol.rs:107-148`

`ClientMessage` uses `entity_id: String` and `player_id: String` while `PeerMessage` uses `entity_id: EntityId`. The coordinator messages (`CoordClientMsg`, `CoordServerMsg`) use `entity_ids: Vec<String>` and `entity_id: String` alongside `server_id: u16` (not `ServerId`). This forces UUID parsing at every consumption site (`uuid::Uuid::parse_str` appears 4 times in `systems.rs`).

**Simplification:** Use `EntityId` and `ServerId` in all protocol messages. The JSON serialization already works (they derive Serialize/Deserialize). This removes all the `parse_str` calls and makes invalid IDs a deserialization error instead of a runtime parse failure.

---

### 4.3 `EntityKind` and `EntityTypeComp` naming confusion

**Files:** `crates/sim-server/src/components.rs:18-19`

The ECS component is named `EntityKind` wrapping `EntityType`. The conversion (`EntityKind(state.entity_type)`) reads awkwardly because the inner type is `EntityType`. The component was renamed from `EntityTypeComp` (mentioned in CLAUDE.md) but the mismatch between `EntityKind` (component) and `EntityType` (enum) is confusing to newcomers.

**Simplification:** Either rename the component to `EntityTypeComp` to match the wrapped type, or rename `EntityType` to `EntityKind` in `shared/entity.rs`. Pick one name and use it consistently.

---

## 5. Awkward API Surface

### 5.1 `WorldView.applySnapshot` and `WorldView.applyDelta` duplicate entity update logic

**Files:** `client/src/world-view.ts:83-182`, `client/src/world-view.ts:187-251`

Both methods contain nearly identical entity update logic: check if local player, copy position, set velocity, update authority, handle `hasSecondSnapshot`. The full snapshot version additionally removes entities not in the incoming set. The code for updating an existing entity is ~20 lines duplicated between the two methods.

**Simplification:** Extract an `updateEntity(existing, raw, isLocalPlayer, now)` private method. Both `applySnapshot` and `applyDelta` call it. The removal logic stays in `applySnapshot` and `applyDelta` respectively.

---

### 5.2 `ServerEntityCounts` recomputed from scratch on every frame

**Files:** `client/src/world-view.ts:175-182`, `client/src/world-view.ts:243-251`

Both `applySnapshot` and `applyDelta` clear and rebuild `serverEntityCounts` by iterating all entities. With deltas, most entities are unchanged, so the full rebuild is wasteful.

**Simplification:** Maintain `serverEntityCounts` incrementally: decrement when removing/changing an entity's server, increment when adding/changing. Or accept the full rebuild as simple enough (it is O(N) with small N).

---

## 6. Unnecessary Indirection

### 6.1 `SharedInbound` / `SharedCoordEvents` / `SharedCoordOutbound` / `SharedPeerHealth` are all `struct { queue: Mutex<T> }`

**Files:** `crates/sim-server/src/game_loop.rs:112-152`

Four separate structs each wrap a single `std::sync::Mutex<T>`. They add no methods, no invariants, no additional fields. They exist solely as named wrappers.

**Simplification:** Use `Arc<Mutex<InboundQueue>>` directly and type-alias if naming is desired: `type SharedInbound = Arc<Mutex<InboundQueue>>`. This removes four struct definitions and four `.queue.lock()` chains (just `.lock()` instead).

---

### 6.2 `ClientFrame.into_vec()` unwrap dance for an unreachable variant

**Files:** `crates/sim-server/src/resources.rs:36-42`

`ClientFrame::into_vec` matches on `Shared` with `Arc::try_unwrap(...).unwrap_or_else(|arc| (*arc).clone())`. Since `Shared` is never constructed (see finding 1.1), this entire branch is dead code hidden behind an enum match.

**Simplification:** After removing `Shared`, `into_vec` becomes a direct field access, or `ClientFrame` becomes `Vec<u8>`.

---

### 6.3 `DrainState` has redundant `started_tick` tracked separately from `draining`

**Files:** `crates/sim-server/src/resources.rs:181-187`

`DrainState` has three fields: `draining: bool`, `targets: Option<Vec<u16>>`, and `started_tick: Option<u64>`. The `draining` boolean is redundant with `targets.is_some()` -- whenever `draining` is set to `true`, `targets` is set to `Some(...)`. And `started_tick` is always set alongside `draining`.

**Simplification:** Collapse to `DrainState(Option<DrainInfo>)` where `DrainInfo { targets: Vec<u16>, started_tick: u64 }`. `is_draining()` checks `is_some()`.

---

## 7. Hard-Coded Assumptions

### 7.1 Metrics port formula `8100 + server_id` hard-coded in 3 places

**Files:** `crates/sim-server/src/main.rs:81`, `crates/coordinator/src/registry.rs:329`, `crates/coordinator/src/main.rs:682`

The formula `8100 + server_id` for the metrics port is computed independently in three places. If the base port changes, all three must be updated. The sim-server computes it for its own binding, the coordinator computes it for the server list API response, and the coordinator computes it again for the metrics proxy endpoint.

**Simplification:** Have the sim-server report its actual metrics port in the `Register` message (add a `metrics_port` field to `CoordClientMsg::Register`). The coordinator stores it in `ServerEntry` instead of computing it. This also fixes a latent bug: if `--metrics-port` is used to override the default, the coordinator still computes `8100 + id` and proxies to the wrong port.

---

### 7.2 NPC spawn range `400.0` is not configurable or related to any constant

**Files:** `crates/sim-server/src/game_loop.rs:201`

NPCs spawn in `[-400, 400]` which is a magic number. The comment explains the rationale (cluster near origin for AoI visibility) but the value is not a named constant and is unrelated to `AOI_RADIUS` (500) or `WORLD_EXTENT` (5000).

**Simplification:** Define `const NPC_SPAWN_RANGE: f32 = 400.0;` with a comment explaining the relationship to `AOI_RADIUS`.

---

### 7.3 Heartbeat timeout differs between code (15s) and CLAUDE.md (5s)

**Files:** `crates/coordinator/src/registry.rs:124-133`

`detect_dead_servers` uses a 15-second timeout, but CLAUDE.md and various docs reference a "5s heartbeat timeout." The actual heartbeat interval is 1 second (coordinator_client.rs:56) and the check interval is 5 seconds (coordinator main.rs:252). The 15-second timeout is correct for the implementation but the documentation is stale.

**Simplification:** Define `const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);` as a named constant in `registry.rs` and update CLAUDE.md to match.

---

### 7.4 Coordinator read timeout (30s) is unrelated to any named constant

**Files:** `crates/sim-server/src/coordinator_client.rs:93`

The coordinator client uses `Duration::from_secs(30)` as a read timeout with a comment explaining the rationale (6 missed keepalives). This value should relate to the coordinator's keepalive interval (5s) but it is a magic number.

**Simplification:** Define `const COORDINATOR_READ_TIMEOUT: Duration = Duration::from_secs(30);` or compute it as `KEEPALIVE_INTERVAL * 6`.

---

### 7.5 Raw HTTP response construction repeated throughout coordinator

**Files:** `crates/coordinator/src/main.rs:593-795`

The coordinator builds HTTP responses with raw string formatting 10+ times. Each response manually constructs `HTTP/1.1 {status}\r\nContent-Type: ...\r\n...`. This is fragile (easy to miss a header), repetitive, and makes adding global headers (e.g., security headers) require touching every response site.

**Simplification:** Extract a `fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8>` helper. Or consider using `hyper` / `axum` -- the coordinator is simple enough that a lightweight HTTP framework would reduce code and eliminate hand-rolled HTTP parsing.

---

### 7.6 Dashboard HTML is embedded as massive string literals

**Files:** `crates/sim-server/src/metrics_server.rs:6-205`, `crates/coordinator/src/main.rs:352-575`

Two HTML dashboards (~200 lines of inline JavaScript each) are embedded as raw string literals in Rust source files. This makes them hard to edit (no syntax highlighting, no formatting), hard to test, and they inflate the binary. The coordinator dashboard alone is ~6KB of HTML/JS.

**Simplification:** Move dashboards to separate `.html` files and include them via `include_str!("dashboard.html")`. Or serve them from the `--static-dir` mechanism that already exists. This preserves the zero-dependency approach while improving editability.

---

## 8. Dependency Notes

### 8.1 `rand` crate pinned to 0.8 (not latest)

**Files:** `crates/sim-server/Cargo.toml:19`

`rand = "0.8"` is one major version behind (0.9 has been available). Not a pressing issue but worth noting for a future dependency update pass.

---

### 8.2 `shared` crate includes `bincode` and `serde_json` but only uses them in tests

**Files:** `crates/shared/Cargo.toml:9-10`

The shared crate declares `serde_json = "1"` and `bincode = "1"` as full dependencies, but they are only used in `#[cfg(test)]` blocks in `entity.rs` and `protocol.rs`. They should be `[dev-dependencies]` to avoid pulling them into coordinator/sim-server compilation through `shared`.

**Simplification:** Move `serde_json` and `bincode` to `[dev-dependencies]` in `shared/Cargo.toml`. Both downstream crates already declare their own dependency on these crates.

---

## Summary Table

| # | Category | Finding | Effort |
|---|----------|---------|--------|
| 1.1 | Dead code | `ClientFrame::Shared` never constructed | small |
| 1.2 | Dead code | `TimingAccumulator` resource unused | small |
| 1.3 | Dead code | `PeerUpdate.server_id` unused | small |
| 1.4 | Dead code | `InboundTransfer.tick` unused | small |
| 1.5 | Dead code | `encoding.rs` false `allow(dead_code)` | trivial |
| 1.6 | Dead code | Test wrapper functions in `network.rs` | trivial |
| 1.7 | Dead code | `ClientSnapshot` global frame never sent | medium |
| 1.8 | Dead code | `FULL_REPL_INTERVAL` shadowed by config | trivial |
| 2.1 | Over-engineering | `repl_backpressure` set but never checked | small |
| 2.2 | Over-engineering | Full snapshot CLI flags for disabled feature | small |
| 2.3 | Over-engineering | `GatewayMetrics` decodes permanent zeros | small |
| 2.4 | Over-engineering | `CompactEntityState` redundant with `EntityState` | small |
| 2.5 | Over-engineering | `ServerMessage` snapshot/delta variants unused | small |
| 2.6 | Over-engineering | `ClientRegistry.next_id` maintained but unused | trivial |
| 3.1 | Duplication | `build_tls_acceptor` duplicated between crates | small |
| 3.2 | Duplication | `TICK_DURATION` computed in two files | trivial |
| 3.3 | Duplication | Player speed `5.0` hardcoded in 3 places | small |
| 3.4 | Duplication | Inbound queue field-by-field swap (10 fields) | small |
| 3.5 | Duplication | Spawn + ownership tracking repeated | small |
| 3.6 | Duplication | `serialize_versioned` + push pattern x5 | small |
| 4.1 | Leaky abstraction | `PeerBounds` in shared but sim-server-only | small |
| 4.2 | Leaky abstraction | Mix of `String` and `EntityId` in protocol | medium |
| 4.3 | Leaky abstraction | `EntityKind` vs `EntityType` naming | small |
| 5.1 | Awkward API | Snapshot/delta entity update logic duplicated | small |
| 5.2 | Awkward API | Server counts rebuilt from scratch each frame | trivial |
| 6.1 | Indirection | Four `struct { queue: Mutex<T> }` wrappers | small |
| 6.2 | Indirection | `into_vec()` match on unreachable variant | trivial |
| 6.3 | Indirection | `DrainState` redundant fields | small |
| 7.1 | Hard-coded | Metrics port `8100+id` in 3 places | small |
| 7.2 | Hard-coded | NPC spawn range magic number | trivial |
| 7.3 | Hard-coded | Heartbeat timeout docs vs code mismatch | trivial |
| 7.4 | Hard-coded | Coordinator read timeout magic number | trivial |
| 7.5 | Hard-coded | Raw HTTP responses repeated 10+ times | medium |
| 7.6 | Hard-coded | Dashboard HTML as inline string literals | small |
| 8.1 | Dependency | `rand` pinned to 0.8 | trivial |
| 8.2 | Dependency | `shared` full deps that should be dev-deps | small |
