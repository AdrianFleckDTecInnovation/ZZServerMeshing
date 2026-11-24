# Local mesh benchmark

Each run starts a fresh release coordinator and three release sim-servers on one host. The launcher divides the NPCs evenly and connects one WebSocket client to each server. Each connection adds one player and acknowledges binary full and delta snapshots. NPCs start at random X/Z coordinates within ±400 units. The clients stay connected and stationary while the NPC simulation and UDP replication run. The [six JSON reports](benchmarks/2026-11-24.json) include the counters and percentiles for each server.

## Reproduce

From the repository root with Rust stable and Node.js 22.16+ installed, run one workload at a time on free mesh ports.

```sh
npm --silent run bench -- --servers 3 --npcs 30 --seconds 20
npm --silent run bench -- --servers 3 --npcs 3000 --seconds 20
npm --silent run bench -- --servers 3 --npcs 9000 --seconds 20
```

The launcher checks peer discovery and entity counts, waits for all three clients to receive frames, warms up for five seconds and then measures for 20 seconds. Its JSON contains the actual elapsed time, tick and snapshot counts, per-client payload throughput and per-server tick work percentiles. Service logs go to a distinct directory under `target/` for each run.

Tick and snapshot rates use counter differences over the measured interval. The server calculates p50, p95 and p99 over its latest 200 ticks, about ten seconds at 20 Hz. The budget column is the largest per-server p95 divided by the 50 ms tick period. Snapshot rate measures network delivery to a client. Browser rendering and network latency beyond loopback require separate measurements.

## Windows x64 · 2026-11-24

Windows build 26200, Intel Core i7-13850HX, Rust 1.96.1 MSVC, Node.js 22.16.0. Each row is one independent run. Ranges span the three servers.

| NPCs + players | Tick rate (Hz) | Snapshots/s per client | Tick p50 (ms) | Slowest p95 (ms) | Slowest p99 (ms) | p95 budget |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 30 + 3 | 19.98–20.03 | 19.98–20.03 | 0.27–0.28 | 0.52 | 0.83 | 1.04% |
| 3,000 + 3 | 19.98–20.03 | 19.98–20.03 | 1.97–2.14 | 3.61 | 5.15 | 7.23% |
| 9,000 + 3 | 19.99–20.04 | 19.99–20.04 | 4.43–4.79 | 6.84 | 10.04 | 13.67% |

At 9,000 NPCs each server reported 3,001 authoritative entities, 6,002 shadows and two peers. The three clients received 401, 400 and 401 snapshots respectively over 20.013 seconds.

## Debian 12.15 x64 · 2026-11-24

Debian kernel `6.1.0-56-cloud-amd64`, Rust 1.96.1 GNU, Node.js 22.16.0. Linux ran in a QEMU guest accelerated by the Windows hypervisor on the same i7-13850HX, with six vCPUs and 6 GiB of assigned memory. The Windows run used the host directly. The different resource allocations matter when comparing the absolute times.

| NPCs + players | Tick rate (Hz) | Snapshots/s per client | Tick p50 (ms) | Slowest p95 (ms) | Slowest p99 (ms) | p95 budget |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 30 + 3 | 20.00 | 20.00 | 0.16–0.27 | 0.55 | 1.12 | 1.09% |
| 3,000 + 3 | 19.99 | 19.99 | 1.72–2.13 | 6.04 | 8.12 | 12.07% |
| 9,000 + 3 | 19.99 | 19.99 | 3.45–4.14 | 7.50 | 13.97 | 15.01% |

At 9,000 NPCs each Linux server reported 3,001 authoritative entities, 6,002 shadows and two peers. All three clients received 400 snapshots in 20.007 seconds.

## Functional checks

Both systems passed 642 Rust tests, 83 TypeScript checks and the Vite production build. The optional MCP oracle also compiled on both. Five live scenarios passed on each platform. They exercised input acknowledgements (`input-seq-echo`), peer shadows (`peer-replication`), agreement between clients (`multi-client`), spawning and removal (`entity-lifecycle`), and rebalancing (`authority-transfer`). In the transfer scenario, the client observed nine distinct entity IDs change authority on each platform.

A Windows debug mesh ran for more than 5,000 ticks after correcting a queue check that could race with incoming peer packets. A four-server run with 11 NPCs verified uneven seed totals and custom ports. In a separate 15-second Windows browser run, headless Edge applied all 289 received snapshots. The final sample reported 66 render FPS with zero dropped frames, duplicate ticks or position jumps.
