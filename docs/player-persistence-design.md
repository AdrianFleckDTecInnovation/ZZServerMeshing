# Player Persistence Design

## Status: Planned

## Problem

There's no player identity that survives a reconnect. Each WebSocket connection creates a fresh UUID (`network.rs:288`), and disconnect deletes the entity (`network.rs:366`). The client has no way to resume as the same player. NPCs are cheap to regenerate; player state is not.

## Architecture: Coordinator as Player Authority

The coordinator already sits at the center of the mesh — it manages assignment, peer discovery, rebalancing, and crash detection. It's the only service that outlives any individual sim-server, making it the natural persistence gateway.

```
Client ──GET /assign?account=xyz──> Coordinator ──(SQLite)──> players.db
                                        |
                              "spawn this player at (50,0,30)
                               on server ws://unoq.local:7878"
                                        |
                                        v
                                    Sim-Server
                                  (authoritative,
                                   periodic flush back)
```

## Check-Out / Check-In Protocol

### Login (check-out)

1. Client sends `GET /assign?account_id=<persistent_id>` (stored in localStorage)
2. Coordinator looks up `account_id` in SQLite — finds last saved position, server, gameplay state
3. Coordinator picks target sim-server (prefer the one that last owned this player, else least-loaded)
4. Coordinator responds with `{ ws_url, account_id, spawn_position, player_state }`
5. Client connects to sim-server, sends `SpawnPlayer { account_id, position, state }` instead of server auto-spawning
6. Coordinator marks the account as "checked out" to that server

### Playing (periodic flush)

- Sim-server tracks dirty player entities (position changed since last flush)
- Every 5 seconds, batch-sends dirty player state to coordinator via the existing WebSocket: `PlayerFlush { account_id, position, velocity, gameplay_state }`
- Coordinator writes to SQLite — ~1ms per write, batched

### Disconnect (check-in)

- Sim-server detects WebSocket close, sends final `PlayerCheckIn { account_id, position, state }` to coordinator before deleting the entity
- Coordinator writes final state, marks account as "checked in" (available for re-login)

### Authority transfer during drain

- Before transferring a player entity, sim-server sends `PlayerCheckIn` to coordinator
- Target server receives entity via UDP transfer as today
- Target server sends `PlayerCheckOut { account_id }` to coordinator to claim ownership
- Coordinator updates the "checked out to" field

### Crash recovery

- Coordinator detects dead server, knows which accounts were checked out to it
- Shadow-based recovery promotes entity on a survivor (existing mechanism)
- Coordinator updates "checked out to" to the adopting server
- If no shadow exists, coordinator has the last flushed state (up to 5s old) — can re-spawn on a survivor

## Schema

```sql
CREATE TABLE players (
    account_id    TEXT PRIMARY KEY,
    display_name  TEXT,
    position_x    REAL NOT NULL DEFAULT 0,
    position_y    REAL NOT NULL DEFAULT 0,
    position_z    REAL NOT NULL DEFAULT 0,
    velocity_x    REAL NOT NULL DEFAULT 0,
    velocity_y    REAL NOT NULL DEFAULT 0,
    velocity_z    REAL NOT NULL DEFAULT 0,
    entity_type   TEXT NOT NULL DEFAULT 'Player',
    checked_out_to INTEGER,  -- server_id or NULL if not online
    last_flush_tick INTEGER NOT NULL DEFAULT 0,
    last_seen     TEXT NOT NULL DEFAULT (datetime('now')),
    -- Future gameplay state (health, inventory, level, etc.)
    -- JSON blob until specific fields stabilize, then promote to real columns
    gameplay_json TEXT NOT NULL DEFAULT '{}'
);
```

## Why SQLite

- Runs on the Mac where the coordinator already lives — no additional infrastructure
- Single-file database — trivial to back up, inspect, reset
- WAL mode gives concurrent reads during writes (coordinator serves `/assign` while flushing)
- Latency: <1ms per write for the flush path, <0.1ms for reads
- `rusqlite` is well-maintained, compiles everywhere including cross-compile targets
- Upgrade path: if we outgrow SQLite, the protocol doesn't change — swap the coordinator's storage backend to Postgres

## Write Frequency Analysis

| Event | Frequency | Durability | Latency budget |
|-------|-----------|-----------|----------------|
| Login (check-out) | ~1/min per player | Synchronous | 10-50ms ok |
| Position flush | Every 5s per player | Async batched | <5ms for batch |
| Gameplay state change | Event-driven | Synchronous | 5-10ms ok |
| Disconnect (check-in) | ~1/min per player | Synchronous | 10-50ms ok |
| Authority transfer | Rare | Synchronous | 10-50ms ok |

With 50 concurrent players flushing every 5s: 10 writes/sec to SQLite. Trivial.

## Protocol Additions

```rust
// Sim-Server -> Coordinator
CoordClientMsg::PlayerFlush {
    account_id: String,
    position: [f32; 3],
    velocity: [f32; 3],
    gameplay_state: String,  // JSON blob
}
CoordClientMsg::PlayerCheckIn {
    account_id: String,
    position: [f32; 3],
    gameplay_state: String,
}

// Coordinator -> Sim-Server
CoordServerMsg::SpawnPlayerDirective {
    account_id: String,
    entity_uuid: String,  // coordinator assigns stable UUID
    position: [f32; 3],
    gameplay_state: String,
}
```

## Codebase Changes Required

### Coordinator

- Add `rusqlite` dependency, create DB on startup
- `/assign` endpoint: look up or create account, return spawn position
- Handle `PlayerFlush` and `PlayerCheckIn` messages — write to SQLite
- On crash recovery: update `checked_out_to` for affected accounts

### Sim-server

- Remove auto-spawn in `network.rs` — wait for `SpawnPlayerDirective` from coordinator instead
- Add flush timer: every 5s, iterate player entities, send `PlayerFlush` for dirty ones
- On client disconnect: send `PlayerCheckIn` before deleting entity

### Client

- Generate stable `account_id` in localStorage on first visit
- Send `account_id` in the `/assign` request
- On `PlayerAssigned`, receive the entity UUID from the coordinator (not server-generated)

### NPCs stay ephemeral

NPCs don't need persistence — they're cheap to regenerate. Keep them in-memory-only with `spawn_initial_npcs()` on startup. This keeps the persistence scope small and the flush volume low.

## Data Loss Guarantees

| Failure | Player data loss |
|---------|-----------------|
| Client disconnect | None — check-in writes final state |
| Graceful server drain | None — check-in before transfer |
| Server crash | Up to 5s of position (last flush), gameplay state is synchronous |
| Coordinator crash | None — SQLite WAL survives process crash |
| Power loss on coordinator Mac | Last committed SQLite transaction (sub-second) |
| UNO Q board power loss | Up to 5s position, coordinator has last flush |

The 5s position loss on server crash is acceptable — players reappear close to where they were. Reduce flush interval to 1s for tighter guarantees (50 writes/sec with 50 players — still trivial for SQLite).

## Alternatives Considered

### Centralized Redis/Postgres
Every sim-server writes directly to a shared DB. Single source of truth but adds infrastructure dependency, doesn't fit UNO Q board setup, and every server needs a DB connection.

### Per-server embedded WAL (sled/RocksDB)
Each sim-server persists its own entities locally. Good for graceful restarts, but crash recovery is problematic (entities on dead disk). Authority transfer becomes a distributed transaction across two embedded DBs.

### Event sourcing
All mutations appended to a central ordered log, state derived by replay. Complete audit trail but high volume at 20Hz (10k events/sec with 500 entities). Requires compaction which is effectively periodic snapshots anyway. Overkill for prototype scale.

### Coordinator periodic world snapshots
Coordinator collects full snapshots from all servers every N seconds. Simplest approach but 5-30s data loss window and no player identity/session management. Good for NPC-only worlds, insufficient when player persistence matters.

The coordinator check-out/check-in approach was chosen because it provides player-first persistence with minimal infrastructure (SQLite), leverages the existing coordinator communication channel, and cleanly separates the hot path (position flush) from the critical path (login/logout/gameplay events).
