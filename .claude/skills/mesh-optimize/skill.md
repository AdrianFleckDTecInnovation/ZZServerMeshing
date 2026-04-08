---
name: mesh-optimize
description: Optimize the server meshing system — benchmark, identify bottlenecks, implement fixes, verify, and deploy against the live AKS cluster.
user-invocable: true
---

# Server Meshing Optimization Workflow

You are optimizing a server meshing prototype running on AKS. Always test against the live cluster — local testing is not representative of real-world performance.

## Environment

- **AKS Gateway**: `ws://20.29.160.152` (get fresh IP with `kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}'`)
- **Gateway Metrics**: `http://<gateway-ip>:8200/metrics`
- **Sim-Server Metrics**: port-forward `kubectl port-forward sim-server-N 810N:810N`
- **Test Suite**: `cd tools && npx tsx test-suite/runner.ts ws://<gateway-ip> 8101 3 --no-color --json`
- **Browser Test**: `cd tools && npx tsx browser-test.ts ws://<gateway-ip>`
- **Deploy**: `./deploy/update.sh` (builds, pushes, restarts all pods)

## Pass Log

At the START of every run, read `.claude/skills/mesh-optimize/PASS_LOG.md`. This file tracks the last 5 optimization runs with metrics. Use it to understand what was tried before and what the current performance baseline is.

At the END of every run, append your results to `.claude/skills/mesh-optimize/PASS_LOG.md` and trim to keep only the last 5 entries. Each entry format:

```markdown
## Run [N] — [date] — [one-line summary]
- **Change**: what was optimized
- **Gateway aggregate_us**: before → after
- **Frame rate (Hz)**: before → after
- **Tick budget %**: before → after
- **Entity count**: total / deduped
- **Browser test**: PASS/FAIL (jumps: N, flicker: N)
- **Status**: PASS / FAIL / PARTIAL
- **Notes**: any observations
```

If `.claude/skills/mesh-optimize/PASS_LOG.md` doesn't exist, create it with a header.

## Workflow

### 1. Baseline Metrics
Before changing anything, capture current performance:
```bash
# Gateway metrics
curl -sf http://<gateway-ip>:8200/metrics | jq .

# Frame rate measurement
cd tools && node -e "
const ws = require('ws');
const w = new ws.WebSocket('ws://<gateway-ip>');
let times = [], count = 0;
w.on('message', (d, b) => { if (!b) return; times.push(Date.now()); count++;
  if (count >= 50) { let ivs = []; for (let i=1;i<times.length;i++) ivs.push(times[i]-times[i-1]);
    console.log('Hz: ' + (1000/(ivs.reduce((a,b)=>a+b,0)/ivs.length)).toFixed(1));
    w.close(); process.exit(0); }});
setTimeout(() => process.exit(1), 15000);
"

# Sim-server tick budget
kubectl port-forward sim-server-0 8101:8101 &
curl -sf http://localhost:8101/metrics | jq '.tick_budget_pct, .phases'
kill %1
```

### 2. Identify Bottleneck
Check these in order:
1. **Gateway aggregate_us** — how long to merge + serialize snapshots
2. **Sim-server tick_budget_pct** — how much of the 50ms tick budget is used
3. **Network frame rate** — are frames being dropped between gateway and client?
4. **Entity count discrepancy** — gateway total vs client-received (dedup overhead)
5. **Rebalance events** — is the coordinator causing unnecessary entity transfers?

### 3. Implement Optimization
Make code changes in the relevant crate/file. Key optimization targets:
- `crates/gateway/src/aggregator.rs` — snapshot merging, binary serialization, delta compression
- `crates/sim-server/src/simulation.rs` — game loop phases, entity iteration
- `crates/sim-server/src/replication.rs` — UDP packet sizes, serialization
- `crates/coordinator/src/registry.rs` — rebalance algorithm, thresholds
- `client/src/world-view.ts` — interpolation, entity management

### 4. Verify Locally
```bash
cargo build
cd client && npx tsc --noEmit
cd tools && npx tsc --noEmit --strict --target ES2022 --module nodenext --moduleResolution nodenext --skipLibCheck test-suite/runner.ts
```

### 5. Deploy to AKS
```bash
./deploy/update.sh
# Wait for pods to stabilize
sleep 60
kubectl get pods
```

### 6. Verify on AKS
Run these checks BEFORE telling the user anything works:

```bash
# A) Automated browser test — MUST pass with 0 position jumps
cd tools && npx tsx browser-test.ts ws://<gateway-ip>

# B) Frame rate check — should match tick rate
# (same node -e command from step 1)

# C) Gateway metrics — compare against baseline
curl -sf http://<gateway-ip>:8200/metrics | jq .
```

**CRITICAL**: Do NOT tell the user a fix works until the browser test passes. If it fails, fix the issue and re-test. Never ask the user to manually verify rendering.

### 7. Report Results
Compare before/after:
- Gateway CPU (from `docker stats` or `kubectl top pods`)
- aggregate_us (from gateway metrics)
- Frame rate (Hz measured from client)
- Entity count accuracy
- Position jump count from browser test
- Tick budget % from sim-server metrics

## Rules

1. **Always test against AKS** — local performance is not representative
2. **Never revert to slower implementations** — fix forward
3. **Never ask the user to manually test** — use `browser-test.ts` and automated tools
4. **Capture baseline before making changes** — can't measure improvement without it
5. **The browser test (`browser-test.ts`) is the source of truth** — if it says FAIL, you have a bug
6. **Get the gateway IP fresh** — it may change after redeployments:
   `kubectl get svc gateway -o jsonpath='{.status.loadBalancer.ingress[0].ip}'`
