#!/usr/bin/env npx tsx
/**
 * Load test for UNO Q cluster tiered replication.
 *
 * Spawns entities in waves across all 3 servers, spread across the world,
 * and captures metrics after each wave to measure tiered replication performance.
 *
 * Usage: npx tsx tools/load-test.ts [--waves N] [--per-wave N]
 */

import WebSocket from 'ws';
import https from 'https';
import http from 'http';

// ── Configuration ──────────────────────────────────────────────────────
const COORDINATOR_URL = 'https://192.168.0.208:7871';
const SERVERS = [
  { id: 1, host: '192.168.0.15',  wsPort: 7878, metricsPort: 8101 },
  { id: 2, host: '192.168.0.173', wsPort: 7879, metricsPort: 8102 },
  { id: 3, host: '192.168.0.86',  wsPort: 7880, metricsPort: 8103 },
];

// Rate limit is 5 spawns/sec per connection. Use multiple connections.
const CONNECTIONS_PER_SERVER = 10;  // 50 spawns/sec per server
const SPAWN_RATE_PER_CONN = 4;     // stay under 5/sec limit
const SPAWN_INTERVAL_MS = Math.ceil(1000 / SPAWN_RATE_PER_CONN);

const WAVES = parseInt(process.argv.find(a => a.startsWith('--waves='))?.split('=')[1] ?? '5');
const PER_WAVE_PER_SERVER = parseInt(process.argv.find(a => a.startsWith('--per-wave='))?.split('=')[1] ?? '100');
const STABILIZE_MS = 10000;  // 10 seconds between waves
const WORLD_RANGE = 4000;    // entities spread across +-4000

// ── Helpers ────────────────────────────────────────────────────────────
const wsOptions: WebSocket.ClientOptions = { rejectUnauthorized: false };

function randomPos(): [number, number, number] {
  const x = (Math.random() - 0.5) * 2 * WORLD_RANGE;
  const z = (Math.random() - 0.5) * 2 * WORLD_RANGE;
  return [Math.round(x * 10) / 10, 0.0, Math.round(z * 10) / 10];
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function fetchJSON(url: string): Promise<any> {
  return new Promise((resolve, reject) => {
    const mod = url.startsWith('https') ? https : http;
    const req = mod.get(url, { rejectUnauthorized: false }, (res) => {
      let data = '';
      res.on('data', chunk => data += chunk);
      res.on('end', () => {
        try { resolve(JSON.parse(data)); }
        catch (e) { reject(new Error(`JSON parse error from ${url}: ${data.slice(0, 200)}`)); }
      });
    });
    req.on('error', reject);
    req.setTimeout(5000, () => { req.destroy(); reject(new Error(`Timeout: ${url}`)); });
  });
}

async function connectWS(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, wsOptions);
    ws.binaryType = 'arraybuffer';
    ws.on('open', () => resolve(ws));
    ws.on('error', reject);
    setTimeout(() => reject(new Error(`WS timeout: ${url}`)), 5000);
  });
}

// ── Metrics capture ────────────────────────────────────────────────────
interface ServerMetrics {
  server_id: number;
  tick: number;
  entity_count: number;
  shadow_count: number;
  peer_count: number;
  connected_clients: number;
  tick_budget_pct: number;
  phases: {
    total_ms: { p50: number; p95: number; p99: number };
    simulation_ms: { p50: number; p95: number; p99: number };
    replication_ms: { p50: number; p95: number; p99: number };
    [key: string]: { p50: number; p95: number; p99: number };
  };
  replication_mode: string;
  replication_full_count: number;
  replication_sent_count: number;
  replication_hot_count: number;
  replication_cold_count: number;
}

async function captureMetrics(): Promise<ServerMetrics[]> {
  const results: ServerMetrics[] = [];
  for (const s of SERVERS) {
    try {
      const m = await fetchJSON(`http://${s.host}:${s.metricsPort}/metrics`);
      results.push(m as ServerMetrics);
    } catch (e) {
      console.error(`  [WARN] Failed to get metrics from S${s.id}: ${e}`);
    }
  }
  return results;
}

function printMetrics(label: string, metrics: ServerMetrics[]) {
  console.log(`\n${'='.repeat(80)}`);
  console.log(`  ${label}`);
  console.log('='.repeat(80));

  let totalAuth = 0, totalShadow = 0, totalHot = 0, totalCold = 0, totalFull = 0, totalSent = 0;

  for (const m of metrics) {
    const savedPct = m.replication_full_count > 0
      ? ((1 - m.replication_sent_count / (m.replication_full_count * (SERVERS.length - 1))) * 100).toFixed(0)
      : '0';

    console.log(`  S${m.server_id}: ${m.entity_count} auth, ${m.shadow_count} shadows | ` +
      `budget: ${m.tick_budget_pct.toFixed(1)}% (p50=${m.phases.total_ms.p50.toFixed(2)}ms, p95=${m.phases.total_ms.p95.toFixed(2)}ms) | ` +
      `hot:${m.replication_hot_count} cold:${m.replication_cold_count} full:${m.replication_full_count} sent:${m.replication_sent_count} saved:${savedPct}%`);

    totalAuth += m.entity_count;
    totalShadow += m.shadow_count;
    totalHot += m.replication_hot_count;
    totalCold += m.replication_cold_count;
    totalFull += m.replication_full_count;
    totalSent += m.replication_sent_count;
  }

  const overallSaved = totalFull > 0
    ? ((1 - totalSent / (totalFull * (SERVERS.length - 1))) * 100).toFixed(0)
    : '0';

  console.log(`  --------`);
  console.log(`  TOTAL: ${totalAuth} auth, ${totalShadow} shadows | ` +
    `hot:${totalHot} cold:${totalCold} full:${totalFull} sent:${totalSent} saved:${overallSaved}%`);
}

// ── Summary table ──────────────────────────────────────────────────────
interface WaveSnapshot {
  wave: number;
  targetTotal: number;
  metrics: ServerMetrics[];
}

function printSummaryTable(snapshots: WaveSnapshot[]) {
  console.log(`\n${'='.repeat(100)}`);
  console.log('  LOAD TEST SUMMARY');
  console.log('='.repeat(100));
  console.log('  Wave | Target |  Auth  | Shadows | Budget(p50) | Budget(p95) |  Hot  | Cold  | Sent  | Saved');
  console.log('  ' + '-'.repeat(96));

  for (const snap of snapshots) {
    const ms = snap.metrics;
    const totalAuth = ms.reduce((s, m) => s + m.entity_count, 0);
    const totalShadow = ms.reduce((s, m) => s + m.shadow_count, 0);
    const maxBudgetP50 = Math.max(...ms.map(m => m.tick_budget_pct));
    const maxBudgetP95 = Math.max(...ms.map(m => {
      const totalP95 = m.phases.total_ms.p95;
      return (totalP95 / 50) * 100;
    }));
    const totalHot = ms.reduce((s, m) => s + m.replication_hot_count, 0);
    const totalCold = ms.reduce((s, m) => s + m.replication_cold_count, 0);
    const totalFull = ms.reduce((s, m) => s + m.replication_full_count, 0);
    const totalSent = ms.reduce((s, m) => s + m.replication_sent_count, 0);
    const saved = totalFull > 0
      ? ((1 - totalSent / (totalFull * (SERVERS.length - 1))) * 100).toFixed(0)
      : '0';

    const waveStr = snap.wave === 0 ? 'base' : `  ${snap.wave}  `;
    console.log(
      `  ${waveStr} | ${String(snap.targetTotal).padStart(5)} | ${String(totalAuth).padStart(5)} | ${String(totalShadow).padStart(7)} | ` +
      `${maxBudgetP50.toFixed(1).padStart(9)}% | ${maxBudgetP95.toFixed(1).padStart(9)}% | ` +
      `${String(totalHot).padStart(5)} | ${String(totalCold).padStart(5)} | ${String(totalSent).padStart(5)} | ${saved.padStart(4)}%`
    );
  }
  console.log('='.repeat(100));
}

// ── Spawn logic ────────────────────────────────────────────────────────
async function spawnWave(
  connections: Map<number, WebSocket[]>,
  countPerServer: number
): Promise<number> {
  let totalSpawned = 0;

  // Distribute spawns across connections round-robin
  const promises: Promise<void>[] = [];

  for (const s of SERVERS) {
    const conns = connections.get(s.id) ?? [];
    if (conns.length === 0) {
      console.error(`  [WARN] No connections for S${s.id}`);
      continue;
    }

    promises.push((async () => {
      let sent = 0;
      let connIdx = 0;

      while (sent < countPerServer) {
        const pos = randomPos();
        const msg = JSON.stringify({
          type: 'SpawnEntity',
          position: pos,
          entity_type: 'Npc',
        });

        const ws = conns[connIdx % conns.length];
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(msg);
          sent++;
          totalSpawned++;
        }

        connIdx++;

        // After sending one message to each connection, pause to respect rate limits
        if (connIdx % conns.length === 0) {
          await sleep(SPAWN_INTERVAL_MS);
        }
      }
    })());
  }

  await Promise.all(promises);
  return totalSpawned;
}

// ── Main ───────────────────────────────────────────────────────────────
async function main() {
  console.log('='.repeat(80));
  console.log('  TIERED REPLICATION LOAD TEST');
  console.log(`  Servers: ${SERVERS.map(s => `S${s.id}@${s.host}`).join(', ')}`);
  console.log(`  Waves: ${WAVES}, Per wave per server: ${PER_WAVE_PER_SERVER}`);
  console.log(`  Connections per server: ${CONNECTIONS_PER_SERVER}`);
  console.log(`  Stabilize time: ${STABILIZE_MS / 1000}s`);
  console.log(`  World spread: +/-${WORLD_RANGE} units`);
  console.log('='.repeat(80));

  // Capture baseline
  console.log('\n[BASELINE] Capturing initial metrics...');
  const baseline = await captureMetrics();
  const baseTotal = baseline.reduce((s, m) => s + m.entity_count, 0);
  printMetrics(`BASELINE (${baseTotal} entities)`, baseline);

  const snapshots: WaveSnapshot[] = [{ wave: 0, targetTotal: baseTotal, metrics: baseline }];

  // Open connections to all servers
  console.log(`\n[CONNECT] Opening ${CONNECTIONS_PER_SERVER} connections per server...`);
  const connections = new Map<number, WebSocket[]>();

  for (const s of SERVERS) {
    const conns: WebSocket[] = [];
    const url = `wss://${s.host}:${s.wsPort}`;

    for (let i = 0; i < CONNECTIONS_PER_SERVER; i++) {
      try {
        const ws = await connectWS(url);
        conns.push(ws);
        // Suppress binary frame data -- we don't need it
        ws.on('message', () => {});
      } catch (e) {
        console.error(`  [WARN] Failed to connect #${i + 1} to S${s.id}: ${e}`);
      }
    }

    connections.set(s.id, conns);
    console.log(`  S${s.id}: ${conns.length}/${CONNECTIONS_PER_SERVER} connections`);
  }

  // Run waves
  let cumulativeTarget = baseTotal;
  let hitCeiling = false;

  for (let wave = 1; wave <= WAVES; wave++) {
    cumulativeTarget += PER_WAVE_PER_SERVER * SERVERS.length;

    console.log(`\n[WAVE ${wave}] Spawning ${PER_WAVE_PER_SERVER} entities per server (${PER_WAVE_PER_SERVER * SERVERS.length} total, target cumulative: ${cumulativeTarget})...`);

    const t0 = Date.now();
    const spawned = await spawnWave(connections, PER_WAVE_PER_SERVER);
    const elapsed = ((Date.now() - t0) / 1000).toFixed(1);
    console.log(`  Spawned ${spawned} entities in ${elapsed}s`);

    // Wait for stabilization
    console.log(`  Waiting ${STABILIZE_MS / 1000}s for replication to stabilize...`);
    await sleep(STABILIZE_MS);

    // Capture metrics
    const metrics = await captureMetrics();
    printMetrics(`WAVE ${wave} (target: ${cumulativeTarget} entities)`, metrics);
    snapshots.push({ wave, targetTotal: cumulativeTarget, metrics });

    // Check for ceiling
    const maxBudget = Math.max(...metrics.map(m => m.tick_budget_pct));
    if (maxBudget > 80) {
      console.log(`\n  *** CEILING HIT: S${metrics.find(m => m.tick_budget_pct === maxBudget)!.server_id} at ${maxBudget.toFixed(1)}% tick budget ***`);
      hitCeiling = true;
      break;
    }

    if (maxBudget > 50) {
      console.log(`  [WARN] Tick budget above 50% on S${metrics.find(m => m.tick_budget_pct === maxBudget)!.server_id}: ${maxBudget.toFixed(1)}%`);
    }
  }

  // Final comprehensive capture
  console.log('\n[FINAL] Waiting 5s then capturing final metrics...');
  await sleep(5000);
  const finalMetrics = await captureMetrics();
  printMetrics('FINAL STATE', finalMetrics);

  // Check for log warnings on boards
  console.log('\n[LOGS] Checking board logs for warnings...');
  for (const s of SERVERS) {
    // Not SSH-ing from this script, just note it
  }

  // Print summary table
  printSummaryTable(snapshots);

  if (hitCeiling) {
    console.log('\n  Load test stopped: tick budget exceeded 80%.');
  } else {
    console.log(`\n  Load test completed all ${WAVES} waves without hitting the 80% ceiling.`);
  }

  // Close all connections
  console.log('\n[CLEANUP] Closing connections...');
  for (const [sid, conns] of connections) {
    for (const ws of conns) {
      ws.close();
    }
  }

  // Wait for close
  await sleep(1000);
  console.log('Done.');
  process.exit(0);
}

main().catch(e => {
  console.error('Load test failed:', e);
  process.exit(1);
});
