# Session 2 Changes Summary — For Audit

All changes on `dev` branch since `main` (commit `3049ef6`). This covers Phase 1 (data integrity) and Phase 2 (tick loop health) from `docs/mesh-auditor/findings-review.md`, plus coordinator reconnection fixes and test infrastructure improvements.

## Change Categories

### 1. Delta Compression Fixes (Phase 1)

**Files:** `delta.rs`, `systems.rs`, `resources.rs`, `network.rs`

| Change | Finding | What |
|--------|---------|------|
| Vec3 distance threshold | #022 | `vec3_changed` uses `distance_squared()` instead of per-component `abs()` |
| Delta fallback | #027 | Send full snapshot when >80% entities changed (skip useless delta) |
| Pre-loop change ratio probe | #049 | `change_ratio()` computed once, skips per-client `compute_delta` when fallback fires |
| Stale-ACK detection | #021 | Force full snapshot after 40 ticks without ACK progress |
| ACK tick validation | #025 | Monotonicity check + server tick clamp on acked_tick |
| `last_sent_tick` tracking | — | Per-client field for stale-ACK detection |

**Verify:** Are these all correct and necessary? Is there dead code or over-engineering?

### 2. Tick Loop Health (Phase 2)

**Files:** `game_loop.rs`, `main.rs`, `receiver.rs`, `network.rs`, `coordinator_client.rs`, `metrics_server.rs`

| Change | Finding | What |
|--------|---------|------|
| tokio::Mutex → std::sync::Mutex | #003 | 15 lock sites across 6 files swapped |
| `process::exit(0)` → `break` | #009 | Game loop returns normally on drain |
| Dedicated UDP sender task | #013 | Dual channels (replication 256, control 64), `try_send`, biased select |
| Yield points | — | `yield_now().await` after ECS schedule and UDP enqueue |

**Verify:** Is the mutex swap safe? Are yield points in the right places? Is the dual-channel approach sound? Is the channel sizing correct?

### 3. Coordinator Reconnection

**Files:** `coordinator/registry.rs`, `coordinator/main.rs`, `shared/protocol.rs`, `sim-server/coordinator_client.rs`

| Change | What |
|--------|------|
| Re-registration on dead connection | Accept Register when old `reply_tx.is_closed()` |
| `unregister_if_sender_matches` | Prevent stale disconnect from removing new registration |
| `Deregistered` message | Coordinator sends before dropping dead server |
| 30s read timeout | Sim-server reconnects if no message for 30s |
| `Activity` event | Any WebSocket message refreshes heartbeat timestamp |
| Heartbeat timeout 5s → 30s | More headroom under load |
| `touch()` method | Refresh heartbeat without overwriting entity count |

**Verify:** Is all this plumbing necessary? Are there simpler alternatives? Any race conditions in the reconnection flow?

### 4. Test Infrastructure

**Files:** `runner.ts`, `scenarios/input-latency.ts`

| Change | What |
|--------|------|
| `wsOptions` hoisting fix | Was referenced before definition, silently failed on boards |
| Spawn distribution | Round-robin across all servers via coordinator `/servers` |
| `STEADY_SCALE_MAX` env var | Cap entity ceiling for board tests |
| Inter-test health check | Verify cluster health between scenarios |
| Cleanup all entity types | Was NPC-only, now removes players too |
| Cleanup verification + cooldown | Wait for entity removal and budget to drop |
| `reassignServer()` | Get fresh assignment if current server is dead |
| Input-latency player detection | Diff entity list before/after spawn instead of picking last |

**Verify:** Is the test runner still reliable? Any issues with the spawn distribution approach?

## Files to audit (production code only)

- `crates/sim-server/src/delta.rs` — change_ratio, vec3 threshold
- `crates/sim-server/src/game_loop.rs` — mutex swap, UDP sender, yield points
- `crates/sim-server/src/main.rs` — sender task spawn, dual channels
- `crates/sim-server/src/network.rs` — ACK validation
- `crates/sim-server/src/resources.rs` — last_sent_tick, ACK_STALE_TICKS
- `crates/sim-server/src/systems.rs` — delta fallback, stale ACK, change_ratio probe
- `crates/sim-server/src/receiver.rs` — mutex swap
- `crates/sim-server/src/coordinator_client.rs` — Deregistered handler, read timeout
- `crates/sim-server/src/metrics_server.rs` — mutex swap
- `crates/coordinator/src/registry.rs` — re-registration, touch, deregistered send, heartbeat timeout
- `crates/coordinator/src/main.rs` — Activity event, deregistered plumbing
- `crates/shared/src/protocol.rs` — Deregistered variant
