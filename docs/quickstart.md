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

# Install gateway dependencies
cd gateway && npm install && cd ..

# Install tools dependencies (optional, for stress testing)
cd tools && npm install && cd ..
```

## Run the Cluster

```bash
# Start coordinator + 3 sim-servers + gateway + client
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

## Stress Testing

```bash
cd tools

# Spawn 200 entities at 10/sec
npx tsx stress-test.ts ws://localhost:4322 200 10

# Spawn 500 entities at 50/sec (heavier load)
npx tsx stress-test.ts ws://localhost:4322 500 50
```

## Manual Launch (for debugging)

```bash
# Terminal 1: Coordinator
cargo run -p coordinator -- --port 7870

# Terminal 2: Sim-server 1
cargo run -p sim-server -- --server-id 1 --port 7878 --udp-port 9101 --coordinator 127.0.0.1:7870

# Terminal 3: Sim-server 2
cargo run -p sim-server -- --server-id 2 --port 7879 --udp-port 9102 --coordinator 127.0.0.1:7870

# Terminal 4: Gateway
cd gateway && COORDINATOR_URL=ws://127.0.0.1:7870 GATEWAY_PORT=4322 npx tsx src/index.ts

# Terminal 5: Client
cd client && npm run dev
```

## Log Files

When using `./start.sh`, logs are written to the project root:

| File | Service |
|------|---------|
| `.coordinator.log` | Coordinator |
| `.sim-server-N.log` | Sim-server N |
| `.gateway.log` | Gateway |
| `.client.log` | Vite dev server |

## Key Constants

| Constant | Value | Meaning |
|----------|-------|---------|
| Tick rate | 20 Hz | Server simulation rate |
| World bounds | ±500 | X and Z axis limits |
| Grid cell size | 50 | Spatial grid cell size |
| Rebalance interval | 5s | How often load is checked |
| Heartbeat timeout | 5s | Dead server detection |
