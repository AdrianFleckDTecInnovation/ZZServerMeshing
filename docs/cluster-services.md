# Cluster Services Reference

How to start, stop, and manage every service on the Pi/UNO Q hardware mesh.

## Hardware Layout

| Device | Hostname | IP | Role |
|--------|----------|----|------|
| Pi Zero W2 | zzttpi | 192.168.0.208 | Coordinator + client |
| UNO Q #1 | unoq | 192.168.0.15 | Sim-server 1 + dashboard |
| UNO Q #2 | unoq2 | 192.168.0.173 | Sim-server 2 + dashboard |
| UNO Q #3 | unoq3 | 192.168.0.86 | Sim-server 3 + dashboard |

All SSH connections use hostname aliases (`ssh unoq`, `ssh zzttpi`, etc.) configured in `~/.ssh/config`.

## Services

### Coordinator (zzttpi)

**Type:** systemd user service (`mesh-coordinator.service`)
**Binary:** `/home/dz/mesh/coordinator`
**Ports:** 7870 (WebSocket), 7871 (HTTPS — serves client app + dashboard)
**Auto-start on boot:** Yes (enabled)

```sh
# Start / stop / restart
ssh zzttpi 'systemctl --user start mesh-coordinator'
ssh zzttpi 'systemctl --user stop mesh-coordinator'
ssh zzttpi 'systemctl --user restart mesh-coordinator'

# Status and logs
ssh zzttpi 'systemctl --user status mesh-coordinator'
ssh zzttpi 'journalctl --user -u mesh-coordinator -f'

# Enable/disable auto-start on boot
ssh zzttpi 'systemctl --user enable mesh-coordinator'
ssh zzttpi 'systemctl --user disable mesh-coordinator'
```

### Sim-Servers (unoq, unoq2, unoq3)

**Type:** Manual process (started via `nohup` by `start-unoq.sh`)
**Binary:** `/home/arduino/mesh/sim-server`
**Logs:** `~/mesh/sim{1,2,3}.log` on each board

| Board | Server ID | WS Port | UDP Port | Metrics Port |
|-------|-----------|---------|----------|--------------|
| unoq  | 1 | 7878 | 9101 | 8101 |
| unoq2 | 2 | 7879 | 9102 | 8102 |
| unoq3 | 3 | 7880 | 9103 | 8103 |

```sh
# Start (example for unoq / server 1)
ssh unoq 'nohup ~/mesh/sim-server \
  --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 \
  --coordinator 192.168.0.208:7870 --advertise-host 192.168.0.15 \
  --initial-npcs 15 \
  --tls-cert ~/mesh/certs/mesh-coordinator.crt \
  --tls-key ~/mesh/certs/mesh-coordinator.key \
  > ~/mesh/sim1.log 2>&1 &'

# Stop (kills all sim-server processes on the board)
ssh unoq 'pkill -f sim-server'

# Check if running
ssh unoq 'pgrep -a sim-server'

# Tail logs
ssh unoq 'tail -f ~/mesh/sim1.log'

# Metrics
curl http://192.168.0.15:8101/metrics | jq .
```

Sim-servers are **not** systemd services — they don't auto-start on boot.

### Board Dashboards (unoq, unoq2, unoq3)

**Type:** systemd user service (`dashboard.service`)
**Binary:** `/home/arduino/dashboard/unoq-dashboard`
**Port:** 3000 on each board
**Auto-start on boot:** Only unoq3 (unoq and unoq2 are not enabled)

The dashboard provides per-board system health monitoring (CPU, memory, temperature) and a sim-server start/stop toggle. It receives sim-server args via a systemd drop-in override at `~/.config/systemd/user/dashboard.service.d/mesh.conf`.

```sh
# Start / stop / restart (same on all 3 boards)
ssh unoq 'systemctl --user start dashboard'
ssh unoq 'systemctl --user stop dashboard'
ssh unoq 'systemctl --user restart dashboard'

# Status
ssh unoq 'systemctl --user status dashboard'

# Enable auto-start on boot (currently only enabled on unoq3)
ssh unoq 'systemctl --user enable dashboard'
ssh unoq2 'systemctl --user enable dashboard'

# Access
# http://192.168.0.15:3000   (unoq)
# http://192.168.0.173:3000  (unoq2)
# http://192.168.0.86:3000   (unoq3)
```

## Quick Reference

### Start everything

The easiest way is `./start-unoq.sh` — it cross-compiles, deploys binaries, and starts all services. For manual startup, start in this order:

```sh
# 1. Coordinator (must be up first — sim-servers register with it)
ssh zzttpi 'systemctl --user start mesh-coordinator'

# 2. Dashboards
ssh unoq 'systemctl --user start dashboard'
ssh unoq2 'systemctl --user start dashboard'
ssh unoq3 'systemctl --user start dashboard'

# 3. Sim-servers (start after coordinator is accepting connections)
# Use the nohup commands above, or start via the dashboard web UI toggle
```

### Stop everything

```sh
./stop-unoq.sh
```

Or manually:

```sh
# 1. Sim-servers
ssh unoq 'pkill -f sim-server'
ssh unoq2 'pkill -f sim-server'
ssh unoq3 'pkill -f sim-server'

# 2. Coordinator
ssh zzttpi 'systemctl --user stop mesh-coordinator'

# 3. Dashboards (optional — they're lightweight and can stay running)
ssh unoq 'systemctl --user stop dashboard'
ssh unoq2 'systemctl --user stop dashboard'
ssh unoq3 'systemctl --user stop dashboard'
```

### Check cluster health

```sh
# Quick: is anything running?
ssh zzttpi 'pgrep -a coordinator'
ssh unoq 'pgrep -a sim-server'
ssh unoq2 'pgrep -a sim-server'
ssh unoq3 'pgrep -a sim-server'

# Coordinator dashboard
# https://192.168.0.208:7871/dashboard

# Server list with entity counts
curl -k https://192.168.0.208:7871/servers | jq .

# Per-server metrics
curl http://192.168.0.15:8101/metrics | jq .
curl http://192.168.0.173:8102/metrics | jq .
curl http://192.168.0.86:8103/metrics | jq .
```

## File Locations on Boards

| Path | What | Device |
|------|------|--------|
| `~/mesh/coordinator` | Coordinator binary | zzttpi |
| `~/mesh/client-dist/` | Built client app (served by coordinator) | zzttpi |
| `~/mesh/certs/` | TLS cert + key | all |
| `~/mesh/sim-server` | Sim-server binary | unoq, unoq2, unoq3 |
| `~/mesh/sim{1,2,3}.log` | Sim-server logs | unoq, unoq2, unoq3 |
| `~/dashboard/unoq-dashboard` | Dashboard binary | unoq, unoq2, unoq3 |
| `~/dashboard/static/` | Dashboard web assets | unoq, unoq2, unoq3 |
| `~/.config/systemd/user/mesh-coordinator.service` | Coordinator service file | zzttpi |
| `~/.config/systemd/user/dashboard.service` | Dashboard service file | unoq, unoq2, unoq3 |
| `~/.config/systemd/user/dashboard.service.d/mesh.conf` | Per-board sim-server args | unoq, unoq2, unoq3 |
