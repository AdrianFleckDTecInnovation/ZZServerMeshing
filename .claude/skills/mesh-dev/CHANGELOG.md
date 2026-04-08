# Server Meshing Development Changelog

History of completed features, fixes, and known issues. Read at the start of `/mesh-dev`.

## 2026-04-08 — Test Suite & Performance Infrastructure

### Added
- Per-tick metrics instrumentation on sim-servers (`metrics.rs`, `metrics_server.rs`) — ring buffer with p50/p95/p99 for each game loop phase, exposed via HTTP on port 8100+N
- 6 automated test scenarios: spawn-burst, steady-scale, crash-recovery, rebalance-storm, spatial-hotspot, input-latency
- Smoothness validation scenario that checks for position jumps and entity flicker
- Playwright browser test (`tools/browser-test.ts`) — runs real Chromium, reads `window.__diag` for position jumps, flicker, frame drops
- Client-side diagnostics (`window.__diag`) tracking frames received/applied/dropped, position jumps, flicker events
- Docker test cluster with Pi Zero W2 constraints (0.25 CPU / 512MB per container)

### Added — Rust Gateway
- Replaced Node.js gateway with Rust — 35x less CPU, 15x less memory
- Binary WebSocket protocol: 43 bytes/entity vs ~200 JSON (frame type 0x01=full, 0x02=delta)
- Entity deduplication in gateway — prevents duplicate entities during authority transfers
- Event-driven snapshot ingestion via channels — no lock contention between server snapshot receivers and broadcast loop

### Added — AKS Deployment
- Deploy scripts (`deploy/01-infra.sh` through `deploy/teardown.sh`)
- K8s manifests with pod anti-affinity, graceful drain on SIGTERM
- `--advertise-host` for Docker/k8s peer discovery with DNS resolution fallback
- Coordinator uses connection source IP when `host` field is absent

### Fixed
- Client interpolation warping — `previousPosition` now set from rendered position, not server position
- 3-frame warmup period on connection to prevent initial teleport
- Duplicate tick broadcasts — gateway skips broadcasting when no new server data arrived
- Adaptive interpolation interval — measures actual network rate instead of assuming 50ms
- Recovery dedup — survivors split orphaned entities by UUID hash (no duplicate adoption)
- Rebalance oscillation — threshold set to 20% of average AND 10-entity minimum
- Rebalance detection false positives — uses merged entity view, not per-server iteration
- Debug overlay FPS — now counts from render loop (`requestAnimationFrame`), not debug interval
- Debug overlay Net Hz — counts raw frame arrivals, not filtered applied snapshots
- Server entity counts — rebuilt from deduplicated entity map, not raw snapshot

### Known Issues
- Delta compression disabled (`FULL_SNAPSHOT_INTERVAL=1`) — caused interpolation bugs, needs per-entity timers
- Client receives ~10Hz over internet despite 20Hz server broadcast — TCP/WebSocket latency
- Rolling restarts cause temporary rebalance storms (~60s to settle)
- App Service quota unavailable in westus2 — client served locally via `deploy/client-local.sh`
