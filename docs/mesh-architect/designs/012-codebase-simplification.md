# Design 012: Codebase Simplification

A comprehensive cleanup effort to reduce unnecessary complexity, remove dead code,
eliminate duplication, and improve module boundaries across the server meshing
codebase. The goal is to make the prototype clean enough to serve as a drop-in
package with clear API surface and minimal internal friction.

**Status:** Planned
**Audit reference:** `docs/mesh-auditor/complexity-audit.md` (34 findings)
**Estimated total effort:** 3-4 days

---

## Problem Statement

The codebase has accumulated complexity through rapid prototyping across 11 design
iterations. The result is:

1. **systems.rs is 8,041 lines** -- a single file containing 15 ECS systems, 225
   unit tests, and all game logic. It is the largest file by a factor of 10x over
   anything else in the codebase and is the primary barrier to comprehension.

2. **12 dead code items** marked with `#[allow(dead_code)]` or otherwise identified
   as unreachable. These create false complexity where a reader expects machinery
   that does nothing (`repl_backpressure`, `ClientFrame::Shared`, `TimingAccumulator`,
   `CompactEntityState`, etc.).

3. **Type inconsistency in the protocol layer** -- `ClientMessage` and
   `CoordClientMsg`/`CoordServerMsg` use `String` for entity IDs and raw `u16` for
   server IDs, while `PeerMessage` correctly uses `EntityId` and `ServerId`. This
   forces 4 `uuid::Uuid::parse_str` calls at consumption sites in systems.rs.

4. **Duplicated code** -- `build_tls_acceptor` is identical across two crates.
   `TICK_DURATION` is computed twice. Player speed 5.0 is hardcoded in 3 places.
   Inbound queue swap is a 20-line field-by-field copy that could be one line.

5. **Inline HTML dashboards** -- ~200 lines of HTML/JS embedded as raw string
   literals in Rust source files. No syntax highlighting, no formatting, inflates
   binary size for what could be `include_str!`.

6. **Leaky abstractions** -- `PeerBounds` (sim-server-only) lives in `shared/`.
   Four `struct { queue: Mutex<T> }` wrappers add indirection for no invariant.
   `DrainState` has 3 fields where 1 `Option<DrainInfo>` suffices.

---

## Design Principles

1. **Zero behavior change.** Every phase is pure refactoring. The binary output of
   each system must be identical before and after. Existing tests are the safety
   net -- they must all pass without modification (except tests for removed items).

2. **One phase at a time.** Each phase can be committed independently and deployed
   to the UNO Q boards for verification. No phase depends on a later phase.

3. **Dead code first.** Removing dead code reduces the surface area for all
   subsequent changes. It is the safest operation (pure deletion) and the easiest
   to verify (compile + test).

4. **Tests move with their code.** When systems.rs is split into modules, each
   system's tests move to the new module file. The schedule integration tests
   stay in schedule.rs since they test cross-system ordering.

---

## Phase 1: Dead Code Removal

Remove all 12 dead code items identified in findings 1.1-1.8, 2.1-2.6. These are
pure deletions with no behavior change.

### Items to Remove

| Item | File(s) | Lines removed |
|------|---------|---------------|
| `ClientFrame::Shared` variant + `as_bytes()` + `into_vec` match arm | resources.rs | ~15 |
| `TimingAccumulator` resource + all `init_resource` calls | resources.rs, schedule.rs, game_loop.rs | ~15 |
| `PeerUpdate.server_id` field + assignment in receiver | resources.rs, receiver.rs | ~3 |
| `InboundTransfer.tick` field + assignment in receiver | resources.rs, receiver.rs | ~3 |
| `ClientRegistry.next_id` field + bump logic | resources.rs | ~5 |
| `ServerMessage::WorldSnapshot` and `::EntityDelta` variants + tests | protocol.rs | ~20 |
| `ClientSnapshot` resource + all reads/writes | resources.rs, systems.rs, game_loop.rs, schedule.rs | ~30 |
| `CompactEntityState` + `From` impl, refactor delta.rs to use `EntityState` | delta.rs, resources.rs | ~25 |
| `GatewayMetrics` struct + `readMetrics()` + `_metrics` field | world-view.ts, binary-protocol.ts, network.ts | ~25 |
| `repl_backpressure` + `repl_drops_this_tick` variables | game_loop.rs | ~5 |
| `--full-repl-interval` + `--full-snapshot` CLI flags + `ReplicationConfig` resource + `FULL_REPL_INTERVAL` constant | main.rs, resources.rs, game_loop.rs | ~30 |
| `#[allow(dead_code)]` on `mod encoding` | main.rs | 1 |
| Test wrapper functions in network.rs `#[cfg(test)]` | network.rs | ~15 |

**Total estimated deletions:** ~190 lines of dead code + ~50 lines of dead tests.

### `ClientFrame` Simplification

After removing `ClientFrame::Shared`, the enum collapses to a single variant.
Replace the entire enum with a type alias:

```rust
// Before:
pub enum ClientFrame {
    Shared(Arc<Vec<u8>>),  // never constructed
    Owned(Vec<u8>),
}

// After:
pub type ClientFrame = Vec<u8>;
```

All channel types become `Sender<Vec<u8>>` / `Receiver<Vec<u8>>`. The `into_vec()`
call sites become identity (the value is already `Vec<u8>`).

### `CompactEntityState` Elimination

The delta module's `CompactEntityState` stores every field of `EntityState` minus
the `id`. The baseline map changes from `HashMap<EntityId, CompactEntityState>` to
`HashMap<EntityId, EntityState>`. The `entity_changed` function takes two
`&EntityState` instead of `(&EntityState, &CompactEntityState)`.

This affects:
- `delta.rs`: Remove `CompactEntityState` and `From<&EntityState>` impl. Update
  `compute_delta`, `change_ratio`, `build_baseline`, `update_baseline`, `entity_changed`.
- `resources.rs`: `ClientHandle.baseline` changes from
  `Option<HashMap<EntityId, CompactEntityState>>` to `Option<HashMap<EntityId, EntityState>>`.
- Import in resources.rs: Remove `use crate::delta::CompactEntityState`.

### `ClientSnapshot` Removal

The `ClientSnapshot` resource holds a global binary frame that is written by
`prepare_client_snapshot` but never sent to any client (per-client channels are
the delivery mechanism). The global frame is only read by schedule tests that
verify frame content.

Approach:
- Remove the `ClientSnapshot` resource and all writes to it.
- Update schedule tests to read frame content from a test client channel instead.
  (Register a test client, run the schedule, receive from the channel.)

### `ReplicationConfig` and Full Snapshot Removal

With the cold tier at 5Hz providing crash recovery coverage, the full-snapshot
machinery is permanently disabled. Remove:
- `FULL_REPL_INTERVAL` constant
- `ReplicationConfig` resource (both `full_interval` and `full_snapshot_enabled` fields)
- `--full-repl-interval` and `--full-snapshot` CLI args
- All code paths in `prepare_replication` and `game_loop.rs` that check
  `full_snapshot_enabled`

The `prepare_replication` system already handles the no-full-snapshot path as
its default behavior. Removing the config just deletes the dead branch.

### `ServerMessage::WorldSnapshot` and `::EntityDelta`

These JSON variants are never sent -- the server uses binary frames exclusively.
Remove both variants from the enum. Remove the roundtrip tests that exercise them.
Keep `PlayerAssigned` and `Reconnect` which are actively used.

### Client-side `GatewayMetrics`

The server writes `[0u8; 14]` for the metrics placeholder in binary frame headers.
The client decodes these zeros into a `GatewayMetrics` struct that is stored but
never displayed.

- Remove `GatewayMetrics` interface from world-view.ts
- Remove `readMetrics()` function from binary-protocol.ts
- Remove `_metrics` field from `WorldSnapshot` interface
- Remove `metrics` field from `WorldView` class
- Skip the 14 bytes in the binary decoder (advance offset without reading)

**Note:** The 14 bytes remain in the binary header for protocol compatibility.
The server still writes them and the client still skips them. Only the decode-to-
struct step is removed.

---

## Phase 2: Split systems.rs into Focused Modules

Break the 8,041-line monolith into a `systems/` directory with 10 focused modules.
Each module contains one or two related ECS systems and their unit tests.

### Module Layout

```
crates/sim-server/src/systems/
  mod.rs           -- re-exports all public system functions
  inbound.rs       -- drain_inbound_events (lines 79-289) + drain_client_events (436-480)
  input.rs         -- process_client_inputs (486-718) + send_transfer_ack helper (46-72)
  transfers.rs     -- process_transfer_acks (290-331), process_transfer_timeouts (332-429), process_transfers (899-981)
  drain.rs         -- process_drain (989-1098)
  replication.rs   -- prepare_replication (1112-1342)
  snapshot.rs      -- prepare_client_snapshot (1353-1591)
  simulation.rs    -- npc_behavior (841-864), physics_integration (871-878), clamp_positions (886-893)
  coordinator.rs   -- process_coord_events (724-834)
  shadow.rs        -- shadow_cleanup (1597-1612)
  metrics.rs       -- record_metrics (1618-1628)
```

### Estimated Module Sizes

| Module | Systems | Code lines | Test lines | Total |
|--------|---------|------------|------------|-------|
| inbound.rs | 2 | ~250 | ~300 | ~550 |
| input.rs | 1 + helper | ~260 | ~600 | ~860 |
| transfers.rs | 3 | ~240 | ~500 | ~740 |
| drain.rs | 1 | ~110 | ~200 | ~310 |
| replication.rs | 1 | ~230 | ~800 | ~1030 |
| snapshot.rs | 1 | ~240 | ~900 | ~1140 |
| simulation.rs | 3 | ~55 | ~200 | ~255 |
| coordinator.rs | 1 | ~110 | ~300 | ~410 |
| shadow.rs | 1 | ~20 | ~100 | ~120 |
| metrics.rs | 1 | ~15 | ~50 | ~65 |
| mod.rs | 0 (re-exports) | ~30 | 0 | ~30 |
| **Total** | **15** | **~1560** | **~3950** | **~5510** |

The remaining ~2500 lines of the original 8041 are the schedule integration tests
(in schedule.rs) and constants/imports that get consolidated.

### Shared Constants

Module-level constants in systems.rs (such as `SHADOW_STALE_TICKS`,
`TRANSFER_TIMEOUT_TICKS`, `TRANSFER_MAX_RETRIES`, `DRAIN_TIMEOUT_TICKS`,
`DELTA_FALLBACK_THRESHOLD`, `NPC_DIRECTION_CHANGE_INTERVAL`) move to the module
that uses them. Constants used by multiple modules go in `systems/mod.rs`.

### mod.rs Re-exports

```rust
// systems/mod.rs
mod inbound;
mod input;
mod transfers;
mod drain;
mod replication;
mod snapshot;
mod simulation;
mod coordinator;
mod shadow;
mod metrics;

pub use inbound::{drain_inbound_events, drain_client_events};
pub use input::process_client_inputs;
pub use transfers::{process_transfer_acks, process_transfer_timeouts, process_transfers};
pub use drain::process_drain;
pub use replication::prepare_replication;
pub use snapshot::prepare_client_snapshot;
pub use simulation::{npc_behavior, physics_integration, clamp_positions};
pub use coordinator::process_coord_events;
pub use shadow::shadow_cleanup;
pub use metrics::record_metrics;
```

### schedule.rs Unchanged

The schedule file continues to reference `systems::drain_inbound_events`, etc.
The re-exports ensure all references resolve. Schedule integration tests stay
in schedule.rs.

### Migration Safety

The split is a pure structural refactoring:
- No function signatures change.
- No behavior changes.
- All existing tests are moved verbatim.
- `cargo test` must pass identically before and after.

---

## Phase 3: Type-Safe Protocol IDs

Change `ClientMessage` and `CoordClientMsg`/`CoordServerMsg` to use `EntityId`
and `ServerId` instead of `String` and `u16`.

### Protocol Changes

```rust
// Before:
pub enum ClientMessage {
    SpawnPlayer { player_id: String, position: [f32; 3] },
    MoveInput { entity_id: String, direction: [f32; 3], seq: u32 },
    RemoveEntity { entity_id: String },
    ...
}

// After:
pub enum ClientMessage {
    SpawnPlayer { player_id: EntityId, position: [f32; 3] },
    MoveInput { entity_id: EntityId, direction: [f32; 3], seq: u32 },
    RemoveEntity { entity_id: EntityId },
    ...
}
```

Similarly for coordinator messages:

```rust
// Before:
pub enum CoordClientMsg {
    Register { server_id: u16, ... },
    Heartbeat { server_id: u16, ... },
    TransferResult { entity_id: String, target_server: u16, ... },
    ...
}

// After:
pub enum CoordClientMsg {
    Register { server_id: ServerId, ... },
    Heartbeat { server_id: ServerId, ... },
    TransferResult { entity_id: EntityId, target_server: ServerId, ... },
    ...
}
```

And for `CoordServerMsg`:

```rust
// Before:
RebalanceDirective { entity_ids: Vec<String>, target_server: u16 },
DrainDirective { target_servers: Vec<u16> },
RecoverEntities { dead_server_id: u16, survivors: Vec<u16> },

// After:
RebalanceDirective { entity_ids: Vec<EntityId>, target_server: ServerId },
DrainDirective { target_servers: Vec<ServerId> },
RecoverEntities { dead_server_id: ServerId, survivors: Vec<ServerId> },
```

Also `PeerInfo`:

```rust
// Before:
pub struct PeerInfo { pub server_id: u16, ... }

// After:
pub struct PeerInfo { pub server_id: ServerId, ... }
```

### Impact

Both `EntityId` and `ServerId` already derive `Serialize`/`Deserialize`. JSON
serialization of `EntityId` produces a UUID string, `ServerId` serializes as a
number. The wire format does not change.

Consumption sites in systems.rs lose their `uuid::Uuid::parse_str` calls:
- Line 448: `EntityId(uuid::Uuid::parse_str(&event.player_uuid)...)` becomes
  direct field access since `player_uuid` is already `EntityId`.
- Lines 515, 537, 628: `uuid::Uuid::parse_str(&entity_id)` disappears.

The coordinator side (registry.rs) loses its `.0` unwrapping on server IDs.

### Client Compatibility

The client sends JSON with `entity_id: "uuid-string"` and `player_id: "uuid-string"`.
Since `EntityId` serializes as a UUID string and `ServerId` as a number, the JSON
wire format is unchanged. No client code changes are needed for this phase.

### `ClientRegistrationEvent.player_uuid`

Currently `player_uuid: String`. Change to `player_uuid: EntityId`. The assignment
in `handle_client` (network.rs) will construct the `EntityId` at the point of entry
instead of systems.rs.

### Coordinator-Side Changes

The coordinator parses `CoordClientMsg` and `CoordServerMsg` for its own logic.
Its `registry.rs` currently works with `u16` server IDs and `String` entity IDs.
With `ServerId` and `EntityId` types, the coordinator gains type safety too.

Files affected:
- `crates/shared/src/protocol.rs` -- type changes
- `crates/sim-server/src/systems/*.rs` -- remove parse_str calls
- `crates/sim-server/src/network.rs` -- construct EntityId at entry
- `crates/sim-server/src/resources.rs` -- `ClientRegistrationEvent.player_uuid`
- `crates/coordinator/src/registry.rs` -- use ServerId/EntityId
- `crates/coordinator/src/main.rs` -- propagate type changes

---

## Phase 4: Deduplication

Fix the 6 duplication findings.

### 4a: `build_tls_acceptor` to shared

Move the identical `build_tls_acceptor` function from both `sim-server/src/main.rs`
and `coordinator/src/main.rs` into `shared/src/tls.rs`.

New file: `crates/shared/src/tls.rs`
```rust
pub fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    // ... existing implementation ...
}
```

This requires adding `rustls`, `tokio-rustls`, and `rustls-pemfile` to shared's
Cargo.toml. Since both downstream crates already depend on these, the total
dependency count does not increase.

**Alternative:** If adding async deps to shared/ feels wrong, create a small
`mesh-tls` crate or just use a `#[path]` include. The shared crate approach is
simplest.

### 4b: `TICK_DURATION` to shared/tick.rs

Add to `crates/shared/src/tick.rs`:
```rust
pub const TICK_DURATION: std::time::Duration =
    std::time::Duration::from_millis(TICK_DURATION_MS);
```

Remove the local `const TICK_DURATION` from both `systems.rs` (line 16) and
`game_loop.rs` (line 20). All usage sites import from `shared::tick::TICK_DURATION`.

### 4c: Player and NPC Speed Constants

Add to `crates/shared/src/tick.rs` (or a new `shared/src/gameplay.rs`):
```rust
pub const PLAYER_SPEED: f32 = 5.0;
pub const NPC_SPEED: f32 = 2.0;
```

Update `crates/sim-server/src/systems/input.rs` (formerly systems.rs lines 237, 571)
and `client/src/config.ts` (`GAMEPLAY.PLAYER_SPEED`) to reference these constants.

The client cannot import Rust constants, so `config.ts` keeps its own value but
gets a comment: `// Must match shared::PLAYER_SPEED in Rust`.

### 4d: Inbound Queue `std::mem::swap`

Replace the 20-line field-by-field swap pattern in game_loop.rs (lines 250-272)
with:

```rust
{
    let mut iq = inbound.queue.lock().unwrap_or_else(|e| e.into_inner());
    let mut ecs_queue = world.resource_mut::<InboundQueue>();
    std::mem::swap(&mut *iq, &mut *ecs_queue);
}
```

Same for `CoordEventQueue` (lines 274-286):

```rust
{
    let mut cq = coord_events.queue.lock().unwrap_or_else(|e| e.into_inner());
    let mut ecs_queue = world.resource_mut::<CoordEventQueue>();
    std::mem::swap(&mut *cq, &mut *ecs_queue);
}
```

This works because both types implement `Default`, and after the swap the shared
buffer contains the default (empty) state while the ECS resource contains the
queued data.

**Correctness note:** The previous pattern drained the shared buffer and left it
empty. `std::mem::swap` leaves the shared buffer with whatever was in the ECS
resource from the previous tick. Since ECS systems drain their queues each tick,
the ECS resource should be empty at the start of each tick -- making swap
equivalent. Add a debug assertion to verify: the swapped-in ECS values should
have empty Vecs.

### 4e: `spawn_and_track_ownership` Helper

Extract from systems.rs (the `process_client_inputs` system):

```rust
fn spawn_and_track_ownership(
    world: &mut World,
    state: &EntityState,
    client_id: u64,
) -> bevy_ecs::entity::Entity {
    let eid = state.id;
    let entity = spawn_from_entity_state_direct(world, state, true, None);
    world.resource_mut::<EntityOwnership>().owners.insert(eid, client_id);
    entity
}
```

Used by both `SpawnEntity` and `SpawnPlayer` handlers.

### 4f: `enqueue_peer_message` Helper

Extract the repeated pattern of serialize + push:

```rust
fn enqueue_forward(outbound: &mut OutboundForwards, addr: SocketAddr, msg: &PeerMessage) {
    if let Some(bytes) = serialize_versioned(msg) {
        outbound.messages.push((addr, bytes));
    }
}

fn enqueue_transfer(outbound: &mut OutboundTransfers, addr: SocketAddr, msg: &PeerMessage) {
    if let Some(bytes) = serialize_versioned(msg) {
        outbound.messages.push((addr, bytes));
    }
}
```

These replace the 5 instances of the serialize-and-push pattern.

---

## Phase 5: Extract Inline HTML

Move the two dashboard HTML strings to separate files and use `include_str!`.

### Coordinator Dashboard

New file: `crates/coordinator/static/dashboard.html` (~224 lines)

```rust
// coordinator/src/main.rs
const COORDINATOR_DASHBOARD: &str = include_str!("../static/dashboard.html");
```

### Metrics Server Dashboard

New file: `crates/sim-server/static/dashboard.html` (~205 lines)

```rust
// sim-server/src/metrics_server.rs
const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");
```

Benefits:
- HTML files get proper syntax highlighting and formatting in editors.
- Easier to iterate on dashboard UI without touching Rust source.
- No behavior change -- `include_str!` embeds at compile time.

---

## Phase 6: Clean Abstractions

### 6a: Move PeerBounds from shared to sim-server

Move `PeerBounds`, `PEER_AOI_RADIUS`, `PEER_AOI_RADIUS_SQ` from
`crates/shared/src/spatial.rs` to a new file
`crates/sim-server/src/peer_bounds.rs`.

`shared/spatial.rs` keeps only the truly shared geometry: `CELL_SIZE`,
`WORLD_EXTENT`, `AOI_RADIUS`, `AOI_RADIUS_SQ`, `clamp_to_world`, `in_aoi`,
`world_to_cell`, etc.

Move the PeerBounds tests too.

### 6b: Collapse DrainState

```rust
// Before:
pub struct DrainState {
    pub draining: bool,
    pub targets: Option<Vec<u16>>,
    pub started_tick: Option<u64>,
}

// After:
pub struct DrainInfo {
    pub targets: Vec<ServerId>,
    pub started_tick: u64,
}

#[derive(Resource, Default)]
pub struct DrainState(pub Option<DrainInfo>);

impl DrainState {
    pub fn is_draining(&self) -> bool { self.0.is_some() }
}
```

All `drain_state.draining` checks become `drain_state.is_draining()`.
All `drain_state.targets.as_ref().unwrap()` become `drain_state.0.as_ref().unwrap().targets`.
Or provide convenience methods.

### 6c: Replace Shared* Wrappers with Type Aliases

```rust
// Before:
pub struct SharedInbound { pub queue: std::sync::Mutex<InboundQueue> }
pub struct SharedCoordEvents { pub queue: std::sync::Mutex<CoordEventQueue> }
pub struct SharedCoordOutbound { pub queue: std::sync::Mutex<Vec<CoordClientMsg>> }
pub struct SharedPeerHealth { pub stats: std::sync::Mutex<PeerHealthAccumulator> }

// After:
pub type SharedInbound = std::sync::Mutex<InboundQueue>;
pub type SharedCoordEvents = std::sync::Mutex<CoordEventQueue>;
pub type SharedCoordOutbound = std::sync::Mutex<Vec<CoordClientMsg>>;
pub type SharedPeerHealth = std::sync::Mutex<PeerHealthAccumulator>;
```

All `.queue.lock()` chains become `.lock()`. All `Arc::new(SharedInbound { queue: ... })`
become `Arc::new(std::sync::Mutex::new(...))`.

### 6d: EntityKind / EntityType Naming

Rename the ECS component `EntityKind` back to `EntityTypeComp` to match the
documentation and reduce confusion with the `EntityType` enum.

```rust
// components.rs
pub struct EntityTypeComp(pub EntityType);
```

Or alternatively, rename `shared::entity::EntityType` to `EntityKind` and update
everything consistently. The first option is less invasive.

---

## Phase 7: Coordinator HTTP Cleanup

### Option A: Extract http_response helper (minimal change)

```rust
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK", 204 => "No Content", 400 => "Bad Request",
        404 => "Not Found", 503 => "Service Unavailable", _ => "Unknown",
    };
    let mut resp = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        body.len()
    ).into_bytes();
    resp.extend_from_slice(body);
    resp
}
```

This replaces the 10+ raw HTTP response constructions.

### Option B: Migrate to axum (larger change)

The `board-dash` crate already uses axum. Migrating the coordinator would:
- Eliminate all hand-rolled HTTP parsing and response building
- Give proper routing, middleware, and error handling
- Add ~50KB to the binary

This is a larger effort but would be the right long-term choice. The work item
list provides Option A (helper) as the default, with Option B as an optional
follow-up.

---

## Phase 8: Dependency Cleanup

### 8a: shared/Cargo.toml dev-dependencies

Move `serde_json` and `bincode` from `[dependencies]` to `[dev-dependencies]`
in `shared/Cargo.toml`. Both are only used in `#[cfg(test)]` blocks. Both
downstream crates declare their own dependency on these.

### 8b: rand 0.9 update

Update `rand` from 0.8 to 0.9 in `sim-server/Cargo.toml`. The main API changes:
- `rand::thread_rng()` becomes `rand::rng()`
- `Rng::gen_range` API is the same
- `rand::Rng` trait is the same

Small diff -- mostly import changes.

---

## Phase 9: Documentation Cleanup

### Archive Old Reports

Move the 79 mesh-auditor finding files into an `archive/` subdirectory:
```
docs/mesh-auditor/archive/    -- old per-finding reports
docs/mesh-auditor/complexity-audit.md  -- keep (active)
```

Similarly for mesh-architect:
```
docs/mesh-architect/archive/  -- old designs/work-items
docs/mesh-architect/designs/012-codebase-simplification.md  -- keep (active)
docs/mesh-architect/work-items/012-codebase-simplification.md  -- keep (active)
```

### Keep Essential Docs

After archiving, the docs/ directory should contain:
- `CLAUDE.md` (project instructions)
- `docs/api-reference.md`
- `docs/mesh-improvements.md` (roadmap)
- `docs/player-persistence-design.md`
- `docs/mesh-architect/` (active designs only)
- `docs/mesh-auditor/` (active audits only)

---

## Risk Assessment

| Phase | Risk | Mitigation |
|-------|------|------------|
| 1 (Dead code) | Removing ClientSnapshot breaks schedule tests | Rewrite tests to use client channels |
| 2 (Split) | Missing re-export breaks downstream reference | `cargo build` catches immediately |
| 3 (Type IDs) | JSON wire format changes for coordinator msgs | EntityId serializes as UUID string, ServerId as number -- unchanged |
| 4 (Dedup) | `std::mem::swap` on queues has different semantics | Add debug assertion that swapped ECS values are empty |
| 5 (HTML) | `include_str!` path resolution at compile time | Test with `cargo build` |
| 6 (Abstractions) | DrainState API change propagates widely | Small surface area, easy to grep |
| 7 (HTTP) | Helper misses a header the current code has | Diff all current responses against helper output |
| 8 (Deps) | rand 0.9 API changes break tests | Well-documented migration, small diff |

---

## Testing Strategy

Each phase:
1. `cargo build` -- compile check
2. `cargo test` -- all unit/integration tests pass
3. `./test.sh` -- cluster test suite passes
4. Deploy to UNO Q boards -- verify on real hardware

The existing 225 tests in systems.rs plus the 14 schedule integration tests are
the primary safety net. No new tests are needed for this effort -- it is pure
refactoring.

---

## Success Criteria

After all 9 phases:
- systems.rs no longer exists (replaced by systems/ directory, no file > 1200 lines)
- Zero `#[allow(dead_code)]` annotations in the codebase
- Zero `uuid::Uuid::parse_str` calls in consumption code
- Zero duplicate `TICK_DURATION` definitions
- Zero inline HTML in Rust source files
- `shared/spatial.rs` contains only truly shared geometry
- All 225+ tests pass
- All 14 test scenarios pass
- Boards run identically
