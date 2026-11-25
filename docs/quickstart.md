# Quick Start Guide

Start the persistent Bevy game with `cargo run --locked -p infinite-stars`. Its worker commands, game extension example and release measurements are in the [Infinite Stars guide](infinite-stars.md). The commands below start the original browser demo.


## Prerequisites

- Rust stable with the platform C toolchain. On Windows install the MSVC toolchain, Visual Studio C++ Build Tools and the Windows SDK. On Debian/Ubuntu the native compiler and linker are provided by `build-essential`.
- Node.js 22.16 or newer with npm. Run commands from the repository root. On PowerShell, `npm.cmd` also works when the local execution policy restricts `npm.ps1`.

## Setup

```bash
npm run setup
```

## Run the Cluster

The native launcher binds coordinator, simulation, metrics and Vite to IPv4 loopback. Standalone server processes keep their deployment bindings. The [cluster benchmark guide](cluster-performance.md) includes commands for measuring the browser demo and persistent game beside a running development mesh.

```bash
# Start coordinator + 3 sim-servers + client
npm start

# Or with a custom number of servers
npm start -- --servers 5 --npcs 100

# Stop from another terminal, or press Ctrl+C in the foreground terminal
npm stop
```

Open http://127.0.0.1:4321 in a browser with WebGPU or WebGL2 support. Each launcher owns its child processes and prints the service log directory under `target/mesh-run-*/`. Local port defaults and environment variable names are in `scripts/local-defaults.env`.

## Verify and benchmark

`npm test` runs the Rust workspace, three TypeScript test files and the client production build. Use free mesh ports for the smoke test and benchmark. The smoke test verifies the Vite page and assignment proxy, player input acknowledgement and peer shadow replication. The benchmark starts a fresh release mesh and writes JSON to stdout.

```sh
npm test
npm run smoke
npm run bench -- --npcs 3000 --seconds 20
```

For a specific scenario on an already running local mesh, pass the first sim-server WebSocket URL, the first metrics port, the server count and the scenario list.

```sh
npm --prefix tools run scenario -- ws://127.0.0.1:7878 8101 3 peer-replication
```

The release workloads and interpretation of the percentiles are in [the benchmark report](benchmark.md).

To exercise the actual renderer against `npm start`, install the matching Playwright Chromium with `npm --prefix tools exec -- playwright install chromium` and run `npm --prefix tools run browser` in a second terminal. To use an installed Microsoft Edge, set `PLAYWRIGHT_CHANNEL` to `msedge`.

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
cargo run -p sim-server -- --server-id 1 --port 7878 --udp-port 9101 --metrics-port 8101 --coordinator 127.0.0.1:7870 --cold-start

# Terminal 3: Sim-server 2
cargo run -p sim-server -- --server-id 2 --port 7879 --udp-port 9102 --metrics-port 8102 --coordinator 127.0.0.1:7870 --cold-start

# Terminal 4: Client
npm --prefix client run dev -- --port 4321
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
| World bounds | ±5000 | X and Z axis limits |
| Grid cell size | 50 | Spatial grid cell size |
| Rebalance interval | 5s | How often load is checked |
| Heartbeat timeout | 15s | Dead server detection |
