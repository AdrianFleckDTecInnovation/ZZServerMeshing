# Network Saturation Telemetry

New fields added to the `/metrics` endpoint on each sim-server for detecting WiFi/UDP saturation.

## Endpoints

Each sim-server exposes metrics at:
- Local: `http://localhost:810N/metrics` (where N = server number)
- UNO Q boards: `http://<board-ip>:810N/metrics`

## New Fields

### Outbound UDP Health

| Field | Type | Description |
|-------|------|-------------|
| `udp_repl_drops` | `u64` | Replication channel drops in the current 5-second window. Non-zero means outbound UDP is saturated. |
| `udp_ctrl_drops` | `u64` | Control channel drops (authority transfers, forwarded inputs). Non-zero means latency-critical messages are being lost. |
| `udp_bytes_out` | `u64` | Bytes enqueued for UDP send this tick. Multiply by 20 for bytes/second estimate. |

### Per-Peer Inbound Health

The `peer_health` field is a map keyed by peer server_id:

| Field | Type | Description |
|-------|------|-------------|
| `peer_health.<id>.latest_tick` | `u64` | Most recent tick number received from this peer. |
| `peer_health.<id>.staleness_ticks` | `u64` | `current_tick - latest_tick`. How far behind this peer's data is. |
| `peer_health.<id>.packets_received` | `u32` | UDP packets received from this peer in the current 5-second window. Healthy = ~100 (20Hz * 5s). |
| `peer_health.<id>.chunk_evictions` | `u32` | Incomplete chunk reassemblies evicted for this peer. Non-zero = partial packet loss. |

## Example Response

```json
{
  "server_id": 1,
  "tick": 5000,
  "entity_count": 500,
  "shadow_count": 300,
  "peer_count": 2,
  "connected_clients": 1,
  "tick_budget_pct": 25.0,
  "replication_mode": "tiered",
  "replication_full_count": 500,
  "replication_sent_count": 150,
  "replication_hot_count": 30,
  "replication_cold_count": 120,
  "udp_repl_drops": 0,
  "udp_ctrl_drops": 0,
  "udp_bytes_out": 12500,
  "peer_health": {
    "2": {
      "latest_tick": 4998,
      "staleness_ticks": 2,
      "packets_received": 97,
      "chunk_evictions": 0
    },
    "3": {
      "latest_tick": 4999,
      "staleness_ticks": 1,
      "packets_received": 100,
      "chunk_evictions": 0
    }
  },
  "phases": {
    "total_ms": { "p50": 12.5, "p95": 18.0, "p99": 22.0 }
  }
}
```

## Interpreting the Data

### Healthy Mesh
- `udp_repl_drops`: 0
- `udp_ctrl_drops`: 0
- `staleness_ticks`: 1-3 (normal replication delay)
- `packets_received`: 90-100 per 5s window
- `chunk_evictions`: 0

### WiFi Saturation Beginning (~8k entities)
- `udp_repl_drops`: sporadic non-zero (1-10 per window)
- `staleness_ticks`: rising to 5-15
- `packets_received`: dropping to 70-85
- `chunk_evictions`: occasional (0-2)

### WiFi Saturated (~10k entities)
- `udp_repl_drops`: consistently non-zero (10-50+)
- `udp_ctrl_drops`: may become non-zero (critical — inputs/transfers affected)
- `staleness_ticks`: 20-100+
- `packets_received`: below 50 (< 10Hz effective rate)
- `chunk_evictions`: frequent (5+)

### Peer Connection Failing
- `staleness_ticks`: 100+ (5+ seconds)
- `packets_received`: near 0
- Peer will be removed from `peer_health` once coordinator detects heartbeat timeout

## Updating Dashboards

### curl / jq

```bash
# Quick saturation check for all 3 boards
for port in 8101 8102 8103; do
  echo "=== Server on port $port ==="
  curl -s http://192.168.0.15:$port/metrics 2>/dev/null || \
  curl -s http://192.168.0.173:$port/metrics 2>/dev/null || \
  curl -s http://192.168.0.86:$port/metrics 2>/dev/null | \
  jq '{
    server_id,
    entity_count,
    udp_repl_drops,
    udp_ctrl_drops,
    udp_bytes_out,
    peer_health
  }'
done

# Per-board check
curl -s http://192.168.0.15:8101/metrics | jq '{
  server_id,
  tick,
  entities: .entity_count,
  shadows: .shadow_count,
  budget_pct: .tick_budget_pct,
  repl_drops: .udp_repl_drops,
  ctrl_drops: .udp_ctrl_drops,
  bytes_out: .udp_bytes_out,
  peers: [.peer_health | to_entries[] | {
    id: .key,
    stale: .value.staleness_ticks,
    rx_pkts: .value.packets_received,
    evictions: .value.chunk_evictions
  }]
}'
```

### JavaScript / TypeScript (load test tools, board dashboards)

```typescript
interface PeerHealth {
  latest_tick: number;
  staleness_ticks: number;
  packets_received: number;
  chunk_evictions: number;
}

interface MetricsResponse {
  // ... existing fields ...
  udp_repl_drops: number;
  udp_ctrl_drops: number;
  udp_bytes_out: number;
  peer_health: Record<string, PeerHealth>;
}

// Fetch and check saturation
const res = await fetch(`http://${host}:${port}/metrics`);
const metrics: MetricsResponse = await res.json();

// Saturation warning thresholds
const saturated = metrics.udp_repl_drops > 0;
const critical = metrics.udp_ctrl_drops > 0;

for (const [peerId, health] of Object.entries(metrics.peer_health)) {
  if (health.staleness_ticks > 20) {
    console.warn(`Peer ${peerId} is ${health.staleness_ticks} ticks behind`);
  }
  if (health.packets_received < 50) {
    console.warn(`Peer ${peerId} receive rate low: ${health.packets_received}/5s`);
  }
  if (health.chunk_evictions > 0) {
    console.warn(`Peer ${peerId} losing chunks: ${health.chunk_evictions} evictions`);
  }
}
```

### Board Dashboard (port 3000)

The board dashboards at `http://<board-ip>:3000` read from the local metrics endpoint. To add network health:

1. Fetch `/metrics` from `localhost:810N` (already done for existing gauges)
2. Add a "Network Health" section with:
   - **UDP Drops** gauge: `udp_repl_drops` (green=0, yellow=1-10, red=10+)
   - **Ctrl Drops** gauge: `udp_ctrl_drops` (green=0, red=any)
   - **Bytes Out** text: `udp_bytes_out` formatted as KB/tick
   - Per-peer rows showing staleness, receive rate, and evictions

### Coordinator Dashboard

The coordinator dashboard at `https://192.168.0.208:7871/dashboard` does not read sim-server metrics directly. To add network health, it would need to poll each sim-server's `/metrics` endpoint. The coordinator already knows each server's IP and metrics port from the registration data.
