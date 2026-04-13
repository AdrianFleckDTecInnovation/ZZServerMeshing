# Quick Start Guide

## Prerequisites

- Rust (stable, 1.75+)
- Node.js (18+)
- npm

## Setup

```bash
# Clone and enter
git clone <repo-url>
cd ZZServerMeshing

# Install client dependencies
cd client && npm install && cd ..

# Install tools dependencies (optional, for stress testing)
cd tools && npm install && cd ..
```

## Run the Cluster

```bash
# Start coordinator + 3 sim-servers + client
./start.sh

# Or with a custom number of servers
./start.sh 5

# Stop everything
./stop.sh
```

Open http://localhost:4321 in your browser.

## Controls

| Input | Action |
|-------|--------|
| **WASD** / Arrow keys | Move your player |
| **Click** | Spawn NPC at cursor |
| **Shift+Click** | Spawn Player entity |
| **Scroll** | Zoom |
| **Drag** | Orbit camera |
| **F** | Toggle camera follow mode |

## What You'll See

- Your player is a **bright green box** — move with WASD
- NPCs are **colored spheres** — color indicates which server owns them:
  - Red = Server 1, Blue = Server 2, Green = Server 3, etc.
- Other players are **gold boxes**
- Debug overlay (top-left) shows:
  - Connection status, tick number, FPS
  - Per-server entity counts with load bars
  - Network metrics (snapshots/sec, bandwidth, rebalance events)

## Endpoints

| Service | URL |
|---------|-----|
| Client | http://localhost:4321 |
| Coordinator Dashboard | http://localhost:7871 |
| Server Assignment | http://localhost:7871/assign |
| S1 Metrics | http://localhost:8101/metrics |
| S2 Metrics | http://localhost:8102/metrics |
| S3 Metrics | http://localhost:8103/metrics |

## Manual Launch (for debugging)

```bash
# Terminal 1: Coordinator
cargo run -p coordinator -- --port 7870 --http-port 7871

# Terminal 2: Sim-server 1
cargo run -p sim-server -- --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 --coordinator 127.0.0.1:7870

# Terminal 3: Sim-server 2
cargo run -p sim-server -- --server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 --coordinator 127.0.0.1:7870

# Terminal 4: Client
cd client && npx vite --port 4321
```

## Log Files

When using `./start.sh`, logs are written to the project root:

| File | Service |
|------|---------|
| `.coordinator.log` | Coordinator |
| `.sim-server-N.log` | Sim-server N |
| `.client.log` | Vite dev server |

## UNO Q Board Deployment

To deploy on the embedded hardware mesh (3 UNO Q boards + Pi Zero W2), see [UNO Q Deployment](uno-q-deployment.md). For board specs, SSH setup, and systemd services, see [Hardware Stack](hardware-stack.md).

```bash
./start-unoq.sh    # cross-compile, deploy, start on boards
./stop-unoq.sh     # stop everything
```

## Key Constants

| Constant | Value | Meaning |
|----------|-------|---------|
| Tick rate | 20 Hz | Server simulation rate |
| World bounds | ±500 | X and Z axis limits |
| Grid cell size | 50 | Spatial grid cell size |
| Rebalance interval | 5s | How often load is checked |
| Heartbeat timeout | 5s | Dead server detection |
