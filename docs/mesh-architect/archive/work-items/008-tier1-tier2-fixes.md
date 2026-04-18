# Work Items: Tier 1 & 2 Fixes (008)

Items remaining after verifying existing code against auditor findings.
Many Tier 1 items were already fixed in prior work.

## WI-080: Reassembly entry cap and per-server stale cleanup

**File:** `crates/sim-server/src/receiver.rs`

The global reassembly sweep uses a single global max tick to evict entries.
This can evict entries from legitimately slow peers (e.g., newly joined server at tick 0).
Also no cap on max reassembly entries — crashed peers that never complete chunks grow HashMap without bound.

**Changes:**
1. Replace global max tick with per-server latest tick in `latest_tick` HashMap (already tracked)
2. Use per-server latest tick for stale eviction instead of global max
3. Add `MAX_REASSEMBLY_ENTRIES = 1000` constant. When exceeded, evict oldest entries first.

## WI-081: Client input rate limiting

**File:** `crates/sim-server/src/network.rs`

No rate limiting on client inputs. A single client can flood with unlimited MoveInput/SpawnEntity messages.

**Changes:**
1. Add a simple token bucket rate limiter per client in `handle_client`
2. Constants: `INPUT_RATE_LIMIT = 30` messages/sec, `SPAWN_RATE_LIMIT = 5` messages/sec
3. Track last message timestamps; drop excess messages with a warn log
4. Use a lightweight approach — just track count + window, not a full token bucket library

## WI-082: Reduce coordinator read timeout

**File:** `crates/sim-server/src/coordinator_client.rs`

The 120-second WebSocket read timeout delays crash detection. The coordinator already sends keepalives every 5 seconds on stable clusters.

**Changes:**
1. Reduce `read_timeout` from 120s to 30s
2. That's it — the coordinator `broadcast_keepalive()` already runs every 5s on the rebalance interval

## WI-083: Weighted rebalance algorithm

**Files:**
- `crates/shared/src/protocol.rs` — Add weighted_load to Heartbeat
- `crates/sim-server/src/coordinator_client.rs` — Send weighted count in heartbeat
- `crates/sim-server/src/systems.rs` — Compute weighted count
- `crates/coordinator/src/registry.rs` — Use weighted_load for rebalance decisions

**Changes:**
1. Add `weighted_load: u32` field to `CoordClientMsg::Heartbeat` (with `#[serde(default)]` for backward compat)
2. Sim-server computes: `weighted = (player_count * 3) + npc_count`
3. Heartbeat includes both `entity_count` (raw) and `weighted_load`
4. Coordinator stores weighted_load in ServerEntry
5. Rebalance algorithm uses weighted_load instead of entity_count for imbalance detection
6. Assignment (`get_least_loaded_ws_addr`) uses weighted_load for client routing

## WI-084: Shared snapshot encoding for matching AoI sets

**File:** `crates/sim-server/src/systems.rs` (prepare_client_snapshot)

Currently encodes an identical binary snapshot N times when clients share the same player position (same AoI filter result). The `ClientFrame::Shared(Arc<Vec<u8>>)` variant exists but is unused.

**Changes:**
1. Group clients by AoI center (player position, or "no player" bucket)
2. For each group, encode the snapshot once into `Arc<Vec<u8>>`
3. Send `ClientFrame::Shared(arc)` to all clients in the group
4. Delta frames remain per-client (different baselines)
5. Only applies to full snapshot frames (periodic full, stale ACK, no baseline)

Note: This optimization has diminishing returns with AoI filtering since each client's player is in a different position. Primary benefit is during startup (multiple clients, no players yet) or when players cluster together.
