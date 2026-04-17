# 001: Bevy ECS Migration

## Problem Statement

The sim-server currently uses `HashMap<EntityId, EntityState>` for entity storage and a hand-rolled game loop with explicit phase ordering. This works for the current prototype (~2,500 LOC, hundreds of entities) but has structural limitations:

1. **Monolithic EntityState** -- Adding new fields (health, inventory, faction, cooldowns) means every entity carries every field, increasing serialization cost and making the struct unwieldy. New game systems require modifying the god struct.

2. **Manual phase ordering** -- The game loop in `simulation.rs` manually sequences input drain, simulation, transfers, replication, and shadow cleanup. Adding new systems (combat, abilities, AI state machines) means modifying one large function and reasoning about ordering by hand.

3. **No composition** -- All entities share the same flat struct. An NPC with patrol behavior and a Player with input prediction have identical data layouts. There is no way to attach behavior-specific data to subsets of entities.

4. **Query inefficiency** -- Finding "all NPCs" or "all authoritative players" requires scanning the entire HashMap with filter predicates. With ECS, these become typed queries that the World indexes.

Bevy ECS (`bevy_ecs` crate, standalone, no rendering/windowing) provides:

- Component-based entity storage with archetypal layout
- Typed queries with compile-time checked filters
- Systems as plain functions with declarative parameter signatures
- Schedules for ordering systems into phases
- Resources for global state (server config, tick counter, peer addresses)
- All of this without pulling in Bevy's rendering, windowing, or app runner -- just the ECS core

Now is the right time for this migration. The codebase is small enough that a structural change is tractable, and the ECS foundation will pay dividends as game systems multiply.

## Design

### Architecture Overview

```
+----------------------------------------------+
|              tokio async runtime              |
|                                               |
|  +------------------+  +-----------------+    |
|  | UDP Receiver     |  | Coordinator     |    |
|  | (async task)     |  | Client (async)  |    |
|  | writes to        |  | writes to       |    |
|  | InboundQueue     |  | CoordEventQueue |    |
|  +--------+---------+  +--------+--------+    |
|           |                      |             |
|           v                      v             |
|  +-------------------------------------------+|
|  |          bevy_ecs::World (sync)           ||
|  |                                           ||
|  |  Resources:                               ||
|  |    ServerConfig, TickCounter,             ||
|  |    PeerAddresses, InboundQueue,           ||
|  |    CoordEventQueue, OutboundReplication,  ||
|  |    DrainState, PendingTransfers,          ||
|  |    TimingAccumulator                      ||
|  |                                           ||
|  |  Entities:                                ||
|  |    [NetworkId, Position, Velocity,        ||
|  |     EntityKind, AuthorityServer,          ||
|  |     Authoritative | Shadow]               ||
|  |                                           ||
|  |  Schedule (run each tick):                ||
|  |    1. drain_inbound_events                ||
|  |    2. process_client_inputs               ||
|  |    3. process_coord_events                ||
|  |    4. npc_behavior                        ||
|  |    5. physics_integration                 ||
|  |    6. clamp_positions                     ||
|  |    7. process_transfers                   ||
|  |    8. process_drain                       ||
|  |    9. prepare_replication                 ||
|  |   10. prepare_client_snapshot             ||
|  |   11. shadow_cleanup                      ||
|  |   12. record_metrics                      ||
|  +-------------------------------------------+|
|           |                      |             |
|           v                      v             |
|  +------------------+  +-----------------+    |
|  | UDP Sender       |  | WebSocket       |    |
|  | (async task)     |  | Broadcast       |    |
|  | reads             |  | (async task)   |    |
|  | OutboundRepl     |  | reads Snapshot  |    |
|  +------------------+  +-----------------+    |
+----------------------------------------------+
```

The key insight: **bevy_ecs systems are sync functions**. They cannot do async I/O directly. Instead, async tasks (UDP receiver, coordinator client, WebSocket server) communicate with the ECS World through thread-safe queue Resources. The game loop remains a tokio interval that calls `schedule.run(&mut world)` each tick.

### Component Definitions

```rust
// crates/sim-server/src/components.rs

use bevy_ecs::prelude::*;
use glam::Vec3;
use shared::entity::{EntityId, EntityType, ServerId};

/// Network-visible UUID identity. Maps between Bevy Entity and network EntityId.
#[derive(Component, Debug, Clone, Copy)]
pub struct NetworkId(pub EntityId);

/// Spatial position in world coordinates.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Position(pub Vec3);

/// Movement velocity (units per second).
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Velocity(pub Vec3);

/// What kind of entity this is.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityKind(pub EntityType);

/// Which server is authoritative for this entity.
#[derive(Component, Debug, Clone, Copy)]
pub struct AuthorityServer(pub ServerId);

/// Marker: this server is authoritative for this entity.
#[derive(Component, Debug, Default)]
pub struct Authoritative;

/// Marker + data: this is a shadow copy from a peer server.
#[derive(Component, Debug)]
pub struct Shadow {
    pub last_seen_tick: u64,
}

/// Last acknowledged input sequence number. Only present on player entities.
#[derive(Component, Debug, Default)]
pub struct LastInputSeq(pub u32);
```

**Why this decomposition:**

- `Position` and `Velocity` are the most frequently accessed components (every tick for physics). Separating them means the physics system only pulls the columns it needs.
- `EntityKind` is a lightweight tag for branching behavior (NPC vs Player). It stays as a component rather than separate NPC/Player marker components because the wire protocol uses the same enum, making conversion trivial.
- `Authoritative` and `Shadow` are mutually exclusive markers. An entity is exactly one. This enables queries like `Query<..., With<Authoritative>>` and `Query<..., With<Shadow>>` with no runtime filtering.
- `LastInputSeq` is only added to player entities, not NPCs. This saves 4 bytes per NPC in the archetype and makes it clear at the type level that NPCs do not track input sequences.
- `NetworkId` maps the ECS-internal `Entity` handle to the stable UUID used across the network. A `HashMap<EntityId, Entity>` resource provides reverse lookup.

### Resource Definitions

```rust
// crates/sim-server/src/resources.rs

use bevy_ecs::prelude::*;
use shared::entity::{EntityId, ServerId};
use std::collections::HashMap;
use std::net::SocketAddr;

/// Server identity and configuration.
#[derive(Resource)]
pub struct ServerConfig {
    pub server_id: ServerId,
}

/// Monotonically increasing tick counter.
#[derive(Resource, Default)]
pub struct TickCounter(pub u64);

/// UDP addresses of peer servers.
#[derive(Resource, Default)]
pub struct PeerAddresses(pub HashMap<u16, SocketAddr>);

/// Reverse lookup: NetworkId (EntityId) -> Bevy Entity handle.
#[derive(Resource, Default)]
pub struct EntityLookup(pub HashMap<EntityId, bevy_ecs::entity::Entity>);

/// Queue of inbound events from async tasks (UDP receiver, WebSocket, coordinator).
/// Drained at the start of each tick.
#[derive(Resource, Default)]
pub struct InboundQueue {
    pub peer_updates: Vec<PeerUpdate>,
    pub authority_transfers: Vec<InboundTransfer>,
    pub forwarded_inputs: Vec<ForwardedInput>,
    pub client_inputs: Vec<shared::protocol::ClientMessage>,
}

/// Coordinator events received between ticks.
#[derive(Resource, Default)]
pub struct CoordEventQueue {
    pub peer_list_updates: Vec<Vec<shared::protocol::PeerInfo>>,
    pub rebalance_directives: Vec<(Vec<String>, u16)>,
    pub drain_directives: Vec<Vec<u16>>,
    pub recover_events: Vec<(u16, Vec<u16>)>,
}

/// Outbound replication data, written by prepare_replication system,
/// consumed by the async UDP sender task.
#[derive(Resource, Default)]
pub struct OutboundReplication {
    pub snapshot: Option<Vec<shared::entity::EntityState>>,
    pub tick: u64,
}

/// Outbound client snapshot, written by prepare_client_snapshot system,
/// consumed by the async WebSocket broadcast task.
#[derive(Resource, Default)]
pub struct ClientSnapshot {
    pub frame: Option<Vec<u8>>,
}

/// Pending authority transfers to send this tick.
#[derive(Resource, Default)]
pub struct PendingTransfers {
    pub transfers: Option<(Vec<String>, u16)>,
}

/// Outbound transfer messages to send via UDP.
#[derive(Resource, Default)]
pub struct OutboundTransfers {
    pub messages: Vec<(SocketAddr, Vec<u8>)>,
}

/// Outbound forwarded inputs to send via UDP.
#[derive(Resource, Default)]
pub struct OutboundForwards {
    pub messages: Vec<(SocketAddr, Vec<u8>)>,
}

/// Graceful drain state.
#[derive(Resource, Default)]
pub struct DrainState {
    pub draining: bool,
    pub targets: Option<Vec<u16>>,
}

/// Per-tick timing accumulator for metrics.
#[derive(Resource, Default)]
pub struct TimingAccumulator {
    pub phase_start: Option<std::time::Instant>,
    pub input_drain: std::time::Duration,
    pub simulation: std::time::Duration,
    pub transfers: std::time::Duration,
    pub replication: std::time::Duration,
    pub shadow_cleanup: std::time::Duration,
}

// -- Inbound event types --

pub struct PeerUpdate {
    pub tick: u64,
    pub server_id: ServerId,
    pub entities: Vec<shared::entity::EntityState>,
}

pub struct InboundTransfer {
    pub tick: u64,
    pub entity_id: EntityId,
    pub from_server: ServerId,
    pub to_server: ServerId,
    pub full_state: shared::entity::EntityState,
}

pub struct ForwardedInput {
    pub entity_id: EntityId,
    pub direction: [f32; 3],
    pub seq: u32,
}
```

### Async-to-ECS Bridge

The async tasks (UDP receiver, coordinator client, WebSocket server) cannot directly modify the ECS World. Instead, they push events into `Arc<Mutex<InboundQueue>>` / `Arc<Mutex<CoordEventQueue>>`, and the first system in the tick schedule drains these into the ECS Resources.

```
Async Task                     ECS World
-----------                    ----------
UDP Receiver
  -> Arc<Mutex<InboundQueue>>
                               drain_inbound_events system:
                               - swap(queue, empty)
                               - process peer updates: upsert shadow entities
                               - process transfers: promote to authoritative
                               - process forwarded inputs: apply to entities

Coordinator Client
  -> Arc<Mutex<CoordEventQueue>>
                               process_coord_events system:
                               - handle peer list updates
                               - handle rebalance directives
                               - handle drain directives
                               - handle recovery events

Game Loop (per tick):
  1. Lock InboundQueue, swap contents into ECS InboundQueue resource
  2. Lock CoordEventQueue, swap contents into ECS CoordEventQueue resource
  3. Run schedule (all sync systems)
  4. Lock OutboundReplication, copy data from ECS resource
  5. Lock ClientSnapshot, copy data from ECS resource
  6. Send UDP replication (async)
  7. Send client broadcast (async)
```

The `swap` pattern is critical: the async tasks write to one buffer while the ECS drains another. No lock is held during system execution.

### System Definitions and Schedule

Systems are plain Rust functions with Bevy ECS parameter signatures. They run in the order defined by the schedule.

```rust
// System signatures (pseudocode showing parameter types)

// Phase 1: Drain async queues into ECS
fn drain_inbound_events(
    mut commands: Commands,
    mut queue: ResMut<InboundQueue>,
    mut lookup: ResMut<EntityLookup>,
    config: Res<ServerConfig>,
    tick: Res<TickCounter>,
    auth_query: Query<&NetworkId, With<Authoritative>>,
    mut shadow_query: Query<
        (&NetworkId, &mut Position, &mut Velocity, &mut AuthorityServer, &mut Shadow),
        Without<Authoritative>,
    >,
)

// Phase 2: Process client inputs (WebSocket)
fn process_client_inputs(
    mut commands: Commands,
    mut queue: ResMut<InboundQueue>,
    config: Res<ServerConfig>,
    mut lookup: ResMut<EntityLookup>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound_fwd: ResMut<OutboundForwards>,
    mut auth_query: Query<
        (&NetworkId, &mut Velocity, &EntityKind, Option<&mut LastInputSeq>),
        With<Authoritative>,
    >,
    shadow_query: Query<(&NetworkId, &AuthorityServer), With<Shadow>>,
)

// Phase 3: Process coordinator events
fn process_coord_events(
    mut queue: ResMut<CoordEventQueue>,
    mut peer_addrs: ResMut<PeerAddresses>,
    mut pending: ResMut<PendingTransfers>,
    mut drain: ResMut<DrainState>,
    mut commands: Commands,
    mut lookup: ResMut<EntityLookup>,
    config: Res<ServerConfig>,
    shadow_query: Query<(Entity, &NetworkId, &AuthorityServer), With<Shadow>>,
)

// Phase 4: NPC behavior (wander)
fn npc_behavior(
    tick: Res<TickCounter>,
    mut query: Query<(&NetworkId, &mut Velocity), (With<Authoritative>, With<EntityKind>)>,
)
// Note: filter EntityKind == Npc checked at runtime, or use a dedicated NpcBehavior marker.

// Phase 5: Physics integration
fn physics_integration(
    mut query: Query<(&mut Position, &Velocity), With<Authoritative>>,
)

// Phase 6: Clamp positions
fn clamp_positions(
    mut query: Query<&mut Position, With<Authoritative>>,
)

// Phase 7: Process authority transfers
fn process_transfers(
    mut commands: Commands,
    mut pending: ResMut<PendingTransfers>,
    config: Res<ServerConfig>,
    tick: Res<TickCounter>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound: ResMut<OutboundTransfers>,
    mut lookup: ResMut<EntityLookup>,
    query: Query<
        (Entity, &NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer),
        With<Authoritative>,
    >,
)

// Phase 8: Process drain
fn process_drain(
    mut commands: Commands,
    drain: Res<DrainState>,
    config: Res<ServerConfig>,
    tick: Res<TickCounter>,
    peer_addrs: Res<PeerAddresses>,
    mut outbound: ResMut<OutboundTransfers>,
    mut lookup: ResMut<EntityLookup>,
    query: Query<
        (Entity, &NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer,
         Option<&LastInputSeq>),
        With<Authoritative>,
    >,
)

// Phase 9: Prepare replication data (EntityState vec for UDP sender)
fn prepare_replication(
    tick: Res<TickCounter>,
    config: Res<ServerConfig>,
    mut outbound: ResMut<OutboundReplication>,
    query: Query<
        (&NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer,
         Option<&LastInputSeq>),
        With<Authoritative>,
    >,
)

// Phase 10: Prepare client snapshot (binary frame for WebSocket broadcast)
fn prepare_client_snapshot(
    tick: Res<TickCounter>,
    mut snapshot: ResMut<ClientSnapshot>,
    auth_query: Query<
        (&NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer,
         Option<&LastInputSeq>),
        With<Authoritative>,
    >,
    shadow_query: Query<
        (&NetworkId, &Position, &Velocity, &EntityKind, &AuthorityServer,
         Option<&LastInputSeq>),
        With<Shadow>,
    >,
)

// Phase 11: Shadow cleanup
fn shadow_cleanup(
    mut commands: Commands,
    tick: Res<TickCounter>,
    mut lookup: ResMut<EntityLookup>,
    query: Query<(Entity, &NetworkId, &Shadow)>,
)

// Phase 12: Record metrics
fn record_metrics(
    mut timing: ResMut<TimingAccumulator>,
    tick: Res<TickCounter>,
    auth_query: Query<(), With<Authoritative>>,
    shadow_query: Query<(), With<Shadow>>,
    peer_addrs: Res<PeerAddresses>,
)
```

### Schedule Setup

```rust
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::ScheduleLabel;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TickSchedule;

fn build_schedule() -> Schedule {
    let mut schedule = Schedule::new(TickSchedule);

    schedule.add_systems((
        drain_inbound_events,
        process_client_inputs.after(drain_inbound_events),
        process_coord_events.after(drain_inbound_events),
        npc_behavior.after(process_client_inputs),
        physics_integration.after(npc_behavior),
        clamp_positions.after(physics_integration),
        process_transfers.after(clamp_positions),
        process_drain.after(process_transfers),
        prepare_replication.after(process_drain),
        prepare_client_snapshot.after(prepare_replication),
        shadow_cleanup.after(prepare_client_snapshot),
        record_metrics.after(shadow_cleanup),
    ));

    schedule
}
```

### EntityState Conversion (Network Boundary)

`EntityState` stays in `crates/shared/` unchanged. It remains the wire format for JSON, bincode, and binary snapshots. Conversion happens at two boundaries:

**Inbound (EntityState -> ECS Components):**

```rust
/// Spawn a new entity in the ECS World from an EntityState.
fn spawn_from_entity_state(
    commands: &mut Commands,
    lookup: &mut EntityLookup,
    state: &EntityState,
    is_authoritative: bool,
    shadow_tick: Option<u64>,
) -> bevy_ecs::entity::Entity {
    let mut entity_cmd = commands.spawn((
        NetworkId(state.id),
        Position(state.position),
        Velocity(state.velocity),
        EntityKind(state.entity_type),
        AuthorityServer(state.authority_server),
    ));

    if is_authoritative {
        entity_cmd.insert(Authoritative);
    } else {
        entity_cmd.insert(Shadow {
            last_seen_tick: shadow_tick.unwrap_or(0),
        });
    }

    if state.entity_type == EntityType::Player {
        entity_cmd.insert(LastInputSeq(state.last_input_seq));
    }

    let entity = entity_cmd.id();
    lookup.0.insert(state.id, entity);
    entity
}
```

**Outbound (ECS Components -> EntityState):**

```rust
/// Assemble an EntityState from ECS components for network serialization.
fn to_entity_state(
    network_id: &NetworkId,
    position: &Position,
    velocity: &Velocity,
    kind: &EntityKind,
    authority: &AuthorityServer,
    last_seq: Option<&LastInputSeq>,
) -> EntityState {
    EntityState {
        id: network_id.0,
        position: position.0,
        velocity: velocity.0,
        entity_type: kind.0,
        authority_server: authority.0,
        last_input_seq: last_seq.map_or(0, |s| s.0),
    }
}
```

This conversion is O(N) where N is the number of entities, but it happens at most twice per tick (replication + client snapshot). For 500 entities, this is negligible -- well under 0.1ms.

### Shadow Entity Handling in the Unified World

Both authoritative and shadow entities live in the same `bevy_ecs::World`, distinguished by marker components. This simplifies queries that need all entities (e.g., client snapshots) while still enabling queries scoped to just one type.

**Shadow upsert logic** (in `drain_inbound_events`):

```
for each entity in peer update:
    if entity.id exists in EntityLookup:
        let bevy_entity = lookup[entity.id]
        if bevy_entity has Authoritative:
            skip (don't overwrite our authoritative copy)
        if bevy_entity has Shadow:
            if peer_tick < shadow.last_seen_tick:
                skip (stale packet, out-of-order UDP)
            else:
                update Position, Velocity, AuthorityServer
                shadow.last_seen_tick = peer_tick
    else:
        spawn new entity with Shadow marker
```

**Authority transfer acceptance** (shadow -> authoritative):

```
for each transfer in inbound_transfers:
    if transfer.to_server != our server_id:
        skip
    if entity.id in lookup AND has Authoritative:
        skip (duplicate)
    if entity.id in lookup AND has Shadow:
        remove Shadow, add Authoritative
        update all components from transfer.full_state
    else:
        spawn new entity with Authoritative marker
```

### Integration with Tokio Async Runtime

The game loop remains an async function running on the tokio runtime. Each tick:

```rust
pub async fn run_game_loop(
    world: Arc<std::sync::Mutex<bevy_ecs::world::World>>,
    schedule: Arc<std::sync::Mutex<Schedule>>,
    inbound_queue: Arc<tokio::sync::Mutex<InboundQueueBuffer>>,
    coord_queue: Arc<tokio::sync::Mutex<CoordEventQueueBuffer>>,
    udp_socket: Arc<UdpSocket>,
    broadcast_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    metrics: Arc<tokio::sync::Mutex<MetricsCollector>>,
) {
    let mut interval = tokio::time::interval(TICK_DURATION);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        let tick_start = Instant::now();

        // 1. Swap inbound data from async buffers into ECS resources
        {
            let mut iq = inbound_queue.lock().await;
            let mut world = world.lock().unwrap();
            let mut ecs_queue = world.resource_mut::<InboundQueue>();
            std::mem::swap(&mut *ecs_queue, &mut iq.pending);
        }
        {
            let mut cq = coord_queue.lock().await;
            let mut world = world.lock().unwrap();
            let mut ecs_queue = world.resource_mut::<CoordEventQueue>();
            std::mem::swap(&mut *ecs_queue, &mut cq.pending);
        }

        // 2. Run all ECS systems (sync)
        {
            let mut world = world.lock().unwrap();
            let mut schedule = schedule.lock().unwrap();

            // Increment tick
            world.resource_mut::<TickCounter>().0 += 1;

            schedule.run(&mut world);
        }

        // 3. Extract outbound data and send (async)
        let (repl_data, client_frame, transfers, forwards);
        {
            let mut world = world.lock().unwrap();

            repl_data = world.resource_mut::<OutboundReplication>().snapshot.take();
            client_frame = world.resource_mut::<ClientSnapshot>().frame.take();
            transfers = std::mem::take(&mut world.resource_mut::<OutboundTransfers>().messages);
            forwards = std::mem::take(&mut world.resource_mut::<OutboundForwards>().messages);
        }

        // Send replication, transfers, forwards via UDP
        // Send client frame via broadcast channel
        // Record metrics
    }
}
```

**Important: `std::sync::Mutex` for the World, not `tokio::sync::Mutex`.**

The ECS World is accessed synchronously during schedule execution. Using `std::sync::Mutex` means the lock is never held across an `.await` point, which is correct behavior. The async tasks (UDP receiver, coordinator client) use separate `tokio::sync::Mutex` buffers that are swapped into the World at the start of each tick.

### Cross-Compilation for UNO Q Boards (ARM)

`bevy_ecs` is a pure Rust crate with no native dependencies. It compiles for all targets that Rust supports, including `aarch64-unknown-linux-gnu` (the UNO Q boards). No special cross-compilation flags are needed beyond what already works for the current sim-server.

The main consideration is binary size. `bevy_ecs` adds ~200-400KB to the release binary depending on features. The UNO Q boards have 4GB RAM and gigabit ethernet -- this is not a concern.

### Performance Budget

Current tick budget: 50ms at 20Hz.

Per-tick cost changes with ECS:

| Operation | HashMap (current) | Bevy ECS (new) | Notes |
|---|---|---|---|
| Entity lookup by ID | O(1) HashMap | O(1) HashMap (EntityLookup resource) | Same |
| Iterate all auth entities | O(N) HashMap values | O(N) archetype iteration | ECS is faster due to cache-friendly layout |
| Iterate NPCs only | O(N) with filter | O(M) where M = NPC count | ECS only touches the NPC archetype |
| Spawn entity | O(1) HashMap insert | O(1) archetype insert + lookup insert | Slightly more bookkeeping |
| Despawn entity | O(1) HashMap remove | O(1) + lookup remove | Slightly more bookkeeping |
| Component write | Direct field access | Component column write | Similar, ECS has slightly more indirection |
| EntityState conversion | N/A (it IS EntityState) | O(N) per boundary | New cost, but < 0.1ms for 500 entities |

The ECS adds a small constant overhead per tick for schedule execution and query setup, estimated at 0.01-0.05ms. For 500 entities, archetype iteration should be faster than HashMap iteration due to better cache locality. The conversion overhead at boundaries is the main new cost, but at ~200ns per entity, 500 entities costs ~0.1ms.

**Net impact: negligible. Well within the 50ms budget.**

### Failure Modes

1. **Entity not found in EntityLookup** -- A NetworkId received from the network has no matching Bevy Entity. This happens when a transfer or update arrives for an entity we haven't seen. Solution: spawn a new entity (for shadow updates) or log and skip (for inputs targeting unknown entities). Same as current behavior.

2. **Duplicate EntityLookup entries** -- Two Bevy Entities claim the same NetworkId. This is a bug. Guard against it by checking the lookup before inserting. If a collision is detected, despawn the stale entity.

3. **Command buffer deferred execution** -- Bevy's `Commands` are deferred: entity spawns/despawns happen when commands are applied. This means a system that spawns an entity cannot query it in the same system. Solution: order systems so that spawning (drain_inbound_events) happens before systems that need to query those entities. The schedule ordering handles this.

4. **Archetype fragmentation** -- Adding/removing components (e.g., switching from Shadow to Authoritative) causes archetype moves, which are O(1) but involve a memcpy. At current entity counts (hundreds), this is negligible. At 10,000+, profile and consider using a stable authority tag component instead of marker swap.

5. **Schedule panics** -- If a system panics, the entire schedule run panics. The game loop catches this and logs an error. This is the same as the current behavior where a panic in `simulate_tick` would crash the server.

## Testing Plan

### Unit Tests

1. **Component creation and access** -- Spawn entities with components, verify they can be queried.
2. **EntityState roundtrip** -- `EntityState -> components -> EntityState` preserves all fields.
3. **Shadow upsert** -- Peer update creates shadow, stale tick is rejected, newer tick updates.
4. **Authority transfer** -- Shadow promoted to authoritative, components updated correctly.
5. **EntityLookup consistency** -- After spawn/despawn, the lookup is accurate.
6. **NPC behavior system** -- NPCs change velocity at correct intervals.
7. **Physics integration** -- Position += velocity * dt for authoritative entities only.
8. **Position clamping** -- Entities outside world bounds are clamped.
9. **Shadow cleanup** -- Stale shadows despawned after SHADOW_STALE_TICKS.
10. **Client snapshot** -- Merges authoritative + shadow, authoritative takes precedence for same ID.
11. **Replication output** -- Only authoritative entities are included.
12. **Transfer system** -- Selects NPCs, never players; transfers at most 10%; produces correct wire messages.
13. **Drain system** -- All entities transferred round-robin; empty world triggers shutdown.
14. **Crash recovery** -- Shadow entities from dead server promoted by correct survivor.

### Integration Tests

1. **Full tick cycle** -- Insert entities and inputs, run schedule, verify outputs match expected state.
2. **Multi-tick simulation** -- Run 100 ticks, verify NPC positions change, shadows age correctly.
3. **Transfer + replication** -- Transfer an entity, verify it appears in next replication output from the target and disappears from source.

### Existing Test Preservation

All existing scene tests (`test.sh`, `test-scene.sh`) must pass unchanged. The client and coordinator see the same wire protocol. The only change is internal to the sim-server.

## Migration Strategy

The migration is done in 8 work items, each independently testable. The strategy is:

1. **Add bevy_ecs dependency** (no behavior change)
2. **Define components and resources** (new files, no existing code changes)
3. **Build EntityState conversion functions** (new code with unit tests)
4. **Implement systems one at a time**, each as a new function
5. **Build the schedule and new game loop**
6. **Wire up async-to-ECS bridge** (replace Arc<RwLock<World>> with the new queue pattern)
7. **Remove old World struct and simulation.rs** once all systems are ported
8. **Run full test suite** to verify behavioral equivalence

At no point is the server in a broken state. Each work item can be committed and tested independently. The old `World` struct and `simulation.rs` are only removed in the final work item, after everything else is verified.
