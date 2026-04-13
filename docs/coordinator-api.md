# Coordinator API Reference

The coordinator exposes both a WebSocket interface (for sim-server registration) and an HTTP(S) interface (for client assignment, monitoring, and serving the web client).

## HTTP API

Default port: **7871**. Serves plain HTTP locally, HTTPS with TLS on the board mesh.

### `GET /assign`

Returns the least-loaded sim-server for a new client connection.

**Response:**
```json
{"ws_url": "wss://192.168.0.15:7878"}
```

Returns `ws://` for non-TLS servers, `wss://` for TLS-enabled servers.

**503 response** (no servers available):
```json
{"error": "no servers available"}
```

**Host-aware routing:** The coordinator reads the `Host` header and uses it to construct the `ws_url` with a hostname the client can reach. When accessed via `localhost`, it returns `ws://localhost:port`.

### `GET /servers`

Returns the full list of registered sim-servers with their current state.

**Response:**
```json
{
  "servers": [
    {
      "server_id": 1,
      "host": "192.168.0.15",
      "ws_port": 7878,
      "udp_port": 9101,
      "metrics_port": 8101,
      "entity_count": 15
    },
    {
      "server_id": 2,
      "host": "192.168.0.173",
      "ws_port": 7879,
      "udp_port": 9102,
      "metrics_port": 8102,
      "entity_count": 15
    }
  ]
}
```

### `GET /dashboard`

Returns the coordinator's built-in HTML dashboard. Auto-refreshes every second. Shows:
- Server count, total entities, total shadows, tick drift
- Per-server cards with budget gauge, entity counts, peer status
- Phase timing breakdown (input, simulation, transfers, replication, shadow cleanup)

The dashboard fetches metrics directly from each sim-server's metrics endpoint.

### `GET /` (with `--static-dir`)

When launched with `--static-dir <path>`, serves the built Vite client app. Requests for unknown paths fall back to `index.html` (SPA routing). Without `--static-dir`, `/` serves the dashboard.

## Sim-Server Metrics API

Each sim-server exposes a metrics endpoint on its metrics port (default: 8100 + server_id).

### `GET /metrics`

**Response:**
```json
{
  "server_id": 1,
  "tick": 53580,
  "entity_count": 15,
  "shadow_count": 31,
  "peer_count": 2,
  "tick_budget_pct": 1.17,
  "phases": {
    "input_drain_ms": { "p50": 0.010, "p95": 0.013, "p99": 0.019 },
    "simulation_ms": { "p50": 0.241, "p95": 0.253, "p99": 0.292 },
    "transfers_ms": { "p50": 0.250, "p95": 0.285, "p99": 0.315 },
    "replication_ms": { "p50": 0.004, "p95": 0.006, "p99": 0.010 },
    "shadow_cleanup_ms": { "p50": 0.0, "p95": 0.0, "p99": 0.0 },
    "total_ms": { "p50": 0.541, "p95": 0.587, "p99": 0.617 }
  }
}
```

| Field | Description |
|-------|-------------|
| `server_id` | This server's ID (u16) |
| `tick` | Current simulation tick |
| `entity_count` | Number of authoritative entities |
| `shadow_count` | Number of shadow (replicated) entities |
| `peer_count` | Number of connected peer sim-servers |
| `tick_budget_pct` | Percentage of the 50ms tick budget used (p95) |
| `phases` | Per-phase timing with p50/p95/p99 percentiles in milliseconds |

### Phase descriptions

| Phase | What it measures |
|-------|-----------------|
| `input_drain_ms` | Processing queued client inputs (MoveInput, SpawnEntity) |
| `simulation_ms` | Entity simulation (NPC wandering, physics, movement) |
| `transfers_ms` | Authority transfer processing (inbound + outbound) |
| `replication_ms` | Sending entity state to peers via UDP |
| `shadow_cleanup_ms` | Removing stale shadow entities (>5s without update) |
| `total_ms` | Sum of all phases |

## Board Dashboard API

Each UNO Q board runs a dashboard on port 3000.

### `GET /api/mesh/status`

**Response:**
```json
{
  "running": true,
  "metrics": {
    "server_id": 1,
    "tick": 53580,
    "entity_count": 15,
    "shadow_count": 31,
    "peer_count": 2,
    "tick_budget_pct": 1.17,
    "phases": { ... }
  }
}
```

When the sim-server isn't running: `{"running": false, "metrics": null}`

### `POST /api/mesh/start`

Starts the sim-server process using the configured `MESH_BINARY` and `MESH_ARGS` environment variables (set via systemd override).

### `POST /api/mesh/stop`

Stops the running sim-server process.

## CLI Flags

### Coordinator

```
coordinator [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | 7870 | WebSocket port for sim-server connections |
| `--http-port` | 7871 | HTTP(S) port for API, dashboard, and client serving |
| `--static-dir` | — | Directory to serve client files from |
| `--tls-cert` | — | TLS certificate file (PEM). Enables HTTPS when paired with `--tls-key` |
| `--tls-key` | — | TLS private key file (PEM) |

### Sim-Server

```
sim-server [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--server-id` | required | Unique server ID (u16, starting at 1) |
| `--port` | required | WebSocket port for client connections |
| `--udp-port` | required | UDP port for peer entity replication |
| `--metrics-port` | 8101 | HTTP port for metrics endpoint |
| `--coordinator` | required | Coordinator address (`host:port`) |
| `--advertise-host` | — | IP address to advertise to coordinator (for LAN routing) |
| `--initial-npcs` | 0 | Number of NPCs to spawn on startup |
| `--tls-cert` | — | TLS certificate file (PEM). Enables WSS when paired with `--tls-key` |
| `--tls-key` | — | TLS private key file (PEM) |
