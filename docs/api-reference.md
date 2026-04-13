# UNO Q Game Server API Guide

API reference for the game servers running on the UNO Q boards. Use this to build dashboards, monitoring tools, or game clients that talk to the mesh servers.

See also: [Coordinator API Reference](coordinator-api.md) for the coordinator's HTTP API and CLI flags.

## The Mesh

| Device | Hostname | IP | Role | Ports |
|---|---|---|---|---|
| Pi Zero W2 | zzttpi.local | 192.168.0.208 | Coordinator + Client | ws:7870, https:7871 |
| UNO Q #1 | unoq.local | 192.168.0.15 | Sim-server S1 | wss:7878, udp:9101, metrics:8101, dash:3000 |
| UNO Q #2 | unoq2.local | 192.168.0.173 | Sim-server S2 | wss:7879, udp:9102, metrics:8102, dash:3000 |
| UNO Q #3 | unoq3.local | 192.168.0.86 | Sim-server S3 | wss:7880, udp:9103, metrics:8103, dash:3000 |

Each board runs:
- **sim-server** — the game server (Rust binary at `~/mesh/sim-server`)
- **board-dash** — system + game dashboard on port 3000 (Rust binary at `~/dashboard/unoq-dashboard`, systemd service `dashboard.service`)

The three game servers form a mesh — they replicate entity state to each other over UDP. Each server owns some entities ("authoritative") and holds read-only copies of other servers' entities ("shadows"). A client connecting to any server sees the full world.

When TLS is enabled (board mesh), sim-servers use WSS and the coordinator returns `wss://` URLs from `/assign`.

## Quick Test

```bash
# Coordinator — list all servers
curl -k https://192.168.0.208:7871/servers | jq .

# Get assigned to a server
curl -k https://192.168.0.208:7871/assign

# Check each server's metrics
curl http://192.168.0.15:8101/metrics | jq .tick
curl http://192.168.0.173:8102/metrics | jq .tick
curl http://192.168.0.86:8103/metrics | jq .tick

# Dashboard status (includes game server + system health)
curl http://192.168.0.15:3000/api/mesh/status | jq .

# Start/stop the game server from the dashboard
curl -X POST http://192.168.0.15:3000/api/mesh/start
curl -X POST http://192.168.0.15:3000/api/mesh/stop
```

---

## Game Server Metrics API

Each game server exposes metrics over HTTP. S1 on port 8101, S2 on port 8102.

### `GET /metrics`

```bash
curl http://192.168.0.15:8101/metrics
```

Returns the full server state snapshot:

```json
{
  "server_id": 1,
  "tick": 20239,
  "entity_count": 13,
  "shadow_count": 18,
  "peer_count": 1,
  "tick_budget_pct": 0.57,
  "phases": {
    "input_drain_ms":    {"p50": 0.003, "p95": 0.005, "p99": 0.007},
    "simulation_ms":     {"p50": 0.035, "p95": 0.046, "p99": 0.072},
    "transfers_ms":      {"p50": 0.001, "p95": 0.002, "p99": 0.005},
    "replication_ms":    {"p50": 0.181, "p95": 0.235, "p99": 0.278},
    "shadow_cleanup_ms": {"p50": 0.004, "p95": 0.005, "p99": 0.008},
    "total_ms":          {"p50": 0.232, "p95": 0.287, "p99": 0.351}
  }
}
```

#### Field Reference

| Field | Type | Description |
|---|---|---|
| `server_id` | number | Server ID (1, 2, or 3) |
| `tick` | number | Current simulation tick. Increments 20 times per second (20Hz). |
| `entity_count` | number | Entities this server is authoritative for (owns and simulates). |
| `shadow_count` | number | Read-only copies of entities from other servers. Used for world visibility and crash recovery. |
| `peer_count` | number | Connected peer game servers. Should be 2 in a 3-server mesh. |
| `tick_budget_pct` | number | How much of the 50ms tick budget is being used (p95). Under 50% is healthy. Over 80% means the server is struggling. |

#### Phase Timing

Each tick runs 5 phases in order. All timing values are **milliseconds**. Percentiles are computed over the last 200 ticks (~10 seconds).

| Phase | What it does |
|---|---|
| `input_drain_ms` | Process queued player inputs (MoveInput, SpawnEntity, etc.) |
| `simulation_ms` | Physics integration (position += velocity * dt), NPC AI (direction changes every 3s) |
| `transfers_ms` | Authority transfers from rebalancing — moving entities between servers |
| `replication_ms` | UDP broadcast of all authoritative entity state to peer servers |
| `shadow_cleanup_ms` | Remove shadow entities not updated in 5 seconds (stale = peer probably dead) |
| `total_ms` | Wall clock time for the entire tick |

**How to interpret:** If `replication_ms.p95` is high, the server is spending most of its budget sending UDP. If `simulation_ms` climbs, you have too many entities or expensive AI. `tick_budget_pct` over 80 means the server can't keep up with 20Hz and will start skipping ticks.

### `GET /`

Returns the built-in HTML dashboard with a gauge and sparkline chart. Open in a browser:

```
http://192.168.0.15:8101/
http://192.168.0.173:8102/
```

---

## Game Server WebSocket Protocol

Clients connect to the game server via WebSocket to receive entity state and send input. With TLS: `wss://192.168.0.15:7878`, without: `ws://localhost:7878`.

Use the coordinator's `/assign` endpoint to get the best server to connect to.

### Connection Flow

```
1. Client fetches /assign from coordinator → gets ws[s]://host:port
2. Client opens WebSocket to the assigned server
3. Server creates a player entity at (0,0,0)
4. Server sends JSON text frame:  {"type":"PlayerAssigned","player_id":"<uuid>"}
5. Server begins streaming binary frames at 20Hz (full world snapshot every tick)
6. Client sends JSON text frames for input
7. On disconnect, server deletes the player entity
```

### Receiving Data: Binary Frames (Server → Client)

Every tick (50ms), the server sends a binary WebSocket frame containing ALL entities in the world (this server's authoritative entities + shadow copies from peers).

#### Frame Layout

```
Header (27 bytes):
  Byte 0        u8     Frame type = 0x01 (full snapshot)
  Bytes 1-8     u64LE  Tick number
  Bytes 9-12    u32LE  Entity count
  Bytes 13-26   14B    Reserved (zeroed)

Then N entities, each 47 bytes:
  Bytes 0-15    16B    Entity UUID (raw bytes)
  Bytes 16-19   f32LE  Position X
  Bytes 20-23   f32LE  Position Y (always 0 — flat world)
  Bytes 24-27   f32LE  Position Z
  Bytes 28-31   f32LE  Velocity X
  Bytes 32-35   f32LE  Velocity Y (always 0)
  Bytes 36-39   f32LE  Velocity Z
  Byte  40      u8     Entity type: 0 = NPC, 1 = Player
  Bytes 41-42   u16LE  Authority server ID (which server owns this entity)
  Bytes 43-46   u32LE  Last input sequence number (for client prediction, 0 for NPCs)
```

Total frame size: `27 + entity_count * 47` bytes.

With 30 entities, that's 27 + 1410 = **1437 bytes per frame**, 20 frames/sec = ~28 KB/s.

#### Decoding Example (JavaScript)

```javascript
// Set up connection
const ws = new WebSocket('ws://192.168.0.15:7878');
ws.binaryType = 'arraybuffer';

// UUID decoder
const HEX = Array.from({length: 256}, (_, i) => i.toString(16).padStart(2, '0'));
function uuidFromBytes(bytes, off) {
  return (
    HEX[bytes[off]]   + HEX[bytes[off+1]] +
    HEX[bytes[off+2]] + HEX[bytes[off+3]] + '-' +
    HEX[bytes[off+4]] + HEX[bytes[off+5]] + '-' +
    HEX[bytes[off+6]] + HEX[bytes[off+7]] + '-' +
    HEX[bytes[off+8]] + HEX[bytes[off+9]] + '-' +
    HEX[bytes[off+10]] + HEX[bytes[off+11]] +
    HEX[bytes[off+12]] + HEX[bytes[off+13]] +
    HEX[bytes[off+14]] + HEX[bytes[off+15]]
  );
}

// Entity decoder
function readEntity(view, bytes, offset) {
  return {
    id: uuidFromBytes(bytes, offset),
    position: {
      x: view.getFloat32(offset + 16, true),
      y: view.getFloat32(offset + 20, true),
      z: view.getFloat32(offset + 24, true),
    },
    velocity: {
      x: view.getFloat32(offset + 28, true),
      y: view.getFloat32(offset + 32, true),
      z: view.getFloat32(offset + 36, true),
    },
    entityType: bytes[offset + 40] === 1 ? 'Player' : 'Npc',
    authorityServer: view.getUint16(offset + 41, true),
    lastInputSeq: view.getUint32(offset + 43, true),
  };
}

// Frame decoder
function decodeFrame(buffer) {
  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);
  if (bytes[0] !== 0x01) return null; // only handle full snapshots

  const tick = view.getUint32(1, true) + view.getUint32(5, true) * 0x100000000;
  const count = view.getUint32(9, true);
  const entities = [];

  let offset = 27;
  for (let i = 0; i < count; i++) {
    entities.push(readEntity(view, bytes, offset));
    offset += 47;
  }

  return { tick, entities };
}

// Handle messages
let playerId = null;

ws.onmessage = (event) => {
  if (event.data instanceof ArrayBuffer) {
    // Binary frame — entity snapshot
    const frame = decodeFrame(event.data);
    if (frame) {
      console.log(`Tick ${frame.tick}: ${frame.entities.length} entities`);
      // Do something with frame.entities...
    }
  } else {
    // JSON text frame
    const msg = JSON.parse(event.data);
    if (msg.type === 'PlayerAssigned') {
      playerId = msg.player_id;
      console.log('My player:', playerId);
    }
  }
};
```

#### Decoding Example (Python)

```python
import websocket
import struct
import json

def decode_frame(data):
    if len(data) < 27 or data[0] != 0x01:
        return None
    tick = struct.unpack_from('<Q', data, 1)[0]
    count = struct.unpack_from('<I', data, 9)[0]
    entities = []
    offset = 27
    for _ in range(count):
        uuid_bytes = data[offset:offset+16]
        eid = uuid_bytes.hex()
        eid = f"{eid[:8]}-{eid[8:12]}-{eid[12:16]}-{eid[16:20]}-{eid[20:]}"
        px, py, pz = struct.unpack_from('<fff', data, offset+16)
        vx, vy, vz = struct.unpack_from('<fff', data, offset+28)
        etype = 'Player' if data[offset+40] == 1 else 'Npc'
        auth = struct.unpack_from('<H', data, offset+41)[0]
        seq = struct.unpack_from('<I', data, offset+43)[0]
        entities.append({
            'id': eid, 'position': [px,py,pz], 'velocity': [vx,vy,vz],
            'entity_type': etype, 'authority_server': auth, 'last_input_seq': seq,
        })
        offset += 47
    return {'tick': tick, 'entities': entities}

ws = websocket.WebSocket()
ws.connect('ws://192.168.0.15:7878')

while True:
    opcode, data = ws.recv_data()
    if opcode == websocket.ABNF.OPCODE_BINARY:
        frame = decode_frame(data)
        if frame:
            print(f"tick={frame['tick']} entities={len(frame['entities'])}")
    elif opcode == websocket.ABNF.OPCODE_TEXT:
        msg = json.loads(data.decode())
        if msg['type'] == 'PlayerAssigned':
            print(f"My player: {msg['player_id']}")
```

### Sending Input: JSON Text Frames (Client → Server)

#### `SpawnEntity` — Create an NPC or Player

```json
{"type": "SpawnEntity", "position": [100.0, 0.0, -50.0], "entity_type": "npc"}
```

| Field | Type | Values |
|---|---|---|
| `position` | [f32, f32, f32] | World coordinates. X and Z range is -500 to 500. Y is always 0. |
| `entity_type` | string | `"npc"` or `"player"` |

#### `MoveInput` — Move an Entity

```json
{"type": "MoveInput", "entity_id": "550e8400-e29b-41d4-a716-446655440000", "direction": [1.0, 0.0, -1.0], "seq": 42}
```

| Field | Type | Description |
|---|---|---|
| `entity_id` | string | UUID of the entity to move |
| `direction` | [f32, f32, f32] | Movement direction. Server normalizes this and multiplies by speed (5.0 for players, 2.0 for NPCs). Y is ignored. |
| `seq` | number | Input sequence number for client-side prediction. The server echoes this back as `last_input_seq` in the entity's binary data. Optional — defaults to 0. |

**Cross-server input forwarding:** If you send MoveInput for an entity owned by the OTHER server, it works — the connected server automatically forwards the input via UDP to the authority server. The entity's velocity changes on the next tick and the update propagates back via replication.

#### `RemoveEntity` — Delete an Entity

```json
{"type": "RemoveEntity", "entity_id": "550e8400-e29b-41d4-a716-446655440000"}
```

Only removes entities that the connected server is authoritative for. Silently ignored for entities on other servers.

---

## Board Dashboard API (port 3000)

The dashboard runs on each board as a systemd service. It provides system health monitoring AND game server control.

### `GET /api/mesh/status`

**This is the main endpoint for game server monitoring.** Returns whether the game server is running, plus its full metrics if available.

```bash
curl http://192.168.0.15:3000/api/mesh/status
```

**Running with metrics:**
```json
{
  "running": true,
  "metrics": {
    "server_id": 1,
    "tick": 20239,
    "entity_count": 13,
    "shadow_count": 18,
    "peer_count": 1,
    "tick_budget_pct": 0.57,
    "phases": {
      "input_drain_ms":    {"p50": 0.003, "p95": 0.005, "p99": 0.007},
      "simulation_ms":     {"p50": 0.035, "p95": 0.046, "p99": 0.072},
      "transfers_ms":      {"p50": 0.001, "p95": 0.002, "p99": 0.005},
      "replication_ms":    {"p50": 0.181, "p95": 0.235, "p99": 0.278},
      "shadow_cleanup_ms": {"p50": 0.004, "p95": 0.005, "p99": 0.008},
      "total_ms":          {"p50": 0.232, "p95": 0.287, "p99": 0.351}
    }
  }
}
```

**Running but metrics port not responding:**
```json
{"running": true, "metrics": null}
```

**Server not running:**
```json
{"running": false, "metrics": null}
```

The `metrics` object is the exact same data as `GET /metrics` on the game server's metrics port (8101/8102). The dashboard fetches it internally from `127.0.0.1`.

### `POST /api/mesh/start`

Start the game server process on this board.

```bash
curl -X POST http://192.168.0.15:3000/api/mesh/start
```

**Success:**
```json
{"ok": true, "pid": 12345}
```

**Already running:**
```json
{"ok": false, "error": "already running"}
```

**Binary not found:**
```json
{"ok": false, "error": "No such file or directory (os error 2)"}
```

The game server binary and arguments are configured via environment variables `MESH_BINARY` and `MESH_ARGS` on the dashboard systemd service. These are set by `start-unoq.sh` when deploying.

### `POST /api/mesh/stop`

Stop the game server process on this board.

```bash
curl -X POST http://192.168.0.15:3000/api/mesh/stop
```

**Success:**
```json
{"ok": true}
```

**Not running:**
```json
{"ok": false, "error": "not running or pkill failed"}
```

### `GET /api/stats`

Board system statistics — CPU, memory, temperature, network, uptime. Useful for correlating game server performance with system health.

```bash
curl http://192.168.0.15:3000/api/stats
```

```json
{
  "cpu": {
    "cores": [
      {"user": 35407, "nice": 29, "system": 55616, "idle": 7022181, "iowait": 207},
      {"user": 33731, "nice": 8, "system": 60048, "idle": 7027401, "iowait": 271},
      {"user": 37375, "nice": 0, "system": 45696, "idle": 7045818, "iowait": 304},
      {"user": 39533, "nice": 2, "system": 39705, "idle": 7054901, "iowait": 156}
    ]
  },
  "mem": {
    "total_mb": 3812,
    "used_mb": 1204,
    "free_mb": 890,
    "available_mb": 2608,
    "buffers_mb": 112,
    "cached_mb": 1406,
    "swap_total_mb": 100,
    "swap_used_mb": 0
  },
  "temp": 42.3,
  "load": [0.15, 0.22, 0.18],
  "net": [
    {"name": "eth0", "rx_bytes": 1024000, "tx_bytes": 512000},
    {"name": "wlan0", "rx_bytes": 0, "tx_bytes": 0}
  ],
  "uptime_secs": 86400.5
}
```

| Field | Type | Notes |
|---|---|---|
| `cpu.cores[].user/nice/system/idle/iowait` | number | Cumulative jiffies from `/proc/stat`. Take deltas between two polls to compute CPU %. |
| `mem.*_mb` | number | Memory in megabytes. `available_mb` is what matters for "how much room is left". |
| `temp` | number | CPU temperature in Celsius (max across all thermal zones). UNO Q boards throttle around 80C. |
| `load` | [f64, f64, f64] | 1-minute, 5-minute, 15-minute load averages. |
| `net[].rx_bytes/tx_bytes` | number | Cumulative bytes since boot. Take deltas between polls to compute throughput. |
| `uptime_secs` | number | System uptime in seconds. |

### Computing CPU Percentage from Two Polls

```javascript
async function getCpuPercent(boardIp) {
  const poll = async () => (await fetch(`http://${boardIp}:3000/api/stats`).then(r => r.json())).cpu.cores;
  const prev = await poll();
  await new Promise(r => setTimeout(r, 1000));
  const curr = await poll();

  let totalDelta = 0, idleDelta = 0;
  for (let i = 0; i < curr.length; i++) {
    const pTotal = prev[i].user + prev[i].nice + prev[i].system + prev[i].idle + prev[i].iowait;
    const cTotal = curr[i].user + curr[i].nice + curr[i].system + curr[i].idle + curr[i].iowait;
    totalDelta += cTotal - pTotal;
    idleDelta += (curr[i].idle + curr[i].iowait) - (prev[i].idle + prev[i].iowait);
  }
  return totalDelta > 0 ? ((totalDelta - idleDelta) / totalDelta) * 100 : 0;
}
```

---

## Entity Model

Every entity in the world has this state:

| Field | Type | Range | Description |
|---|---|---|---|
| `id` | UUID | v4 | Globally unique, assigned at spawn, never changes |
| `position` | [f32, f32, f32] | X,Z: -500..500, Y: always 0 | World position in units |
| `velocity` | [f32, f32, f32] | varies | Current velocity in units/second |
| `entity_type` | enum | `Npc` or `Player` | NPCs wander randomly. Players respond to MoveInput. |
| `authority_server` | u16 | 1..N | Which server owns and simulates this entity |
| `last_input_seq` | u32 | 0..2^32 | Last processed input sequence number (players only, 0 for NPCs) |

**World bounds:** X and Z axis from -500 to +500 (1000x1000 unit world). Y is always 0 (flat ground plane). Entities are clamped to bounds every tick.

**Tick rate:** 20Hz (50ms per tick). Entity positions update 20 times per second.

**NPC behavior:** NPCs pick a random direction and speed (1-3 units/sec) every ~3 seconds and walk in that direction.

**Player speed:** 5.0 units/second when given a MoveInput direction.

**Authority:** Each entity is owned by exactly one server. That server simulates it (updates position from velocity) and replicates it to peers. Other servers hold read-only shadow copies. Authority can transfer between servers during rebalancing.

---

## Monitoring Patterns

### Poll both boards every second

```javascript
const BOARDS = [
  { name: 'S1', ip: '192.168.0.15', metricsPort: 8101, dashPort: 3000 },
  { name: 'S2', ip: '192.168.0.173', metricsPort: 8102, dashPort: 3000 },
  { name: 'S3', ip: '192.168.0.86', metricsPort: 8103, dashPort: 3000 },
];

async function pollAll() {
  const results = await Promise.all(BOARDS.map(async (board) => {
    const [mesh, system] = await Promise.all([
      fetch(`http://${board.ip}:${board.dashPort}/api/mesh/status`).then(r => r.json()).catch(() => null),
      fetch(`http://${board.ip}:${board.dashPort}/api/stats`).then(r => r.json()).catch(() => null),
    ]);
    return { board: board.name, mesh, system };
  }));
  return results;
}

// Example output per board:
// { board: 'S1', mesh: { running: true, metrics: { tick: 20000, entity_count: 15, ... } }, system: { temp: 42, mem: {...}, ... } }
```

### Health check

```javascript
function isHealthy(meshStatus) {
  if (!meshStatus?.running) return 'down';
  if (!meshStatus.metrics) return 'no-metrics';
  if (meshStatus.metrics.peer_count === 0) return 'no-peers';
  if (meshStatus.metrics.tick_budget_pct > 80) return 'overloaded';
  return 'healthy';
}
```

### Watch for entity imbalance

```javascript
function checkBalance(metrics) {
  const counts = metrics.map(m => m.entity_count);
  const max = Math.max(...counts);
  const min = Math.max(Math.min(...counts), 1);
  const ratio = max / min;
  return { ratio, balanced: ratio < 2.0, counts };
}
```

---

## All CORS-enabled

Every endpoint on both the game server metrics port and the dashboard port returns `Access-Control-Allow-Origin: *`, so you can call them from any web page or tool without CORS issues.
