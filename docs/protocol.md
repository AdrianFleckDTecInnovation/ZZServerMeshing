# Network Protocol Reference

## Transport Summary

| Path | Transport | Encoding | Rate |
|------|-----------|----------|------|
| Sim-Server ↔ Sim-Server | UDP | bincode (binary) | Every tick (20 Hz) |
| Sim-Server ↔ Coordinator | WebSocket | JSON | Events + 1s heartbeat |
| Sim-Server → Browser Client | WebSocket (WS/WSS) | Binary (47B/entity) | Every tick (20 Hz) |
| Browser Client → Sim-Server | WebSocket (WS/WSS) | JSON | On input |
| Coordinator → Browser Client | HTTPS | JSON | On assignment request |

Clients connect directly to sim-servers after getting an assignment from the coordinator's `/assign` endpoint. There is no intermediary — each sim-server broadcasts the full world view (authoritative + shadow entities) to its connected clients.

## Message Types

### Server → Browser Client (Binary WebSocket Frames)

Each tick, the server sends a binary frame with ALL entities (authoritative + shadows). See [API Reference](api-reference.md) for the full 47-byte binary entity format.

```
Header (27 bytes):
  Byte 0        u8     Frame type (0x01 = full, 0x02 = delta)
  Bytes 1-8     u64LE  Tick number
  Bytes 9-12    u32LE  Entity count
  Bytes 13-26   14B    Reserved / metrics

Then N entities, each 47 bytes:
  Bytes 0-15    16B    Entity UUID
  Bytes 16-27   3×f32  Position (x, y, z)
  Bytes 28-39   3×f32  Velocity (x, y, z)
  Byte  40      u8     Entity type (0=NPC, 1=Player)
  Bytes 41-42   u16LE  Authority server ID
  Bytes 43-46   u32LE  Last input sequence number
```

### Server → Browser Client (JSON Text Frames)

```json
// PlayerAssigned — sent on client connect
{
  "type": "PlayerAssigned",
  "player_id": "550e8400-e29b-41d4-a716-446655440000"
}

// Reconnect — sent when player authority transfers to another server
{
  "type": "Reconnect",
  "url": "wss://192.168.0.15:7878"
}
```

### Browser Client → Server (`ClientMessage`)

```json
// SpawnEntity — click to spawn
{ "type": "SpawnEntity", "position": [10.0, 0.0, -20.0], "entity_type": "npc" }

// MoveInput — WASD movement (with prediction sequence number)
{ "type": "MoveInput", "entity_id": "uuid-here", "direction": [1, 0, 0], "seq": 42 }

// RemoveEntity — delete an entity
{ "type": "RemoveEntity", "entity_id": "uuid-here" }
```

Input routing: if a client sends `MoveInput` for an entity owned by another server, the connected server automatically forwards it to the authority server via UDP.

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

ForwardedInput {
    entity_id: EntityId,
    direction: Vec3,
    seq: u32,
}
```

### Sim-Server → Coordinator (`CoordClientMsg`)

```json
{ "type": "Register", "server_id": 1, "udp_port": 9101, "ws_port": 7878, "tls": true }
{ "type": "Heartbeat", "server_id": 1, "entity_count": 42 }
{ "type": "Draining", "server_id": 1 }
```

The `tls` field tells the coordinator whether this server accepts WSS connections. The coordinator uses this to return `wss://` or `ws://` URLs in `/assign`.

### Coordinator → Sim-Server (`CoordServerMsg`)

```json
// Peer discovery
{ "type": "PeerList", "peers": [{ "server_id": 2, "udp_addr": "192.168.0.173:9102", "ws_addr": "192.168.0.173:7879" }] }

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
