# Configuration Packaging Recommendations

Scan of the full codebase for configuration sprawl, duplication, and hardcoded values. Organized by impact and effort.

---

## 1. Port Numbers Duplicated Everywhere (High Impact)

The port formulas (`7877 + server_id`, `9100 + server_id`, `8100 + server_id`) and base ports (7870, 7871) are repeated independently across 6+ files with no single source of truth.

**Where they appear:**
- `start.sh` — formula-based
- `start-unoq.sh` — hardcoded per-board
- `docker-compose.test.yml` — hardcoded per-service
- `k8s/sim-server.yaml` — hardcoded in init script
- `k8s/coordinator.yaml` — hardcoded
- `test.sh` — hardcoded for health checks
- `client/vite.config.ts` — proxy target default
- `CLAUDE.md` — documentation table

**Recommendation:** Create a `config.sh` at the project root that defines base ports. Source it from all shell scripts. For K8s, use a ConfigMap or Kustomize overlay. For Docker Compose, use `.env` + variable interpolation.

```sh
# config.sh
COORD_WS_PORT=7870
COORD_HTTP_PORT=7871
SIM_WS_BASE=7877    # actual = base + server_id
SIM_UDP_BASE=9100
SIM_METRICS_BASE=8100
CLIENT_PORT=4321
BOARD_DASH_PORT=3000
```

**Effort:** Low. Shell scripts just need `source ./config.sh`. Docker Compose supports `.env` natively.

---

## 2. NPC Count Inconsistent Across Deployments (Medium Impact)

Initial NPC counts differ by deployment mode with no obvious reason:

| Deployment | NPCs per server | Total |
|-----------|----------------|-------|
| Local (`start.sh`) | 10 (30 / 3) | 30 |
| Docker (`docker-compose.test.yml`) | 10 | 30 |
| UNO Q (`start-unoq.sh`) | 15 | 45 |
| K8s (`k8s/sim-server.yaml`) | 10 | 30 |

The UNO Q value of 15 may be intentional (boards can handle it), but it's not documented anywhere.

**Recommendation:** Add `DEFAULT_NPCS` to `config.sh` with per-deployment overrides. Document why the UNO Q value differs if intentional.

**Effort:** Trivial.

---

## 3. Sim-Server Has 30+ Compile-Time Constants That Could Be Runtime (Medium Impact)

These constants in `sim-server` control important operational behavior but require recompilation to change:

| Constant | Value | File | Why it matters |
|----------|-------|------|---------------|
| `SHADOW_STALE_TICKS` | 300 (15s) | systems.rs:24 | Tuning for different network conditions |
| `TRANSFER_TIMEOUT_TICKS` | 10 (500ms) | systems.rs:27 | WiFi vs wired needs different values |
| `TRANSFER_MAX_RETRIES` | 3 | systems.rs:33 | Same |
| `DRAIN_TIMEOUT_TICKS` | 600 (30s) | systems.rs:36 | Operational concern |
| `DELTA_FALLBACK_THRESHOLD` | 0.80 | systems.rs:41 | Performance tuning |
| `INPUT_RATE_LIMIT` | 30/sec | network.rs:18 | Anti-abuse tuning |
| `SPAWN_RATE_LIMIT` | 5/sec | network.rs:20 | Same |
| `FULL_SNAPSHOT_INTERVAL` | 100 ticks | delta.rs:22 | Bandwidth vs reliability tradeoff |
| `PER_CLIENT_CHANNEL_SIZE` | 8 | resources.rs:221 | Backpressure tuning |
| `ACK_STALE_TICKS` | 40 (2s) | resources.rs:227 | Client timeout sensitivity |

`full-repl-interval` and `full-snapshot` are already CLI flags -- good precedent.

**Recommendation:** Don't make all of these CLI args (too many flags). Instead, add an optional `--config <path>` flag that loads a TOML file. Fall back to compiled defaults when no config is provided. This keeps the zero-config developer experience while enabling runtime tuning on boards.

```toml
# mesh.toml (optional)
[timeouts]
shadow_stale_secs = 15
transfer_timeout_ms = 500
transfer_max_retries = 3
drain_timeout_secs = 30

[rate_limits]
inputs_per_sec = 30
spawns_per_sec = 5

[delta]
fallback_threshold = 0.80
full_snapshot_interval_ticks = 100

[channels]
per_client_buffer = 8
ack_stale_ticks = 40
```

**Effort:** Medium. Add `toml` crate, define a config struct with `#[serde(default)]`, load on startup.

---

## 4. Coordinator Has Hardcoded Rebalance/Health Parameters (Medium Impact)

The coordinator's operational behavior is also compile-time only:

| Constant | Value | File |
|----------|-------|------|
| Heartbeat timeout | 15s | registry.rs:126 |
| Rebalance interval | 5s | main.rs (timer) |
| Weighted load formula | players=3x, NPCs=1x | registry.rs |
| In-flight transfer tracking | per-server counter | registry.rs:21 |

**Recommendation:** Same TOML config approach. The coordinator already parses CLI args via clap -- add `--config` alongside existing flags.

```toml
# coordinator.toml (optional)
[health]
heartbeat_timeout_secs = 15

[rebalance]
interval_secs = 5
player_weight = 3
npc_weight = 1
```

**Effort:** Low (coordinator is simpler than sim-server).

---

## 5. Client Constants Are All Hardcoded in TypeScript (Low Impact)

The client has ~40 magic numbers scattered across 7 files. Key ones:

| Constant | Value | File | Category |
|----------|-------|------|----------|
| `PLAYER_SPEED` | 5.0 | prediction.ts | Gameplay |
| `CORRECTION_BLEND` | 0.2 | prediction.ts | Netcode feel |
| `MAX_PENDING` | 128 | prediction.ts | Memory |
| Reconnect delay | 1000ms initial, 10000ms max | network.ts | UX |
| Stale entity timeout | 6000ms | world-view.ts | Ghost cleanup |
| AoI radius (visual) | 500 | renderer.ts | Must match server |
| Camera height | 600 | renderer.ts | View |
| Entity sizes | sphere r=12, box 16^3 | renderer.ts | Visual |
| Server colors | 8-color palette | world-view.ts | Visual |
| Debug poll interval | 2000ms | debug-overlay.ts | Network load |

**Recommendation:** Create a `client/src/config.ts` that exports all tunable values as named constants. Not a runtime config file -- just consolidation so there's one place to look. Group by category (netcode, rendering, gameplay, debug).

```ts
// config.ts
export const NETCODE = {
  RECONNECT_DELAY_MS: 1000,
  RECONNECT_MAX_DELAY_MS: 10000,
  STALE_ENTITY_MS: 6000,
  WARMUP_SNAPSHOTS: 3,
  DIAG_HISTORY_SIZE: 200,
  DIAG_SUMMARY_TICKS: 100,
} as const;

export const GAMEPLAY = {
  PLAYER_SPEED: 5.0,
  MAX_PENDING_INPUTS: 128,
  CORRECTION_BLEND: 0.2,
} as const;

export const RENDERING = {
  CAMERA_HEIGHT: 600,
  CAMERA_FOV: 90,
  NPC_RADIUS: 12,
  PLAYER_SIZE: 16,
  AOI_RADIUS: 500,   // must match shared/spatial.rs AOI_RADIUS
  GROUND_SIZE: 10000, // must match WORLD_EXTENT * 2
} as const;
```

**Effort:** Low. Pure refactor, no behavior change. Mark values that must match server-side with comments.

---

## 6. Shared Constants Duplicated Between Rust and TypeScript (Medium Impact)

Several values must be identical on server and client but are defined independently:

| Value | Rust location | TS location | Risk |
|-------|--------------|-------------|------|
| Tick duration (50ms) | `shared/tick.rs` | `world-view.ts` (implicit in interpolation) | Desync |
| AoI radius (500) | `shared/spatial.rs:49` | `renderer.ts:89` | Visual mismatch |
| World extent (5000) | `shared/spatial.rs:7` | `renderer.ts:73` (as 10000 = 2x) | Ground size wrong |
| Entity binary size (47 bytes) | `encoding.rs` (implicit) | `binary-protocol.ts:25` | Parse failure |
| Protocol version | `shared/lib.rs:9` | Not checked client-side | Silent incompatibility |

**Recommendation:** Generate a `client/src/shared-constants.ts` from the Rust source at build time. A simple build script that greps the Rust consts and emits a TS file. Add it as a Vite plugin or `prebuild` npm script. Alternatively, just add prominent `// MUST MATCH: crates/shared/src/spatial.rs:49` comments to each duplicated value.

**Effort:** Comments = trivial. Build-time codegen = medium.

---

## 7. UNO Q Board IPs Hardcoded in Shell Script (Low Impact)

`start-unoq.sh` has 4 IPs hardcoded. If a board gets a new DHCP lease, the script breaks.

```sh
UNOQ_IP=192.168.0.15
UNOQ2_IP=192.168.0.173
UNOQ3_IP=192.168.0.86
ZZTTPI_IP=192.168.0.208
```

These also appear in the TLS cert SAN list (line 42) and the test scripts.

**Recommendation:** Move to a `boards.env` file sourced by all UNO Q scripts. The boards likely have static DHCP leases so this rarely changes, but when it does, one file beats grep-and-replace.

```sh
# boards.env
UNOQ_IP=192.168.0.15
UNOQ2_IP=192.168.0.173
UNOQ3_IP=192.168.0.86
ZZTTPI_IP=192.168.0.208
```

**Effort:** Trivial.

---

## 8. Docker Compose Has No Variable Substitution (Low Impact)

`docker-compose.test.yml` hardcodes all ports, NPC counts, and resource limits per service. Docker Compose supports `.env` files and `${VAR:-default}` syntax natively.

**Recommendation:** Extract repeated values into a `.env` file. Use YAML anchors for the identical resource limits across all 3 sim-server services.

**Effort:** Low.

---

## 9. Kubernetes Manifests Lack Shared Configuration (Low Impact)

Three separate K8s YAML files each define their own resource limits, timeout values, and port numbers. No ConfigMap, no Kustomize, no Helm.

**Recommendation:** Not worth adding Helm for a prototype. But a single ConfigMap for shared values (ports, NPC counts, coordinator address) would reduce duplication. The existing `start-k8s.sh` could template the ConfigMap.

**Effort:** Low-medium.

---

## 10. Board Dashboard Port Is Hardcoded (Trivial)

`crates/board-dash/src/main.rs` binds to port 3000 with no CLI flag or env var.

**Recommendation:** Add a `--port` flag or `BOARD_DASH_PORT` env var. Low priority since only 3 boards run this and they all use 3000.

**Effort:** Trivial.

---

## Priority Order

| # | Recommendation | Impact | Effort | Do it? |
|---|---------------|--------|--------|--------|
| 1 | Central `config.sh` for ports | High | Low | Yes |
| 7 | `boards.env` for UNO Q IPs | Low | Trivial | Yes (quick win) |
| 2 | Consistent NPC counts | Medium | Trivial | Yes (quick win) |
| 5 | Client `config.ts` consolidation | Low | Low | Yes (clean refactor) |
| 6 | Shared constant comments/codegen | Medium | Low-Medium | Comments now, codegen later |
| 3 | Sim-server TOML config | Medium | Medium | Yes, when tuning gets painful |
| 4 | Coordinator TOML config | Medium | Low | Yes, bundle with #3 |
| 8 | Docker Compose `.env` | Low | Low | Nice to have |
| 9 | K8s ConfigMap | Low | Low-Medium | Only if K8s usage grows |
| 10 | Board dashboard port flag | Trivial | Trivial | Only if needed |
