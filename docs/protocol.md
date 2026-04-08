# Network Protocol Reference

## Transport Summary

| Path | Transport | Encoding | Rate |
|------|-----------|----------|------|
| Sim-Server ↔ Sim-Server | UDP | bincode (binary) | Every tick (20 Hz) |
| Sim-Server ↔ Coordinator | WebSocket | JSON | Events + 1s heartbeat |
| Sim-Server → Gateway | WebSocket | JSON | Every tick (20 Hz) |
| Gateway → Browser Client | WebSocket | JSON | Every tick (20 Hz) |
| Browser Client → Gateway | WebSocket | JSON | On input |

## Message Types

### Server → Browser Client (`ServerMessage`)

```json
// WorldSnapshot — full entity state, sent every tick
{
  "type": "WorldSnapshot",
  "tick": 1234,
  "entities": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "position": [123.4, 0.0, -56.7],
      "velocity": [1.2, 0.0, -0.8],
      "entity_type": "Npc",
      "authority_server": 1
    }
  ],
  "_metrics": {                    // added by gateway
    "snapshotsPerSec": 20,
    "bandwidthKBps": 45,
    "rebalanceEvents": 3,
    "serverCount": 3
  }
}

// PlayerAssigned — sent by gateway on client connect
{
  "type": "PlayerAssigned",
  "player_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

### Browser Client / Gateway → Server (`ClientMessage`)

```json
// SpawnEntity — click to spawn
{ "type": "SpawnEntity", "position": [10.0, 0.0, -20.0], "entity_type": "npc" }

// SpawnPlayer — gateway assigns player with specific UUID
{ "type": "SpawnPlayer", "player_id": "uuid-here", "position": [0, 0, 0] }

// MoveInput — WASD movement
{ "type": "MoveInput", "entity_id": "uuid-here", "direction": [1, 0, 0] }

// RemoveEntity — gateway cleanup on disconnect
{ "type": "RemoveEntity", "entity_id": "uuid-here" }
```

### Sim-Server ↔ Sim-Server (`PeerMessage`, bincode over UDP)

```rust
EntityStateUpdate {
    tick: u64,
    server_id: ServerId,
    entities: Vec<EntityState>,   // all authoritative entities
}

AuthorityTransfer {
    tick: u64,
    entity_id: EntityId,
    from_server: ServerId,
    to_server: ServerId,
    full_state: EntityState,      // complete entity snapshot
}
```

### Sim-Server → Coordinator (`CoordClientMsg`)

```json
{ "type": "Register", "server_id": 1, "udp_port": 9101, "ws_port": 7878 }
{ "type": "Heartbeat", "server_id": 1, "entity_count": 42 }
{ "type": "Draining", "server_id": 1 }
```

### Coordinator → Sim-Server (`CoordServerMsg`)

```json
// Peer discovery
{ "type": "PeerList", "peers": [{ "server_id": 2, "udp_addr": "127.0.0.1:9102", "ws_addr": "127.0.0.1:7879" }] }

// Load rebalancing
{ "type": "RebalanceDirective", "entity_ids": [], "target_server": 2 }

// Graceful drain
{ "type": "DrainDirective", "target_servers": [2, 3] }

// Crash recovery
{ "type": "RecoverEntities", "dead_server_id": 1 }
```

## Conflict Resolution

- **UDP packet ordering**: `apply_peer_update()` compares incoming tick to stored `shadow_last_seen` — discards if older
- **Authority transfer**: validates `to_server == our_server_id`, rejects duplicates (entity already authoritative locally)
- **Shadow staleness**: entities not updated in 100 ticks (5 seconds) are removed
