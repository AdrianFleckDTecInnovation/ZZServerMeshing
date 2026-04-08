---
name: mesh-dev
description: Develop features for the server meshing system — implement, test against AKS, verify with Playwright, and deploy.
user-invocable: true
---

# Server Meshing Development Workflow

You are developing features for a server meshing prototype where multiple Rust sim-servers share authority over the same spatial area, distributing entities at the entity level.

## First Steps

1. Read `.claude/skills/mesh-dev/CHANGELOG.md` for history of completed features and known issues
2. Read `CLAUDE.md` for architecture, conventions, and build/run commands
3. Read `.claude/rules/llm-directions.md` for the full system design

## Architecture Quick Reference

```
Browser ←─ binary/ws ─→ Gateway (Rust) ←─ json/ws ─→ Sim-Servers ←─ udp/bincode ─→ each other
                              │                            │
                              └──── json/ws ────→ Coordinator ←──── json/ws ──┘
```

| Service | Crate | Purpose |
|---------|-------|---------|
| Coordinator | `crates/coordinator/` | Server registry, heartbeats, crash detection, rebalance directives |
| Sim-Server | `crates/sim-server/` | Game loop (20Hz), entity simulation, UDP replication, authority transfers |
| Gateway | `crates/gateway/` | Aggregates sim-server snapshots, binary protocol to clients, input routing |
| Shared | `crates/shared/` | Entity types, protocol messages, tick constants |
| Client | `client/` | Three.js WebGPU renderer, binary decoder, interpolation |

## AKS Cluster

Always verify against the live AKS cluster — local testing is not representative.

```bash
# Get gateway IP (may change after redeploys)
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')

# Gateway metrics
curl -sf http://${GW_IP}:8200/metrics | jq .

# Sim-server metrics (port-forward)
kubectl port-forward sim-server-0 8101:8101 &
curl -sf http://localhost:8101/metrics | jq .

# View client locally against AKS
./deploy/client-local.sh

# Deploy changes
./deploy/update.sh          # rebuild + restart all
./deploy/update.sh --client # also rebuild client for App Service
```

## Development Cycle

### 1. Plan
Understand what needs to change. Check CHANGELOG.md for prior work. Use `EnterPlanMode` for non-trivial features.

### 2. Implement
Key files by change type:

| Change | Files |
|--------|-------|
| New entity field | `shared/entity.rs` → auto-propagates via serde. Update `client/world-view.ts` RawEntity to match. |
| New coordinator→server message | `shared/protocol.rs` CoordServerMsg → handle in `sim-server/coordinator_client.rs` |
| New client→server message | `shared/protocol.rs` ClientMessage → handle in `sim-server/simulation.rs:process_input()`, update `gateway/src/main.rs:route_input()` |
| Tick rate change | `shared/tick.rs` — affects sim-server, gateway broadcast, client interpolation |
| Gateway binary protocol | `gateway/aggregator.rs` (encode) + `client/binary-protocol.ts` (decode) + `tools/test-suite/runner.ts` (test decode) |
| Rebalance behavior | `coordinator/registry.rs:try_rebalance()` |
| Crash recovery | `coordinator/registry.rs:broadcast_recovery()` + `sim-server/world.rs:recover_from_dead_server()` |

### 3. Build & Type-check
```bash
cargo build                                    # All Rust crates
cd client && npx tsc --noEmit                  # Client TypeScript
cd tools && npx tsc --noEmit --strict --target ES2022 --module nodenext --moduleResolution nodenext --skipLibCheck test-suite/runner.ts  # Test suite
```

### 4. Deploy to AKS
```bash
./deploy/update.sh
sleep 60  # wait for rolling restart to settle
kubectl get pods  # verify all Running
```

### 5. Verify
Run ALL of these before declaring success:

```bash
GW_IP=$(kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}')

# A) Browser test — MUST pass (0 jumps, 0 flicker)
cd tools && npx tsx browser-test.ts ws://${GW_IP}

# B) Gateway metrics — check entity count, server count, rebalance events
curl -sf http://${GW_IP}:8200/metrics | jq .

# C) Frame rate — should be ~20Hz
node -e "
const ws=require('ws'),w=new ws.WebSocket('ws://${GW_IP}');
let t=[],c=0;w.on('message',(d,b)=>{if(!b)return;t.push(Date.now());c++;
if(c>=50){let i=[];for(let j=1;j<t.length;j++)i.push(t[j]-t[j-1]);
console.log((1000/(i.reduce((a,b)=>a+b,0)/i.length)).toFixed(1)+' Hz');
w.close();process.exit(0)}});setTimeout(()=>process.exit(1),15000);
"

# D) Coordinator — no oscillating rebalances
kubectl logs deployment/coordinator --tail=10
```

**CRITICAL**: Do NOT tell the user a feature works until browser-test.ts PASSES. Never ask the user to manually verify.

### 6. Update CHANGELOG
Append to `.claude/skills/mesh-dev/CHANGELOG.md`.

## Known Gotchas

- **Rolling restarts cause rebalance storms** — the coordinator sees pod termination as a crash. Each sim-server restart triggers recovery + rebalance. Wait 60s after deploy for it to settle.
- **Entity dedup** — during authority transfers, an entity briefly exists on 2 servers. The gateway deduplicates by preferring the server that matches `authority_server`. Entity count in metrics may differ from client-visible count.
- **Binary protocol changes** — if you change the gateway's binary frame format, you MUST update: `client/binary-protocol.ts`, `tools/test-suite/runner.ts` decoder, and `tools/browser-test.ts` diagnostics.
- **Duplicate ticks** — the gateway may broadcast the same data twice if no new server snapshot arrived. The client skips duplicate ticks in `applySnapshot`.
- **Interpolation warmup** — first 3 snapshots after connection snap entities to position without interpolation to prevent initial warping.
- **Recovery dedup** — when a server dies, survivors split orphaned entities by UUID hash so no entity is adopted by multiple servers.
- **Rebalance threshold** — 20% of average AND 10-entity minimum. Below this the coordinator does nothing. Too aggressive = oscillation.
- **Delta compression disabled** — `FULL_SNAPSHOT_INTERVAL=1` in `gateway/aggregator.rs`. The infrastructure is built (entity_changed, mark_broadcast, delta frame type 0x02) but it caused interpolation bugs. Needs per-entity interpolation timers before re-enabling.

## Conventions

- Rust: `cargo fmt`, no unsafe. HashMap entity storage, not ECS.
- TypeScript: strict mode, ES2022. Three.js WebGPU with `three/webgpu` imports.
- Entity IDs: UUID v4. Server IDs: u16, 1-indexed. 0 = gateway.
- World bounds: -500 to 500 on X/Z. Y always 0.
- Binary protocol: little-endian, frame type byte first (0x01=full, 0x02=delta).
