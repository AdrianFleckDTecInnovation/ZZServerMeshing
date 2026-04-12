# UNO Q Mesh Deployment

Deploy the server meshing prototype across two Arduino UNO Q boards, with a Mac as gateway and client.

## Hardware

| Role | Host | Hostname | IP | Specs |
|---|---|---|---|---|
| Sim-server 1 + Coordinator | UNO Q #1 | unoq.local | 192.168.0.15 | 4-core aarch64, 3.6 GB RAM, Debian Trixie 13 |
| Sim-server 2 | UNO Q #2 | unoq2.local | 192.168.0.173 | 4-core aarch64, 3.6 GB RAM, Debian Trixie 13 |
| Gateway + Client | Mac | localhost | — | macOS, runs browser + gateway process |

SSH access: `ssh unoq` / `ssh unoq2` (configured in `~/.ssh/config`, user `arduino`, key `~/.ssh/pi_key`).

## Architecture

```
Browser (Mac :4321)
    │
    WS
    ▼
Gateway (Mac :4322)
    │
    WS ──────────────────── WS
    ▼                        ▼
┌──────────────────┐   ┌──────────────────┐
│  unoq.local      │   │  unoq2.local     │
│                  │   │                  │
│  Coordinator     │   │                  │
│    :7870         │   │                  │
│                  │   │                  │
│  Sim-server 1    │   │  Sim-server 2    │
│    :7878 (ws)    │   │    :7879 (ws)    │
│    :9101 (udp)   │   │    :9102 (udp)   │
│    :8101 (metrics)│   │    :8102 (metrics)|
└──────────────────┘   └──────────────────┘
         │                      │
         └──── UDP (bincode) ───┘
           entity replication
```

The two UNO Q boards are the mesh — they replicate entities over UDP and handle simulation. The Mac is only the gateway (aggregation/routing) and client (browser). This keeps the heavy per-tick work on the boards and the Mac just presents it.

Later, with the true-mesh refactor (see `true-mesh-architecture.md`), the gateway moves onto the boards too, and clients connect directly.

## Port Mapping

| Service | Host | Port |
|---|---|---|
| Coordinator | unoq | 7870 |
| Sim-server 1 WS | unoq | 7878 |
| Sim-server 1 UDP | unoq | 9101 |
| Sim-server 1 Metrics | unoq | 8101 |
| Sim-server 2 WS | unoq2 | 7879 |
| Sim-server 2 UDP | unoq2 | 9102 |
| Sim-server 2 Metrics | unoq2 | 8102 |
| Gateway | Mac | 4322 |
| Client (Vite) | Mac | 4321 |

## Cross-Compilation

The boards are `aarch64-unknown-linux-gnu`. No Rust toolchain is installed on them (3 GB free disk — tight for rustup). Cross-compile from the Mac instead.

### Option A: `cross` (Docker-based, recommended)

[`cross`](https://github.com/cross-rs/cross) uses Docker containers with the correct linker and sysroot. Docker is already installed.

```bash
# Install cross
cargo install cross --git https://github.com/cross-rs/cross

# Build all Rust crates for aarch64
cross build --release --target aarch64-unknown-linux-gnu
```

Binaries land in `target/aarch64-unknown-linux-gnu/release/`.

### Option B: Native linker (brew)

```bash
# Install the GNU toolchain for aarch64
brew install aarch64-unknown-linux-gnu

# Tell Cargo to use it
mkdir -p .cargo
cat >> .cargo/config.toml << 'EOF'
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-unknown-linux-gnu-gcc"
EOF

# Build
cargo build --release --target aarch64-unknown-linux-gnu
```

### Option C: Build on-board (slow, not recommended)

Install rustup on each board and compile natively. Works but takes a long time on 4 ARM cores with 3.6 GB RAM, and eats into the limited 3 GB free disk.

## Deployment

### 1. Build

```bash
cross build --release --target aarch64-unknown-linux-gnu
```

### 2. Deploy binaries

```bash
# Create deployment directory on each board
ssh unoq 'mkdir -p ~/mesh'
ssh unoq2 'mkdir -p ~/mesh'

# Copy binaries
scp target/aarch64-unknown-linux-gnu/release/coordinator unoq:~/mesh/
scp target/aarch64-unknown-linux-gnu/release/sim-server unoq:~/mesh/
scp target/aarch64-unknown-linux-gnu/release/sim-server unoq2:~/mesh/
```

### 3. Start the mesh

**On unoq (coordinator + sim-server 1):**

```bash
# Terminal 1: coordinator
ssh unoq '~/mesh/coordinator --port 7870'

# Terminal 2: sim-server 1
ssh unoq '~/mesh/sim-server --server-id 1 --port 7878 --udp-port 9101 --coordinator 127.0.0.1:7870'
```

**On unoq2 (sim-server 2):**

```bash
ssh unoq2 '~/mesh/sim-server --server-id 2 --port 7879 --udp-port 9102 --coordinator 192.168.0.15:7870'
```

Note: unoq2 points its coordinator address at unoq's IP (`192.168.0.15`), not localhost.

**On Mac (gateway + client):**

```bash
# Gateway — point at unoq's coordinator
COORDINATOR_URL=ws://192.168.0.15:7870 GATEWAY_PORT=4322 npx tsx gateway/src/index.ts

# Client — update VITE_GATEWAY_URL if needed (default ws://localhost:4322 is fine)
cd client && npm run dev
```

### 4. Verify

```bash
# Check coordinator sees both servers
curl -s http://192.168.0.15:7870/status 2>/dev/null || echo "check coordinator logs"

# Check sim-server metrics
curl -s http://192.168.0.15:8101/metrics | jq .
curl -s http://192.168.0.173:8102/metrics | jq .

# Open browser
open http://localhost:4321
```

## Start/Stop Scripts

A `start-mesh.sh` script to automate the above (future work):

```bash
#!/bin/bash
# start-mesh.sh — launch mesh across UNO Q boards + local gateway

UNOQ=unoq
UNOQ2=unoq2
UNOQ_IP=192.168.0.15

# Start coordinator on unoq
ssh $UNOQ "nohup ~/mesh/coordinator --port 7870 > ~/mesh/coordinator.log 2>&1 &"
sleep 1

# Start sim-server 1 on unoq
ssh $UNOQ "nohup ~/mesh/sim-server --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 --coordinator 127.0.0.1:7870 > ~/mesh/sim1.log 2>&1 &"

# Start sim-server 2 on unoq2
ssh $UNOQ2 "nohup ~/mesh/sim-server --server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 --coordinator ${UNOQ_IP}:7870 > ~/mesh/sim2.log 2>&1 &"

sleep 2

# Start gateway locally
COORDINATOR_URL=ws://${UNOQ_IP}:7870 GATEWAY_PORT=4322 npx tsx gateway/src/index.ts &

# Start client
cd client && npm run dev &

echo "Mesh running. Open http://localhost:4321"
```

```bash
#!/bin/bash
# stop-mesh.sh — kill everything

ssh unoq 'pkill -f coordinator; pkill -f sim-server' 2>/dev/null
ssh unoq2 'pkill -f sim-server' 2>/dev/null
pkill -f "tsx gateway" 2>/dev/null
pkill -f "vite" 2>/dev/null

echo "Mesh stopped."
```

## Gateway Config Changes

The gateway currently discovers sim-servers via the coordinator's `PeerList`. The sim-server WebSocket addresses in `PeerList` are currently `127.0.0.1:port` (since servers register with their local bind address). For cross-host deployment, sim-servers need to register with their LAN-routable address.

**Options:**
1. Add a `--public-addr` flag to sim-server so it registers with the coordinator using its LAN IP
2. Have the coordinator infer the source IP from the WebSocket connection
3. Hardcode the mapping in the gateway

Option 1 is cleanest:

```bash
# sim-server 1 on unoq
~/mesh/sim-server --server-id 1 --port 7878 --udp-port 9101 \
  --public-addr 192.168.0.15 --coordinator 127.0.0.1:7870

# sim-server 2 on unoq2
~/mesh/sim-server --server-id 2 --port 7879 --udp-port 9102 \
  --public-addr 192.168.0.173 --coordinator 192.168.0.15:7870
```

This is a small code change: accept `--public-addr` in sim-server CLI args, use it in the `Register` message to the coordinator instead of `127.0.0.1`.

## Scaling Up

To add more sim-servers on the same boards (each board has 4 cores, 3.6 GB RAM):

```bash
# Second sim-server on unoq (server 3)
ssh unoq '~/mesh/sim-server --server-id 3 --port 7880 --udp-port 9103 --metrics-port 8103 --coordinator 127.0.0.1:7870'

# Second sim-server on unoq2 (server 4)
ssh unoq2 '~/mesh/sim-server --server-id 4 --port 7881 --udp-port 9104 --metrics-port 8104 --coordinator 192.168.0.15:7870'
```

With 4 cores and ~3 GB available RAM per board, 2 sim-servers per board is reasonable. The coordinator is lightweight and can share a board with a sim-server.

## Known Issues / TODO

- [ ] `--public-addr` flag needs to be added to sim-server CLI
- [ ] Coordinator `PeerList` uses the registered address — verify gateway can reach it
- [ ] UDP replication between boards may need firewall rules (`ufw allow 9101:9110/udp`)
- [ ] Binary size — check `release` binary fits comfortably in 3 GB free disk
- [ ] Systemd service files for auto-start on boot (optional)
- [ ] Log rotation on boards (small disk)
