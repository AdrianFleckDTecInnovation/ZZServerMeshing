#!/usr/bin/env npx tsx
/**
 * Staged load test against UNO Q cluster.
 * Ramps to supplied entity counts (e.g. 500 1500 3000 5000 8000 10000).
 * At each stage waits for stabilization, then captures full metrics:
 * - Per-server auth/shadow/tick/budget/udp_drops/peer_health/phases
 * - Coordinator /servers: in_flight_transfers and ttl_prunes_total
 *
 * Reads target totals from argv. Does not remove entities between stages
 * (entity count grows cumulatively). Uses ~evenly distributed spawns across
 * all 3 servers via direct WSS connections.
 */

import WebSocket from 'ws';
import https from 'https';
import http from 'http';

const COORDINATOR_URL = 'https://192.168.0.208:7871';
const SERVERS = [
  { id: 1, host: '192.168.0.15',  wsPort: 7878, metricsPort: 8101 },
  { id: 2, host: '192.168.0.173', wsPort: 7879, metricsPort: 8102 },
  { id: 3, host: '192.168.0.86',  wsPort: 7880, metricsPort: 8103 },
];

const CONNS_PER_SERVER = 10;
const SPAWN_INTERVAL_MS = 250; // ~4/s per connection per server -> ~40/s per server -> ~120/s total
const STABILIZE_MS = 12000;    // wait after ramp before metrics
const WORLD_RANGE = 4000;

const wsOptions: WebSocket.ClientOptions = { rejectUnauthorized: false };

function sleep(ms: number): Promise<void> {
  return new Promise(r => setTimeout(r, ms));
}

function fetchJSON(url: string, timeoutMs = 5000): Promise<any> {
  return new Promise((resolve, reject) => {
    const mod = url.startsWith('https') ? https : http;
    const req = mod.get(url, { rejectUnauthorized: false }, (res) => {
      let data = '';
      res.on('data', c => data += c);
      res.on('end', () => {
        try { resolve(JSON.parse(data)); }
        catch { reject(new Error(`bad JSON from ${url}: ${data.slice(0,200)}`)); }
      });
    });
    req.on('error', reject);
    req.setTimeout(timeoutMs, () => { req.destroy(); reject(new Error(`timeout ${url}`)); });
  });
}

async function connectWS(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, wsOptions);
    ws.binaryType = 'arraybuffer';
    ws.on('open', () => resolve(ws));
    ws.on('error', reject);
    ws.on('message', () => {}); // drain
    setTimeout(() => reject(new Error(`ws timeout ${url}`)), 7000);
  });
}

function randomPos(): [number, number, number] {
  const x = (Math.random() - 0.5) * 2 * WORLD_RANGE;
  const z = (Math.random() - 0.5) * 2 * WORLD_RANGE;
  return [Math.round(x * 10) / 10, 0.0, Math.round(z * 10) / 10];
}

interface SnapshotRow {
  target: number;
  totalAuth: number;
  totalShadow: number;
  perServer: Array<{ id: number; tick: number; auth: number; shadow: number; budget: number; p50: number; p95: number; p99: number; udpDrops: number; udpCtrl: number; bytesOut: number; peers: Array<{id:number,stale:number,session:string,evict:number}> }>;
  inFlight: number[];
  ttlPrunes: number;
}

async function snapshot(target: number): Promise<SnapshotRow> {
  const perServer: SnapshotRow['perServer'] = [];
  for (const s of SERVERS) {
    try {
      const m: any = await fetchJSON(`http://${s.host}:${s.metricsPort}/metrics`);
      const peers = Object.entries(m.peer_health || {}).map(([pid, v]: any) => ({
        id: Number(pid),
        stale: v.staleness_ticks,
        session: String(v.session_id).slice(0,8),
        evict: v.chunk_evictions ?? 0,
      }));
      perServer.push({
        id: m.server_id,
        tick: m.tick,
        auth: m.entity_count,
        shadow: m.shadow_count,
        budget: m.tick_budget_pct,
        p50: m.phases.total_ms.p50,
        p95: m.phases.total_ms.p95,
        p99: m.phases.total_ms.p99,
        udpDrops: m.udp_repl_drops ?? 0,
        udpCtrl: m.udp_ctrl_drops ?? 0,
        bytesOut: m.udp_bytes_out ?? 0,
        peers,
      });
    } catch (e) {
      console.error(`  [WARN] metrics S${s.id} failed: ${e}`);
    }
  }
  let inFlight: number[] = [];
  let ttlPrunes = 0;
  try {
    const sv: any = await fetchJSON(`${COORDINATOR_URL}/servers`);
    inFlight = sv.servers.map((x:any) => x.in_flight_transfers);
    ttlPrunes = sv.ttl_prunes_total;
  } catch (e) {
    console.error(`  [WARN] /servers failed: ${e}`);
  }
  const totalAuth = perServer.reduce((s,p) => s+p.auth, 0);
  const totalShadow = perServer.reduce((s,p) => s+p.shadow, 0);
  return { target, totalAuth, totalShadow, perServer, inFlight, ttlPrunes };
}

function printSnapshot(label: string, snap: SnapshotRow) {
  console.log(`\n===== ${label} =====`);
  console.log(`target=${snap.target} totalAuth=${snap.totalAuth} totalShadow=${snap.totalShadow} in_flight=${JSON.stringify(snap.inFlight)} ttl_prunes=${snap.ttlPrunes}`);
  for (const s of snap.perServer) {
    console.log(`  S${s.id}: tick=${s.tick} auth=${s.auth} shadow=${s.shadow} budget=${s.budget.toFixed(1)}% p50=${s.p50.toFixed(2)}ms p95=${s.p95.toFixed(2)}ms p99=${s.p99.toFixed(2)}ms udp_drops=${s.udpDrops}/${s.udpCtrl} bytes_out=${s.bytesOut}`);
    for (const p of s.peers) {
      console.log(`     peer ${p.id}: staleness=${p.stale} session=${p.session} evict=${p.evict}`);
    }
  }
}

async function spawnTo(target: number, connections: Map<number, WebSocket[]>, current: number): Promise<void> {
  const perServer = Math.ceil(Math.max(0, target - current) / SERVERS.length);
  if (perServer === 0) return;
  console.log(`  spawning ~${perServer} per server (total ~${perServer*SERVERS.length})`);
  const promises: Promise<void>[] = [];
  for (const s of SERVERS) {
    const conns = connections.get(s.id) ?? [];
    if (!conns.length) continue;
    promises.push((async () => {
      let sent = 0; let idx = 0;
      while (sent < perServer) {
        const ws = conns[idx % conns.length];
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'SpawnEntity', position: randomPos(), entity_type: 'Npc' }));
          sent++;
        }
        idx++;
        if (idx % conns.length === 0) await sleep(SPAWN_INTERVAL_MS);
      }
    })());
  }
  await Promise.all(promises);
}

async function main() {
  const targets = process.argv.slice(2).map(n => parseInt(n, 10)).filter(n => !isNaN(n));
  if (!targets.length) {
    console.error('Usage: staged-load.ts 500 1500 3000 5000 8000 10000');
    process.exit(2);
  }

  console.log(`STAGED LOAD TEST: targets=${targets.join(',')}, conns/server=${CONNS_PER_SERVER}, stabilize=${STABILIZE_MS}ms`);

  const base = await snapshot(0);
  printSnapshot('BASELINE', base);

  console.log(`\nOpening ${CONNS_PER_SERVER} conns per server...`);
  const connections = new Map<number, WebSocket[]>();
  for (const s of SERVERS) {
    const conns: WebSocket[] = [];
    for (let i = 0; i < CONNS_PER_SERVER; i++) {
      try { conns.push(await connectWS(`wss://${s.host}:${s.wsPort}`)); }
      catch (e) { console.error(`  ws conn #${i+1} to S${s.id} failed: ${e}`); }
    }
    connections.set(s.id, conns);
    console.log(`  S${s.id}: ${conns.length}/${CONNS_PER_SERVER}`);
  }

  const snaps: { label: string; snap: SnapshotRow }[] = [{ label: 'baseline', snap: base }];
  let cur = base.totalAuth;

  for (const t of targets) {
    const t0 = Date.now();
    await spawnTo(t, connections, cur);
    const rampSecs = ((Date.now()-t0)/1000).toFixed(1);
    console.log(`  ramp finished in ${rampSecs}s; stabilizing ${STABILIZE_MS/1000}s...`);
    await sleep(STABILIZE_MS);
    const snap = await snapshot(t);
    printSnapshot(`STAGE target=${t}`, snap);
    snaps.push({ label: `stage-${t}`, snap });
    cur = snap.totalAuth;
    const maxBudget = Math.max(...snap.perServer.map(p => p.budget));
    if (maxBudget > 90) {
      console.log(`\n*** BUDGET CEILING: max=${maxBudget.toFixed(1)}% — stopping ramp ***`);
      break;
    }
  }

  console.log('\nSUMMARY');
  console.log('label     | target |  auth | shadow | maxBudget | maxP95 | inFlight | ttlPrunes | udpDrops');
  console.log('----------|--------|-------|--------|-----------|--------|----------|-----------|---------');
  for (const { label, snap } of snaps) {
    const mb = Math.max(0, ...snap.perServer.map(p => p.budget)).toFixed(1);
    const mp = Math.max(0, ...snap.perServer.map(p => p.p95)).toFixed(2);
    const ud = snap.perServer.reduce((s,p) => s+p.udpDrops, 0);
    const inF = snap.inFlight.join('/');
    console.log(`${label.padEnd(9)} | ${String(snap.target).padStart(6)} | ${String(snap.totalAuth).padStart(5)} | ${String(snap.totalShadow).padStart(6)} |  ${mb.padStart(6)}%  | ${mp.padStart(5)}ms | ${inF.padStart(8)} | ${String(snap.ttlPrunes).padStart(9)} | ${String(ud).padStart(8)}`);
  }

  console.log('\nClosing connections...');
  for (const [, conns] of connections) for (const ws of conns) ws.close();
  await sleep(1000);
  process.exit(0);
}

main().catch(e => { console.error('FAIL:', e); process.exit(1); });
