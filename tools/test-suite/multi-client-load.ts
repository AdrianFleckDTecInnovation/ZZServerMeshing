// Multi-client load driver: orchestrates N WebSocket clients per sim-server,
// each firing SpawnEntity at the per-client rate-limit (5/sec). Bypasses the
// per-client spawn cap to push total cluster load to a target entity count.
//
// Usage:
//   npx tsx multi-client-load.ts <target_total> <clients_per_server> <hold_seconds>
// Example:
//   npx tsx multi-client-load.ts 5000 8 60
//
// Behavior:
//   - Queries /servers for cluster topology
//   - Opens N clients per server (direct wss://host:port)
//   - Each client sends SpawnEntity at 4.8/sec (below the 5/sec rate limit)
//   - Holds all connections open for hold_seconds after quota reached,
//     sampling /metrics every 5s on each board
//   - Prints per-sample metrics table and a final summary
//
// No cleanup; entities persist post-run. Assumes entities stick (no TTL prunes).

import WebSocket from 'ws';

// Self-signed certs on the boards; skip verification for fetch()
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';

const coordUrl = process.env.COORDINATOR_URL || 'https://192.168.0.208:7871';

interface Server {
  server_id: number;
  host: string;
  ws_port: number;
  metrics_port: number;
  udp_port: number;
  entity_count: number;
}

interface Metrics {
  server_id: number;
  tick: number;
  entity_count: number;
  shadow_count: number;
  tick_budget_pct: number;
  phases: {
    total_ms: { p50: number; p95: number; p99: number };
    simulation_ms: { p50: number; p95: number; p99: number };
  };
  udp_repl_drops: number;
  udp_ctrl_drops: number;
  udp_bytes_out: number;
  peer_health: Record<string, { packets_received: number; latest_tick: number; session_changed_at_tick: number }>;
}

async function getServers(): Promise<Server[]> {
  const resp = await fetch(`${coordUrl}/servers`);
  const data = await resp.json() as { servers: Server[] };
  return data.servers;
}

async function getMetrics(s: Server): Promise<Metrics | null> {
  try {
    const resp = await fetch(`http://${s.host}:${s.metrics_port}/metrics`);
    if (!resp.ok) return null;
    return await resp.json() as Metrics;
  } catch {
    return null;
  }
}

async function connect(url: string, label: string): Promise<WebSocket> {
  return new Promise((res, rej) => {
    const ws = new WebSocket(url, { rejectUnauthorized: false });
    const to = setTimeout(() => rej(new Error(`${label}: connect timeout`)), 8000);
    ws.on('open', () => { clearTimeout(to); res(ws); });
    ws.on('error', (e) => { clearTimeout(to); rej(new Error(`${label}: ${(e as Error).message}`)); });
  });
}

// Each client runs this loop: spawn `quota` entities at `per_sec` rate.
async function clientSpawnLoop(ws: WebSocket, quota: number, per_sec: number, label: string, onSpawn: () => void): Promise<void> {
  const interval = 1000 / per_sec;
  for (let i = 0; i < quota; i++) {
    if (ws.readyState !== WebSocket.OPEN) {
      console.error(`[${label}] ws closed at ${i}/${quota}`);
      return;
    }
    const x = (Math.random() - 0.5) * 8000;
    const z = (Math.random() - 0.5) * 8000;
    ws.send(JSON.stringify({ type: 'SpawnEntity', position: [x, 0, z], entity_type: 'npc' }));
    onSpawn();
    await new Promise(r => setTimeout(r, interval));
  }
}

function fmtMetricsRow(m: Metrics | null, label: string): string {
  if (!m) return `${label}: <unreachable>`;
  const ph = m.peer_health || {};
  const peers = Object.entries(ph).map(([sid, h]) => `p${sid}:rx=${h.packets_received}`).join(' ');
  return `${label}: tick=${m.tick} auth=${m.entity_count} shadow=${m.shadow_count} budget=${m.tick_budget_pct.toFixed(1)}% sim_p95=${m.phases.simulation_ms.p95.toFixed(2)}ms total_p95=${m.phases.total_ms.p95.toFixed(2)}ms drops=${m.udp_repl_drops}/${m.udp_ctrl_drops} bytes_out=${m.udp_bytes_out} ${peers}`;
}

async function sampleAll(servers: Server[]): Promise<Metrics[]> {
  const results = await Promise.all(servers.map(s => getMetrics(s)));
  return results.filter((m): m is Metrics => m !== null);
}

async function main() {
  const target = parseInt(process.argv[2] || '5000');
  const clientsPerServer = parseInt(process.argv[3] || '8');
  const holdSec = parseInt(process.argv[4] || '60');

  const servers = await getServers();
  console.log(`Cluster: ${servers.length} servers`);
  for (const s of servers) {
    console.log(`  S${s.server_id}: ${s.host}:${s.ws_port} (metrics :${s.metrics_port}) auth=${s.entity_count}`);
  }

  const baseline = servers.reduce((sum, s) => sum + s.entity_count, 0);
  const toSpawn = Math.max(0, target - baseline);
  console.log(`\nBaseline total: ${baseline}, target: ${target}, to spawn: ${toSpawn}`);

  const totalClients = servers.length * clientsPerServer;
  const quotaPerClient = Math.ceil(toSpawn / totalClients);
  const perSec = 4.8; // below SPAWN_RATE_LIMIT=5/sec

  console.log(`Connecting ${totalClients} clients (${clientsPerServer} per server)`);
  console.log(`Each client quota: ${quotaPerClient}, rate: ${perSec}/sec`);
  console.log(`Expected spawn time: ${(quotaPerClient / perSec).toFixed(1)}s\n`);

  // Baseline metrics sample
  console.log('--- BASELINE METRICS ---');
  const baselineMetrics = await sampleAll(servers);
  for (const m of baselineMetrics) {
    console.log(fmtMetricsRow(m, `S${m.server_id}`));
  }
  console.log('');

  // Open all client connections first
  const conns: { ws: WebSocket; label: string; server: Server }[] = [];
  for (const s of servers) {
    const url = `wss://${s.host}:${s.ws_port}`;
    for (let i = 0; i < clientsPerServer; i++) {
      const label = `S${s.server_id}-c${i}`;
      try {
        const ws = await connect(url, label);
        conns.push({ ws, label, server: s });
      } catch (e: any) {
        console.error(`  ${label}: FAILED ${e.message}`);
      }
    }
  }
  console.log(`Connected ${conns.length}/${totalClients} clients\n`);

  if (conns.length === 0) {
    console.error('No clients connected; aborting');
    process.exit(1);
  }

  // Spawn phase: each client runs its quota in parallel
  console.log('--- SPAWN PHASE ---');
  const spawnStart = Date.now();
  let spawned = 0;
  let lastReport = Date.now();
  const spawnPromises = conns.map(c =>
    clientSpawnLoop(c.ws, quotaPerClient, perSec, c.label, () => {
      spawned++;
      const now = Date.now();
      if (now - lastReport >= 3000) {
        lastReport = now;
        console.log(`  [+${((now - spawnStart) / 1000).toFixed(1)}s] spawned ~${spawned}`);
      }
    })
  );
  await Promise.all(spawnPromises);
  const spawnElapsed = (Date.now() - spawnStart) / 1000;
  console.log(`  Spawn phase done: ${spawned} attempts in ${spawnElapsed.toFixed(1)}s\n`);

  // Settle briefly so shadows propagate
  console.log('--- SETTLING (5s) ---');
  await new Promise(r => setTimeout(r, 5000));

  // Steady-state sample loop (hold connections open)
  console.log(`--- STEADY STATE (${holdSec}s) ---`);
  const steadyStart = Date.now();
  const samples: Metrics[][] = [];
  const sampleInterval = 5000;
  while (Date.now() - steadyStart < holdSec * 1000) {
    const m = await sampleAll(servers);
    samples.push(m);
    const t = ((Date.now() - steadyStart) / 1000).toFixed(1);
    const totalAuth = m.reduce((s, x) => s + x.entity_count, 0);
    console.log(`[+${t}s] total_auth=${totalAuth}`);
    for (const mm of m) console.log(`  ${fmtMetricsRow(mm, `S${mm.server_id}`)}`);
    await new Promise(r => setTimeout(r, sampleInterval));
  }

  // Disconnect
  for (const c of conns) {
    try { c.ws.close(); } catch { /* ignore */ }
  }
  await new Promise(r => setTimeout(r, 500));

  // Final summary
  console.log('\n--- FINAL SUMMARY ---');
  const finalMetrics = await sampleAll(servers);
  let finalTotalAuth = 0;
  for (const m of finalMetrics) {
    console.log(fmtMetricsRow(m, `S${m.server_id}`));
    finalTotalAuth += m.entity_count;
  }
  console.log(`\nFinal total authoritative: ${finalTotalAuth} (target ${target})`);

  // Tick budget aggregates across steady state. udp_bytes_out is per-tick
  // (reset each tick in game_loop.rs:523), so *20 Hz = bytes/sec.
  const TICK_HZ = 20;
  const aggregate: Record<number, { budgets: number[]; simP95: number[]; totP95: number[]; drops: number; perTickBytes: number[] }> = {};
  for (const sample of samples) {
    for (const m of sample) {
      if (!aggregate[m.server_id]) aggregate[m.server_id] = { budgets: [], simP95: [], totP95: [], drops: 0, perTickBytes: [] };
      const a = aggregate[m.server_id];
      a.budgets.push(m.tick_budget_pct);
      a.simP95.push(m.phases.simulation_ms.p95);
      a.totP95.push(m.phases.total_ms.p95);
      a.drops = Math.max(a.drops, m.udp_repl_drops + m.udp_ctrl_drops);
      a.perTickBytes.push(m.udp_bytes_out);
    }
  }
  console.log('\n--- AGGREGATES (steady state) ---');
  let totalKBs = 0;
  for (const [sid, a] of Object.entries(aggregate)) {
    const avg = (xs: number[]) => xs.reduce((s, x) => s + x, 0) / Math.max(1, xs.length);
    const max = (xs: number[]) => Math.max(...xs);
    const bytesPerSec = avg(a.perTickBytes) * TICK_HZ;
    const peakBps = max(a.perTickBytes) * TICK_HZ;
    totalKBs += bytesPerSec / 1024;
    console.log(`S${sid}: budget_avg=${avg(a.budgets).toFixed(1)}% budget_max=${max(a.budgets).toFixed(1)}% sim_p95_avg=${avg(a.simP95).toFixed(2)}ms tot_p95_avg=${avg(a.totP95).toFixed(2)}ms tot_p95_max=${max(a.totP95).toFixed(2)}ms drops=${a.drops} udp_out_avg=${(bytesPerSec / 1024).toFixed(0)}KB/s peak=${(peakBps / 1024).toFixed(0)}KB/s`);
  }
  console.log(`Cluster total UDP out: ${totalKBs.toFixed(0)} KB/s (${(totalKBs / 1024).toFixed(2)} MB/s)`);
}

main().catch(e => { console.error(e); process.exit(1); });
