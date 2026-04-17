# Work Items: 001 Bevy ECS Migration

Design document: `docs/mesh-architect/designs/001-bevy-ecs-migration.md`

Total work items: 8
Estimated total: 14-20 hours

---

### WI-001: Add bevy_ecs dependency and verify cross-compilation

**Size:** small (< 1 hour)
**Depends on:** none
**Files:**
- `crates/sim-server/Cargo.toml`

**Description:**

Add `bevy_ecs` as a dependency to the sim-server crate. Use the standalone crate, not the full `bevy` engine. Pin to a specific version (check crates.io for latest stable -- as of early 2025, bevy_ecs 0.15.x). Use default features, which should be minimal.

```toml
bevy_ecs = "0.15"
```

Verify that:
1. `cargo build -p sim-server` succeeds on the dev Mac.
2. `cargo build -p sim-server --target aarch64-unknown-linux-gnu` cross-compiles for the UNO Q boards (or whatever the current cross-compile target is -- check `start-unoq.sh` for the target triple).
3. All existing tests still pass: `cargo test -p sim-server`.

Do NOT use `bevy_ecs` anywhere yet. This work item only adds the dependency and confirms it compiles.

If the cross-compile fails due to `bevy_ecs` pulling in a problematic transitive dependency, document the issue and investigate feature flags to disable it.

**Tests:** Run `cargo test --workspace` and `cargo build --target aarch64-unknown-linux-gnu -p sim-server`.

**Done when:**
- `bevy_ecs` is in `Cargo.toml`
- `cargo build -p sim-server` succeeds
- Cross-compilation for UNO Q target succeeds
- All existing tests pass

---

### WI-002: Define ECS components and resources

**Size:** medium (1-3 hours)
**Depends on:** WI-001
**Files:**
- `crates/sim-server/src/components.rs` (new)
- `crates/sim-server/src/resources.rs` (new)
- `crates/sim-server/src/main.rs` (add mod declarations)

**Description:**

Create two new files in the sim-server crate.

**`components.rs`** -- Define all ECS components:

```rust
use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityId, EntityType, ServerId};

#[derive(Component, Debug, Clone, Copy)]
pub struct NetworkId(pub EntityId);

#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Position(pub Vec3);

#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Velocity(pub Vec3);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityKind(pub EntityType);

#[derive(Component, Debug, Clone, Copy)]
pub struct AuthorityServer(pub ServerId);

#[derive(Component, Debug, Default)]
pub struct Authoritative;

#[derive(Component, Debug)]
pub struct Shadow {
    pub last_seen_tick: u64,
}

#[derive(Component, Debug, Default)]
pub struct LastInputSeq(pub u32);
```

**`resources.rs`** -- Define all ECS resources. See design doc for full list. Include the InboundQueue, CoordEventQueue, OutboundReplication, ClientSnapshot, PendingTransfers, OutboundTransfers, OutboundForwards, DrainState, ServerConfig, TickCounter, PeerAddresses, EntityLookup, TimingAccumulator. Also define the event types used in the queues: PeerUpdate, InboundTransfer, ForwardedInput.

Add `mod components; mod resources;` to `main.rs` (do not remove any existing modules).

**Tests:** Write unit tests in each file:

For `components.rs`:
- Verify all component types derive the correct traits (Component, Debug, Clone where appropriate).
- Spawn a minimal bevy_ecs World, insert an entity with all components, query it back, assert values match.
- Spawn entities with Authoritative and Shadow markers, verify `Query<..., With<Authoritative>>` only returns authoritative entities.
- Verify LastInputSeq is optional: spawn an NPC entity without it, query with `Option<&LastInputSeq>`, confirm it is None.

For `resources.rs`:
- Create a World, insert each resource, read it back, assert defaults.
- Insert an entity into EntityLookup, look it up by EntityId, verify the Entity handle matches.

**Done when:**
- `components.rs` and `resources.rs` exist with all types defined
- Modules declared in `main.rs`
- Unit tests pass: component queries work, resource insertion works
- `cargo test -p sim-server` passes (existing tests unaffected)

---

### WI-003: Build EntityState conversion functions

**Size:** medium (1-3 hours)
**Depends on:** WI-002
**Files:**
- `crates/sim-server/src/conversion.rs` (new)
- `crates/sim-server/src/main.rs` (add mod declaration)

**Description:**

Create `conversion.rs` with two core functions:

**`spawn_from_entity_state`** -- Takes an EntityState, a Commands reference, and the EntityLookup. Spawns a Bevy entity with the appropriate components. If `is_authoritative` is true, inserts the `Authoritative` marker; otherwise inserts `Shadow { last_seen_tick }`. If `entity_type == Player`, inserts `LastInputSeq`. Updates the EntityLookup. Returns the Bevy Entity handle.

```rust
pub fn spawn_from_entity_state(
    commands: &mut Commands,
    lookup: &mut ResMut<EntityLookup>,
    state: &EntityState,
    is_authoritative: bool,
    shadow_tick: Option<u64>,
) -> Entity
```

**`to_entity_state`** -- Assembles an EntityState from individual component references. This is a pure function with no World access.

```rust
pub fn to_entity_state(
    network_id: &NetworkId,
    position: &Position,
    velocity: &Velocity,
    kind: &EntityKind,
    authority: &AuthorityServer,
    last_seq: Option<&LastInputSeq>,
) -> EntityState
```

**`despawn_and_remove`** -- Despawns a Bevy entity and removes it from EntityLookup.

```rust
pub fn despawn_and_remove(
    commands: &mut Commands,
    lookup: &mut ResMut<EntityLookup>,
    entity: Entity,
    network_id: EntityId,
)
```

**Tests:**

1. **Roundtrip: EntityState -> ECS -> EntityState** -- Create an EntityState (NPC), spawn it, query components, convert back, assert all fields match.
2. **Roundtrip for Player** -- Same as above but with Player entity type, verify LastInputSeq is preserved.
3. **Authoritative marker** -- Spawn as authoritative, verify `With<Authoritative>` query finds it.
4. **Shadow marker** -- Spawn as shadow with tick 42, verify Shadow.last_seen_tick == 42.
5. **Lookup insertion** -- After spawn, EntityLookup contains the mapping.
6. **Despawn removes from lookup** -- After despawn_and_remove, EntityLookup no longer contains the ID.
7. **Zero last_input_seq for NPC** -- NPC spawn, convert back, last_input_seq == 0.
8. **Non-zero last_input_seq for Player** -- Player with seq=99, roundtrip preserves it.

Note: These tests need to create a `bevy_ecs::world::World`, register command buffers, and apply them. The pattern is:

```rust
#[test]
fn roundtrip_npc() {
    let mut world = World::default();
    world.init_resource::<EntityLookup>();

    let state = EntityState::new_npc(Vec3::new(10.0, 0.0, -20.0), ServerId(1));
    let original_id = state.id;

    // Use World::spawn directly for unit tests (Commands require a stage)
    // OR use a SystemState to get Commands access
    // Simplest: write a helper that takes &mut World directly for tests
}
```

Consider adding a `spawn_from_entity_state_direct(world: &mut World, ...)` variant that operates on `&mut World` directly (no Commands indirection) for easier unit testing. The Commands-based version is used in actual systems.

**Done when:**
- `conversion.rs` exists with all three functions
- Roundtrip tests pass for NPC and Player entities
- EntityLookup consistency tests pass
- `cargo test -p sim-server` passes

---

### WI-004: Implement core simulation systems

**Size:** large (3+ hours)
**Depends on:** WI-003
**Files:**
- `crates/sim-server/src/systems.rs` (new, or `systems/` directory with one file per system)
- `crates/sim-server/src/main.rs` (add mod declaration)

**Description:**

Implement the following ECS systems as standalone functions. Each system is a pure function with Bevy ECS parameter signatures. Do NOT yet wire them into a schedule or replace the existing game loop -- that happens in WI-006.

**Systems to implement:**

1. **`npc_behavior`** -- Port NPC wandering logic from `simulation.rs:simulate_tick()`. NPCs change direction every `NPC_DIRECTION_CHANGE_INTERVAL` ticks based on their UUID hash phase. Query: `(&NetworkId, &mut Velocity, &EntityKind), With<Authoritative>`. Filter EntityKind for Npc at runtime.

2. **`physics_integration`** -- Port position integration: `position += velocity * dt`. Query: `(&mut Position, &Velocity), With<Authoritative>`. Use `TICK_DURATION.as_secs_f32()` for dt.

3. **`clamp_positions`** -- Port `clamp_all_positions()`. Query: `&mut Position, With<Authoritative>`. Call `clamp_to_world(pos.0)`.

4. **`shadow_cleanup`** -- Port `cleanup_shadows()`. Query: `(Entity, &NetworkId, &Shadow)`. Despawn entities where `tick - shadow.last_seen_tick > SHADOW_STALE_TICKS`. Update EntityLookup.

5. **`prepare_replication`** -- Query all authoritative entities, convert to Vec<EntityState> using `to_entity_state`, write to `OutboundReplication` resource.

6. **`prepare_client_snapshot`** -- Query both authoritative and shadow entities, merge (authoritative takes precedence for same NetworkId), encode binary frame using the existing `encode_binary_snapshot` function from `network.rs`. Write to `ClientSnapshot` resource.

    Note: Move `encode_binary_snapshot`, `encode_binary_delta`, and `encode_entity` from `network.rs` into a new `encoding.rs` file (or a shared location) so they can be called from the system without circular module dependencies.

7. **`record_metrics`** -- Read TimingAccumulator, count authoritative/shadow entities, write metrics snapshot.

**Tests for each system:**

For each system, write a test that:
1. Creates a `bevy_ecs::world::World`.
2. Inserts the required resources.
3. Spawns test entities with the appropriate components.
4. Runs the system using a one-system Schedule.
5. Asserts the expected state change.

Example test pattern:

```rust
#[test]
fn physics_integration_moves_entities() {
    let mut world = World::default();
    world.init_resource::<TickCounter>();

    // Spawn an authoritative entity with velocity
    world.spawn((
        NetworkId(EntityId::new()),
        Position(Vec3::ZERO),
        Velocity(Vec3::new(5.0, 0.0, 0.0)),
        EntityKind(EntityType::Player),
        AuthorityServer(ServerId(1)),
        Authoritative,
    ));

    // Run the system
    let mut schedule = Schedule::default();
    schedule.add_systems(physics_integration);
    schedule.run(&mut world);

    // Check position moved
    let mut query = world.query::<&Position>();
    let pos = query.single(&world);
    assert!(pos.0.x > 0.0); // moved in +X
}
```

Specific tests per system:

**npc_behavior:**
- NPC velocity changes after direction change interval tick
- Player velocity is NOT modified by this system
- Shadow entities are NOT modified

**physics_integration:**
- Position integrates by velocity * dt
- Shadow entity positions are NOT changed
- Zero velocity means no position change

**clamp_positions:**
- Out-of-bounds position is clamped to WORLD_EXTENT
- In-bounds position is unchanged
- Y axis is not clamped

**shadow_cleanup:**
- Fresh shadow (tick delta < SHADOW_STALE_TICKS) is kept
- Stale shadow (tick delta > SHADOW_STALE_TICKS) is despawned
- EntityLookup is updated on despawn

**prepare_replication:**
- Only authoritative entities appear in the output
- Shadow entities are excluded
- Empty world produces empty Vec
- LastInputSeq is correctly set (0 for NPCs, actual value for players)

**prepare_client_snapshot:**
- Merge includes both auth and shadow
- Auth takes precedence when same NetworkId exists as both (shouldn't happen, but guard)
- Binary frame format matches existing tests (header + N * 47 bytes)

**Done when:**
- All 7 systems implemented as standalone functions
- Each system has unit tests demonstrating correct behavior
- Existing sim-server tests still pass (old code untouched)
- `cargo test -p sim-server` passes

---

### WI-005: Implement input processing and event drain systems

**Size:** large (3+ hours)
**Depends on:** WI-004
**Files:**
- `crates/sim-server/src/systems.rs` (or `systems/input.rs` and `systems/events.rs`)

**Description:**

Implement the systems that handle inbound data from async tasks. These are the most complex systems because they bridge async I/O with ECS state.

**Systems to implement:**

1. **`drain_inbound_events`** -- Process the InboundQueue resource:

    - **Peer updates (EntityStateUpdate):** For each entity in the update:
      - If the NetworkId is in EntityLookup and the entity has `Authoritative`, skip.
      - If the NetworkId is in EntityLookup and the entity has `Shadow`, check tick ordering. If the update tick > shadow.last_seen_tick, update Position, Velocity, AuthorityServer, and shadow.last_seen_tick. Otherwise skip.
      - If the NetworkId is not in EntityLookup, spawn a new entity with `Shadow` marker.

    - **Authority transfers:** For each transfer:
      - Validate `to_server` matches our server_id.
      - If entity exists as Authoritative, skip (duplicate).
      - If entity exists as Shadow, remove Shadow component, add Authoritative component, update all data components from full_state.
      - If entity does not exist, spawn as Authoritative.

    - **Forwarded inputs:** For each forwarded input:
      - Lookup entity by NetworkId. If authoritative, apply velocity change and update LastInputSeq.
      - If not authoritative, log warning and skip.

    Clear the InboundQueue after processing.

2. **`process_client_inputs`** -- Process client_inputs from InboundQueue:

    Port `process_input()` from `simulation.rs`. For each ClientMessage:

    - **SpawnEntity:** Spawn a new authoritative entity at the given position.
    - **SpawnPlayer:** Spawn a player with the given UUID.
    - **MoveInput:** If entity is authoritative, apply velocity and update LastInputSeq. If entity is a shadow, produce a ForwardedInput and write it to OutboundForwards. If unknown, log and skip.
    - **RemoveEntity:** Despawn the entity, update EntityLookup.
    - **RequestSnapshot:** No-op.

3. **`process_coord_events`** -- Process the CoordEventQueue resource:

    Port coordinator_client.rs message handling:

    - **PeerList:** Update PeerAddresses resource.
    - **RebalanceDirective:** Set PendingTransfers.
    - **DrainDirective:** Set DrainState.
    - **RecoverEntities:** Promote shadow entities from dead server to authoritative (port `recover_from_dead_server` logic).

    Clear the CoordEventQueue after processing.

4. **`process_transfers`** -- Port transfer logic from `simulation.rs:process_transfers()`:

    If PendingTransfers is set:
    - Collect NPC entities sorted by distance from origin (descending).
    - Transfer up to 10% of entities.
    - For each: convert to EntityState, serialize PeerMessage::AuthorityTransfer, push (addr, bytes) to OutboundTransfers.
    - Despawn transferred entities from ECS, update EntityLookup.
    - Clear PendingTransfers.

5. **`process_drain`** -- Port drain logic from `simulation.rs:process_drain()`:

    If DrainState.draining:
    - Round-robin all entities across drain_targets.
    - Serialize AuthorityTransfer messages, push to OutboundTransfers.
    - Despawn all transferred entities.
    - If no entities remain, set a shutdown flag (can be a Resource: `ShutdownRequested(bool)`).

**Tests:**

**drain_inbound_events:**
- Peer update creates shadow entity
- Peer update does not overwrite authoritative entity
- Peer update with stale tick is rejected
- Peer update with newer tick updates shadow
- Authority transfer creates authoritative entity from nothing
- Authority transfer promotes shadow to authoritative
- Authority transfer for wrong server is rejected
- Duplicate transfer for already-authoritative entity is rejected
- Forwarded input applies velocity to authoritative entity
- Forwarded input for non-authoritative entity is ignored

**process_client_inputs:**
- SpawnEntity creates authoritative NPC
- SpawnEntity with "player" type creates Player
- SpawnPlayer with UUID creates player with correct EntityId
- MoveInput sets velocity on authoritative entity
- MoveInput for shadow produces OutboundForward
- MoveInput for unknown entity produces nothing
- RemoveEntity despawns and cleans up lookup
- RequestSnapshot is a no-op

**process_coord_events:**
- PeerList updates PeerAddresses
- RebalanceDirective sets PendingTransfers
- DrainDirective sets DrainState
- RecoverEntities promotes correct subset (hash-based distribution)

**process_transfers:**
- Transfers only NPCs, never players
- Transfers at most 10% of entities
- Peripheral entities (farthest from origin) transferred first
- Transferred entities are despawned
- EntityLookup updated after despawn
- OutboundTransfers contains serialized messages

**process_drain:**
- All entities transferred round-robin to targets
- Entities despawned after transfer
- Shutdown flagged when all entities transferred

**Done when:**
- All 5 systems implemented
- All tests pass
- Existing sim-server tests still pass
- `cargo test -p sim-server` passes

---

### WI-006: Build the schedule and new game loop

**Size:** large (3+ hours)
**Depends on:** WI-004, WI-005
**Files:**
- `crates/sim-server/src/schedule.rs` (new)
- `crates/sim-server/src/game_loop.rs` (new)
- `crates/sim-server/src/main.rs` (add mod declarations)

**Description:**

**`schedule.rs`:** Define the TickSchedule label and `build_schedule()` function that creates a Schedule with all systems in the correct order:

```
drain_inbound_events
  -> process_client_inputs
  -> process_coord_events
  -> npc_behavior
  -> physics_integration
  -> clamp_positions
  -> process_transfers
  -> process_drain
  -> prepare_replication
  -> prepare_client_snapshot
  -> shadow_cleanup
  -> record_metrics
```

Use `.after()` ordering constraints to enforce the sequence.

**`game_loop.rs`:** Implement `run_game_loop_ecs()` -- the new async game loop function. This replaces `simulation::run_game_loop()`.

The function:
1. Creates a `bevy_ecs::world::World`.
2. Registers all resources with initial values (ServerConfig, TickCounter(0), PeerAddresses, EntityLookup, InboundQueue, CoordEventQueue, OutboundReplication, ClientSnapshot, PendingTransfers, OutboundTransfers, OutboundForwards, DrainState, TimingAccumulator).
3. Spawns initial NPCs by creating EntityStates and using `spawn_from_entity_state_direct()`.
4. Builds the schedule.
5. Runs the tick loop:
   - Await interval.
   - Swap inbound queues from async buffers into ECS resources.
   - Increment TickCounter.
   - Run schedule.
   - Extract outbound data (replication, client snapshot, transfers, forwards).
   - Send UDP replication to peers.
   - Send transfers and forwards via UDP.
   - Send client snapshot frame via broadcast channel.
   - Record tick timing to MetricsCollector.

The World is wrapped in `std::sync::Mutex` for the swap steps but the lock is held only briefly (microseconds). During schedule execution, only the game loop thread touches the World.

**Shared buffers** between async tasks and the game loop:

```rust
pub struct SharedInbound {
    pub queue: tokio::sync::Mutex<InboundQueue>,
}

pub struct SharedCoordEvents {
    pub queue: tokio::sync::Mutex<CoordEventQueue>,
}
```

The UDP receiver pushes to `SharedInbound`. The coordinator client pushes to `SharedCoordEvents`. The WebSocket input handler pushes client_inputs to `SharedInbound`. The game loop drains these at tick start.

**Tests:**

1. **Schedule builds without panic** -- Call `build_schedule()`, verify no system ordering conflicts.
2. **Full tick cycle** -- Create a World with resources, spawn an NPC, run one tick, verify position changed (physics integration) and OutboundReplication contains the entity.
3. **Multi-tick cycle** -- Run 10 ticks, verify tick counter advances correctly and entities accumulate position changes.
4. **Inbound queue drain** -- Push a peer update into InboundQueue, run schedule, verify shadow entity created.
5. **Client input processing** -- Push a SpawnEntity into client_inputs, run schedule, verify authoritative entity created.
6. **Replication output** -- Spawn authoritative entities, run schedule, verify OutboundReplication contains correct EntityStates.
7. **Client snapshot output** -- Spawn auth + shadow entities, run schedule, verify ClientSnapshot contains both.

**Done when:**
- Schedule builds and runs without errors
- Full tick cycle test passes
- Game loop function compiles (even if not wired into main.rs yet)
- All system ordering is correct (no deadlocks, no missing dependencies)
- `cargo test -p sim-server` passes

---

### WI-007: Wire up async tasks and replace the old game loop

**Size:** large (3+ hours)
**Depends on:** WI-006
**Files:**
- `crates/sim-server/src/main.rs` (major changes)
- `crates/sim-server/src/receiver.rs` (rewrite to push to SharedInbound)
- `crates/sim-server/src/coordinator_client.rs` (rewrite to push to SharedCoordEvents)
- `crates/sim-server/src/network.rs` (rewrite to push client inputs to SharedInbound, read ClientSnapshot for broadcast)

**Description:**

This is the integration work item. Replace the `Arc<RwLock<World>>` pattern with the new ECS-based architecture.

**`main.rs` changes:**

Replace the current main function flow:

Old:
```
World::new -> Arc<RwLock<World>>
Shared via Arc to: game loop, receiver, coordinator_client, network
```

New:
```
SharedInbound -> Arc (for receiver, network, game loop)
SharedCoordEvents -> Arc (for coordinator_client, game loop)
broadcast::channel -> (game loop writes frames, network reads)
MetricsCollector -> Arc<Mutex> (game loop writes, metrics_server reads)
UdpSocket -> Arc (game loop sends, receiver receives)

Game loop owns the bevy_ecs::World exclusively.
```

The game loop is now the ONLY code that touches the ECS World. Async tasks interact through the shared queue buffers.

**`receiver.rs` changes:**

Rewrite `run_udp_receiver` to push events to `SharedInbound` instead of directly modifying `Arc<RwLock<World>>`:

```rust
pub async fn run_udp_receiver(
    udp_socket: Arc<UdpSocket>,
    inbound: Arc<SharedInbound>,
) {
    // On EntityStateUpdate: push PeerUpdate to inbound.queue
    // On AuthorityTransfer: push InboundTransfer to inbound.queue
    // On ForwardedInput: push ForwardedInput to inbound.queue
}
```

**`coordinator_client.rs` changes:**

Rewrite to push events to `SharedCoordEvents`:

```rust
pub async fn run_coordinator_client(
    coordinator_addr: String,
    server_id: u16,
    udp_port: u16,
    ws_port: u16,
    coord_events: Arc<SharedCoordEvents>,
    advertise_host: Option<String>,
    // Need entity count for heartbeat -- add a shared counter or query the game loop
    entity_count: Arc<AtomicU32>,
) {
    // On PeerList: push to coord_events.queue
    // On RebalanceDirective: push to coord_events.queue
    // On DrainDirective: push to coord_events.queue
    // On RecoverEntities: push to coord_events.queue
}
```

For the heartbeat entity count: add an `Arc<AtomicU32>` that the game loop updates each tick after counting authoritative entities. The coordinator client reads it for heartbeat messages.

**`network.rs` changes:**

Rewrite WebSocket handling:

- Player spawn: push SpawnPlayer client message to SharedInbound instead of directly spawning on World.
- Client input: push to SharedInbound instead of input_tx channel.
- Client disconnect (remove player): push RemoveEntity to SharedInbound.
- Broadcast: receive binary frames from the broadcast channel (game loop sends them after prepare_client_snapshot).

The broadcast ticker currently reads `world.read().await` to encode snapshots. Replace this: the game loop writes encoded frames to a broadcast channel, and the network task just forwards them.

For the PlayerAssigned message, the server needs to respond immediately when a client connects. Two options:
1. (Preferred) Generate the player UUID in the network task, send PlayerAssigned immediately, then push SpawnPlayer to SharedInbound. The player entity is spawned on the next tick. This introduces a 1-tick delay (50ms) before the player appears in the world, which is fine -- the client shows "connecting" during this time anyway.
2. (Alternative) Have the network task wait for confirmation from the game loop. More complex, not needed.

**Critical: Remove `Arc<RwLock<World>>`** from all function signatures. The old `World` struct still exists in `world.rs` but nothing references it anymore.

**Tests:**

This is primarily an integration test work item. After wiring everything up:

1. `cargo build -p sim-server` compiles with no warnings.
2. `cargo test -p sim-server` -- all new ECS tests pass.
3. Start the cluster with `./start.sh`, verify:
   - Entities appear in the client.
   - Clicking spawns NPCs.
   - Player assignment works (WASD movement).
   - Entities replicate between servers (check multiple browser tabs).
   - Authority transfers work (spawn many entities, watch rebalancing).
   - Server dashboards show metrics.
4. Run `./test.sh` -- all existing test scenarios pass.
5. Run `./test-scene.sh` -- all scene tests pass.

**Done when:**
- All async tasks use the new shared queue pattern.
- The old `Arc<RwLock<World>>` is removed from all function signatures.
- `./start.sh` launches the cluster and entities appear in the client.
- `./test.sh` passes all scenarios.
- `./test-scene.sh` passes all scene tests.
- Metrics dashboards work.

---

### WI-008: Remove old World struct and clean up

**Size:** medium (1-3 hours)
**Depends on:** WI-007
**Files:**
- `crates/sim-server/src/world.rs` (delete or gut)
- `crates/sim-server/src/simulation.rs` (delete or gut)
- `crates/sim-server/src/main.rs` (remove old mod declarations if modules deleted)
- All test files (update imports if needed)

**Description:**

Once WI-007 is verified working, remove the old code:

1. **`world.rs`** -- The old `World` struct and all its methods. If any utility functions (like `SHADOW_STALE_TICKS` constant) are still referenced, move them to a constants file or into the relevant system file.

2. **`simulation.rs`** -- The old `run_game_loop`, `process_input`, `simulate_tick`, `process_transfers`, `process_drain`. All of this logic now lives in ECS systems.

3. **Clean up `main.rs`** -- Remove the old module declarations. Verify the module list is clean.

4. **Update CLAUDE.md** -- Update the file descriptions to reflect the new structure:
   - `components.rs` -- ECS component definitions
   - `resources.rs` -- ECS resource definitions
   - `systems.rs` -- ECS system functions
   - `conversion.rs` -- EntityState <-> ECS component conversion
   - `schedule.rs` -- Schedule builder and tick schedule
   - `game_loop.rs` -- Async game loop that runs the ECS schedule
   - `encoding.rs` -- Binary frame encoding for client protocol

5. **Update `docs/mesh-improvements.md`** or create a note that the HashMap-to-ECS migration is complete.

**Tests:**

1. `cargo build -p sim-server` -- no dead code warnings related to old World/simulation.
2. `cargo test -p sim-server` -- all tests pass.
3. `./test.sh` -- full test suite passes.
4. Deploy to UNO Q boards (`./start-unoq.sh`), verify the mesh works on real hardware.

**Done when:**
- Old `world.rs` and `simulation.rs` are removed (or emptied of old logic).
- No dead code warnings.
- All tests pass locally and on UNO Q boards.
- CLAUDE.md updated with new file structure.

---

## Execution Order Summary

```
WI-001  Add bevy_ecs dependency          [small,  no deps]
  |
WI-002  Define components and resources   [medium, depends on 001]
  |
WI-003  EntityState conversion            [medium, depends on 002]
  |
  +--> WI-004  Core simulation systems    [large,  depends on 003]
  |
  +--> WI-005  Input/event systems        [large,  depends on 003]
         |
         v
WI-006  Schedule and game loop            [large,  depends on 004 + 005]
  |
WI-007  Wire up async tasks               [large,  depends on 006]
  |
WI-008  Remove old code, clean up         [medium, depends on 007]
```

WI-004 and WI-005 can be worked in parallel since they both depend only on WI-003 and produce independent system functions.

## Risk Mitigation

**Risk: bevy_ecs API changes between versions.**
Mitigation: Pin the exact version in Cargo.toml. Check the changelog before starting.

**Risk: Cross-compilation failure on ARM.**
Mitigation: WI-001 catches this immediately before any code is written. If it fails, investigate `bevy_ecs` feature flags or use a minimal fork.

**Risk: Performance regression from ECS overhead.**
Mitigation: Each system is benchmarkable independently. If a system takes too long, profile with `cargo flamegraph`. The conversion overhead can be measured with a simple timer.

**Risk: Behavioral divergence from old code.**
Mitigation: The existing test suite (`test.sh`, `test-scene.sh`) provides behavioral coverage. Run the full suite after WI-007 to catch divergences. Write system-level unit tests that mirror the old `world.rs` and `simulation.rs` test cases.

**Risk: Async-to-ECS bridge introduces race conditions.**
Mitigation: The swap pattern ensures only one side accesses the queue at a time. The World is only touched by the game loop thread. No concurrent mutation.
