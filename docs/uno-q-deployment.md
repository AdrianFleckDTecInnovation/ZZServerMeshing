# UNO Q Mesh Deployment

Deploy the server meshing prototype across 3 Arduino UNO Q boards and a Pi Zero W2 coordinator. The Mac is not required — the entire mesh runs on embedded hardware.

For board specs, SSH setup, systemd service files, and first-time setup, see [Hardware Stack](hardware-stack.md).

## Hardware

| Role | Device | Hostname | SSH | IP | Arch | User |
|------|--------|----------|-----|-----|------|------|
| Coordinator + Client | Pi Zero W2 | zzttpi.local | `ssh zzttpi` | 192.168.0.208 | armv7l | dz |
| Sim-server 1 | UNO Q #1 | unoq.local | `ssh unoq` | 192.168.0.15 | aarch64 | arduino |
| Sim-server 2 | UNO Q #2 | unoq2.local | `ssh unoq2` | 192.168.0.173 | aarch64 | arduino |
| Sim-server 3 | UNO Q #3 | unoq3.local | `ssh unoq3` | 192.168.0.86 | aarch64 | arduino |

All boards: 4-core ARM, ~3.6 GB RAM, Debian Trixie 13. Pi Zero W2: 425 MB RAM, Raspbian Trixie 13.

## Architecture

```
Browser (any device on LAN)
    │
    HTTPS (:7871)
    ▼
┌─────────────────────────┐
│  zzttpi (Pi Zero W2)    │
│                         │
│  Coordinator  :7870 ws  │
│  Client+API   :7871 https│
│  (serves built Vite app)│
└─────────────────────────┘
    │ WS (coordinator protocol)
    ▼
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│  unoq        │  │  unoq2       │  │  unoq3       │
│              │  │              │  │              │
│  S1          │  │  S2          │  │  S3          │
│  :7878 wss   │  │  :7879 wss   │  │  :7880 wss   │
│  :9101 udp   │  │  :9102 udp   │  │  :9103 udp   │
│  :8101 metrics│  │  :8102 metrics│  │  :8103 metrics│
│  :3000 dash  │  │  :3000 dash  │  │  :3000 dash  │
└──────────────┘  └──────────────┘  └──────────────┘
    │                  │                  │
    └──── UDP (bincode) entity replication ┘
```

Clients connect directly to sim-servers via WSS. The coordinator assigns clients to the least-loaded server. Each sim-server broadcasts the full world (authoritative + shadow entities) to its clients.

## Quick Start

```sh
./start-unoq.sh       # cross-compile, deploy, start everything
./stop-unoq.sh        # stop everything
```

### URLs

| Service | URL |
|---------|-----|
| Client (Three.js) | https://192.168.0.208:7871 |
| Coordinator Dashboard | https://192.168.0.208:7871/dashboard |
| Server Assignment | https://192.168.0.208:7871/assign |
| Board 1 Dashboard | http://192.168.0.15:3000 |
| Board 2 Dashboard | http://192.168.0.173:3000 |
| Board 3 Dashboard | http://192.168.0.86:3000 |

### Metrics

```sh
curl 192.168.0.15:8101/metrics     # S1
curl 192.168.0.173:8102/metrics    # S2
curl 192.168.0.86:8103/metrics     # S3
```

## Cross-Compilation

Two targets for two architectures:

| Target | Arch | Devices | Binaries |
|--------|------|---------|----------|
| `aarch64-unknown-linux-gnu` | 64-bit ARM | unoq, unoq2, unoq3 | sim-server, board-dash |
| `armv7-unknown-linux-gnueabihf` | 32-bit ARM | zzttpi | coordinator |

Built via [`cross`](https://github.com/cross-rs/cross) (Docker-based):

```sh
# UNO Q boards
cross build --release --target aarch64-unknown-linux-gnu -p sim-server -p board-dash

# Pi Zero W2
cross build --release --target armv7-unknown-linux-gnueabihf -p coordinator
```

Binaries land in `target/<target>/release/`. Deployed via SCP to `~/mesh/` on each device.

## TLS

All WebSocket and HTTP connections use TLS (self-signed certificate). This is required for WebGPU — browsers only expose `navigator.gpu` in secure contexts (HTTPS/WSS or localhost).

### Certificate

Generated automatically on first `./start-unoq.sh` run. Stored in `certs/` (gitignored). Single cert covers all devices via Subject Alternative Names:

```
IP:192.168.0.208, IP:192.168.0.15, IP:192.168.0.173, IP:192.168.0.86
DNS:zzttpi.local, DNS:zzttpi, DNS:unoq.local, DNS:unoq2.local, DNS:unoq3.local
```

### Trust the cert (macOS)

Eliminates browser warnings for all board IPs:

```sh
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain certs/mesh-coordinator.crt
```

### Regenerate

Delete and re-run (e.g., when adding a new board IP):

```sh
rm certs/mesh-coordinator.*
./start-unoq.sh    # generates fresh cert with updated SANs
# Re-trust the new cert:
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain certs/mesh-coordinator.crt
```

**Important:** Regenerating invalidates the trusted cert in the keychain. You must remove the old one and add the new one. To remove:

```sh
sudo security delete-certificate -c "mesh-coordinator" /Library/Keychains/System.keychain
```

## Coordinator Service

The coordinator runs as a systemd user service on zzttpi. It starts automatically on boot and restarts on crash.

```sh
# Status
ssh zzttpi 'systemctl --user status mesh-coordinator'

# Logs
ssh zzttpi 'journalctl --user -u mesh-coordinator -f'

# Manual restart
ssh zzttpi 'systemctl --user restart mesh-coordinator'
```

The service file is at `~/.config/systemd/user/mesh-coordinator.service` on zzttpi. Linger is enabled for user `dz` so the service starts without a login session.

### Coordinator resilience

The coordinator is stateless. If it restarts:
- Sim-servers reconnect automatically (2-second retry loop)
- Existing client WebSocket connections are unaffected (they're direct to sim-servers)
- Only new client assignments (`/assign`) are unavailable during downtime

## Board Dashboards

Each UNO Q board runs a dashboard service on port 3000 (systemd user service `dashboard`). It shows system metrics and has a start/stop toggle for the local sim-server.

The dashboard fetches sim-server metrics from the local metrics port (8101-8105 range, auto-detected).

Dashboard binary: `board-dash` crate, deployed as `~/dashboard/unoq-dashboard`.

## Deployment Directory Layout

### UNO Q boards (`~/mesh/`)
```
~/mesh/
├── sim-server                    # sim-server binary
├── sim1.log (or sim2.log, sim3.log)
└── certs/
    ├── mesh-coordinator.crt      # shared TLS cert
    └── mesh-coordinator.key
~/dashboard/
├── unoq-dashboard                # board-dash binary
└── static/                       # dashboard web assets
```

### zzttpi (`~/mesh/`)
```
~/mesh/
├── coordinator                   # coordinator binary
├── coordinator.log
├── client-dist/                  # built Vite client app
│   ├── index.html
│   └── assets/
│       └── index-*.js
└── certs/
    ├── mesh-coordinator.crt
    └── mesh-coordinator.key
```

## Running Local Client Against Board Mesh

To use the Mac's Vite dev server (with WebGPU via localhost) but connect to the board mesh:

```sh
VITE_COORDINATOR_URL="https://192.168.0.208:7871" npx vite --port 4321
```

The Vite proxy in `client/vite.config.ts` forwards `/assign`, `/servers`, `/dashboard` to the coordinator.

Without `VITE_COORDINATOR_URL`, the proxy defaults to `http://localhost:7871` (local mesh).

## Running Both Meshes Simultaneously

The local mesh (`./start.sh`) and board mesh (`./start-unoq.sh`) use the same ports but on different hosts. They can run at the same time:

| | Local (Mac) | Board Mesh |
|---|---|---|
| Coordinator | localhost:7870/7871 | 192.168.0.208:7870/7871 |
| S1 | localhost:7878 | 192.168.0.15:7878 |
| S2 | localhost:7879 | 192.168.0.173:7879 |
| S3 | localhost:7880 | 192.168.0.86:7880 |
| Client | localhost:4321 | 192.168.0.208:7871 |

## Troubleshooting

### Sim-server won't start
```sh
ssh unoq 'cat ~/mesh/sim1.log'   # check logs
ssh unoq 'pgrep -a sim-server'   # check if already running
```

### Dashboard shows "Running but metrics unavailable"
The `board-dash` checks ports 8101-8105 for the metrics endpoint. If the sim-server uses a different metrics port, it won't find it.

### Certificate warnings after regeneration
The old cert is still trusted in the keychain. Remove it and add the new one (see TLS section above).

### WebGPU not available
WebGPU requires a secure context. Use either:
- `https://` from the Pi (self-signed cert, trusted in keychain)
- `http://localhost:4321` (localhost is always a secure context)

### `openssl` cert generation fails
Use `P-256` not `prime256r1` for the EC curve — macOS OpenSSL doesn't recognize `prime256r1`.
