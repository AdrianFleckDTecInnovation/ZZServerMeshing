// Churn test at steady-state load: kills S3 via SSH, measures recovery.
// Assumes cluster is already loaded (~5k entities) and stable.
//
// Measures:
//   - Time for client connected to S3 to migrate (re-assign, reconnect)
//   - Time for S3 restart to re-register with coordinator
//   - Entity count stability across cluster (no 15 initial NPC balloon on restart)
//   - Time for peer shadows to reconverge on S1/S2 (staleness_ticks returns to low)
//   - Re-check peer packets_received continuity (design 016 Batch 5 WI-212)
//
// Usage:  COORDINATOR_URL=https://192.168.0.208:7871 npx tsx churn-5k.ts

import WebSocket from 'ws';
import { spawn } from 'child_process';

process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';

const coordUrl = process.env.COORDINATOR_URL || 'https://192.168.0.208:7871';
const TARGET_SID = 3;
const SSH_HOST = 'unoq3';

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
  session_id: number;
  tick: number;
  entity_count: number;
  shadow_count: number;
  udp_repl_drops: number;
  udp_ctrl_drops: number;
  peer_health: Record<string, { latest_tick: number; staleness_ticks: number; packets_received: number; session_id: number; session_changed_at_tick: number }>;
}

async function getServers(): Promise<Server[]> {
  const resp = await fetch(`${coordUrl}/servers`);
  const data = await resp.json() as { servers: Server[] };
  return data.servers;
}
async function getAssign(): Promise<string> {
  const resp = await fetch(`${coordUrl}/assign`);
  const data = await resp.json() as { ws_url: string };
  return data.ws_url;
}
async function getMetrics(host: string, port: number): Promise<Metrics | null> {
  try {
    const resp = await fetch(`http://${host}:${port}/metrics`);
    if (!resp.ok) return null;
    return await resp.json();
  } catch { return null; }
}
function sleep(ms: number) { return new Promise(r => setTimeout(r, ms)); }
function now() { return Date.now(); }

const T0 = now();
function tlog(msg: string) {
  console.log(`[+${((now() - T0) / 1000).toFixed(2)}s] ${msg}`);
}

async function ssh(cmd: string): Promise<string> {
  return new Promise((res, rej) => {
    const p = spawn('ssh', [SSH_HOST, cmd], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = ''; let err = '';
    p.stdout.on('data', d => out += d);
    p.stderr.on('data', d => err += d);
    p.on('close', code => {
      if (code === 0) res(out);
      else rej(new Error(`ssh exit ${code}: ${err}`));
    });
  });
}

async function connectTo(url: string): Promise<WebSocket> {
  return new Promise((res, rej) => {
    const ws = new WebSocket(url, { rejectUnauthorized: false });
    const to = setTimeout(() => rej(new Error('connect timeout')), 8000);
    ws.on('open', () => { clearTimeout(to); res(ws); });
    ws.on('error', e => { clearTimeout(to); rej(e); });
  });
}

async function main() {
  // Initial snapshot
  const s0 = await getServers();
  const tot0 = s0.reduce((s, x) => s + x.entity_count, 0);
  tlog(`Initial: ${s0.length} servers, total auth=${tot0}`);
  for (const s of s0) tlog(`  S${s.server_id}: auth=${s.entity_count} host=${s.host}`);
  const targetSrv = s0.find(s => s.server_id === TARGET_SID);
  if (!targetSrv) { tlog(`S${TARGET_SID} not in cluster; aborting`); process.exit(1); }
  const survivors = s0.filter(s => s.server_id !== TARGET_SID);

  // Get an /assign URL — expected to bounce between S1/S2/S3
  const assignedUrl = await getAssign();
  tlog(`/assign returned ${assignedUrl}`);

  // Open a simulated player client. If it lands on S3, great; if not, we still
  // measure the mesh migration behavior via /servers + /assign refetches.
  // For this test we open a client directly to S3 to ensure migration path.
  const s3Url = `wss://${targetSrv.host}:${targetSrv.ws_port}`;
  tlog(`Opening client to ${s3Url} (S3 directly)`);
  let clientWs: WebSocket | null = null;
  try { clientWs = await connectTo(s3Url); tlog(`  client connected to S3`); }
  catch (e: any) { tlog(`  connect failed: ${e.message}`); }

  // Track: when client disconnects, re-/assign, measure migration time.
  let disconnectedAt: number | null = null;
  let migratedAt: number | null = null;
  let migratedTo: string | null = null;
  if (clientWs) {
    clientWs.on('close', async () => {
      disconnectedAt = now();
      tlog(`CLIENT DISCONNECTED from S3`);
      // Loop /assign until we get a non-S3 URL
      for (let i = 0; i < 50; i++) {
        try {
          const newUrl = await getAssign();
          if (!newUrl.includes(`:${targetSrv.ws_port}`)) {
            tlog(`CLIENT /assign returned fresh ${newUrl} (no S3)`);
            try {
              const w = await connectTo(newUrl);
              migratedAt = now();
              migratedTo = newUrl;
              tlog(`CLIENT MIGRATED to ${newUrl} in ${((migratedAt - disconnectedAt) / 1000).toFixed(3)}s`);
              w.close();
              return;
            } catch (e: any) {
              tlog(`  connect retry failed: ${e.message}`);
            }
          }
        } catch (e: any) {
          tlog(`  /assign retry err: ${e.message}`);
        }
        await sleep(200);
      }
    });
  }

  // Baseline peer_health snapshot from S1 to track continuity
  const s1Metrics0 = await getMetrics(survivors[0].host, survivors[0].metrics_port);
  const s2Metrics0 = await getMetrics(survivors[1].host, survivors[1].metrics_port);
  const s3Metrics0 = await getMetrics(targetSrv.host, targetSrv.metrics_port);
  tlog(`BEFORE KILL:`);
  tlog(`  S${survivors[0].server_id} peer_health[${TARGET_SID}]: pkts_rx=${s1Metrics0?.peer_health[String(TARGET_SID)]?.packets_received} staleness=${s1Metrics0?.peer_health[String(TARGET_SID)]?.staleness_ticks} session=${s1Metrics0?.peer_health[String(TARGET_SID)]?.session_id}`);
  tlog(`  S${survivors[1].server_id} peer_health[${TARGET_SID}]: pkts_rx=${s2Metrics0?.peer_health[String(TARGET_SID)]?.packets_received} staleness=${s2Metrics0?.peer_health[String(TARGET_SID)]?.staleness_ticks} session=${s2Metrics0?.peer_health[String(TARGET_SID)]?.session_id}`);
  tlog(`  S3 session_id=${s3Metrics0?.session_id} entity_count=${s3Metrics0?.entity_count}`);
  const oldS3Session = s3Metrics0?.session_id;
  const preKillS1PktsRx = s1Metrics0?.peer_health[String(TARGET_SID)]?.packets_received ?? 0;
  const preKillS2PktsRx = s2Metrics0?.peer_health[String(TARGET_SID)]?.packets_received ?? 0;

  // KILL
  const killAt = now();
  tlog(`--- KILLING S3 via SSH pkill ---`);
  try {
    await ssh('pkill -KILL -f "sim-server --server-id 3"');
    tlog(`  pkill sent`);
  } catch (e: any) {
    tlog(`  pkill err (may be ok): ${e.message}`);
  }

  // Observe survivors: how long until coordinator drops S3 from /servers
  let droppedAt: number | null = null;
  for (let i = 0; i < 80; i++) {
    const srv = await getServers();
    const hasS3 = srv.some(s => s.server_id === TARGET_SID);
    if (!hasS3 && droppedAt === null) {
      droppedAt = now();
      tlog(`COORDINATOR DROPPED S3 from /servers at +${((droppedAt - killAt) / 1000).toFixed(2)}s post-kill`);
      break;
    }
    await sleep(500);
  }
  if (droppedAt === null) tlog(`  (coordinator did not drop S3 within 40s)`);

  // Get entity counts on survivors post-kill (and absence of ballooning)
  await sleep(1000);
  const s1Post = await getMetrics(survivors[0].host, survivors[0].metrics_port);
  const s2Post = await getMetrics(survivors[1].host, survivors[1].metrics_port);
  tlog(`POST-KILL survivors:`);
  tlog(`  S${survivors[0].server_id}: auth=${s1Post?.entity_count} shadow=${s1Post?.shadow_count}`);
  tlog(`  S${survivors[1].server_id}: auth=${s2Post?.entity_count} shadow=${s2Post?.shadow_count}`);

  // Restart S3 via SSH (nohup)
  tlog(`--- RESTARTING S3 ---`);
  const restartAt = now();
  try {
    // Fully detach: redirect stdin+stdout+stderr + disown. Using `setsid` so
    // the new process leaves the ssh TTY session, otherwise ssh blocks waiting
    // for the child's stdout to close.
    const cmd = `cd /home/arduino/mesh && setsid bash -c './sim-server --server-id 3 --port 7880 --udp-port 9103 --metrics-port 8103 --coordinator 192.168.0.208:7870 --advertise-host 192.168.0.86 --initial-npcs 15 --coordinator-tls --tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key >> sim3.log 2>&1 < /dev/null &' && sleep 0.5`;
    await ssh(cmd);
    tlog(`  restart command issued`);
  } catch (e: any) {
    tlog(`  restart failed: ${e.message}`);
  }

  // Wait for /servers to include S3
  let registeredAt: number | null = null;
  for (let i = 0; i < 60; i++) {
    const srv = await getServers();
    const s3 = srv.find(s => s.server_id === TARGET_SID);
    if (s3) {
      registeredAt = now();
      tlog(`COORDINATOR ACCEPTED S3 RE-REGISTRATION at +${((registeredAt - restartAt) / 1000).toFixed(2)}s post-restart (${((registeredAt - killAt) / 1000).toFixed(2)}s post-kill). auth=${s3.entity_count}`);
      break;
    }
    await sleep(500);
  }
  if (registeredAt === null) tlog(`  (S3 did not re-register within 30s)`);

  // Sample metrics to observe S3 entity count and peer_health reconvergence
  tlog(`--- RECONVERGENCE WINDOW (60s) ---`);
  const reconStart = now();
  let s3SettledAt: number | null = null;
  let peerReconvergedAt: number | null = null;
  let maxS3Auth = 0;
  for (let i = 0; i < 30; i++) {
    await sleep(2000);
    const s3m = await getMetrics(targetSrv.host, targetSrv.metrics_port);
    const s1m = await getMetrics(survivors[0].host, survivors[0].metrics_port);
    const s2m = await getMetrics(survivors[1].host, survivors[1].metrics_port);
    const elapsed = ((now() - reconStart) / 1000).toFixed(1);
    const s3Auth = s3m?.entity_count ?? 0;
    if (s3Auth > maxS3Auth) maxS3Auth = s3Auth;

    const s1Stale = s1m?.peer_health[String(TARGET_SID)]?.staleness_ticks ?? 99999;
    const s2Stale = s2m?.peer_health[String(TARGET_SID)]?.staleness_ticks ?? 99999;
    const s1Rx = s1m?.peer_health[String(TARGET_SID)]?.packets_received ?? 0;
    const s2Rx = s2m?.peer_health[String(TARGET_SID)]?.packets_received ?? 0;
    const s1Sess = s1m?.peer_health[String(TARGET_SID)]?.session_id;
    const s2Sess = s2m?.peer_health[String(TARGET_SID)]?.session_id;

    tlog(`  [+${elapsed}s] S3 auth=${s3Auth} shadow=${s3m?.shadow_count} session=${s3m?.session_id}  |  S${survivors[0].server_id}[peer3]: stale=${s1Stale} rx=${s1Rx}  |  S${survivors[1].server_id}[peer3]: stale=${s2Stale} rx=${s2Rx}`);

    if (peerReconvergedAt === null && s1Stale < 50 && s2Stale < 50) {
      peerReconvergedAt = now();
      tlog(`PEER SHADOWS RECONVERGED at +${((peerReconvergedAt - restartAt) / 1000).toFixed(2)}s post-restart (staleness on both survivors < 50 ticks)`);
    }
  }
  tlog(`S3 MAX auth during reconvergence: ${maxS3Auth}`);

  // Final entity state
  const final = await getServers();
  const totFinal = final.reduce((s, x) => s + x.entity_count, 0);
  tlog(`FINAL: total=${totFinal} vs initial ${tot0} (delta ${totFinal - tot0})`);
  for (const s of final) tlog(`  S${s.server_id}: auth=${s.entity_count}`);

  // Close clients
  if (clientWs && clientWs.readyState === WebSocket.OPEN) clientWs.close();
  await sleep(500);

  // Summary
  console.log('\n=== SUMMARY ===');
  console.log(`Pre-kill total auth: ${tot0}`);
  console.log(`Post-reconverge total auth: ${totFinal}`);
  console.log(`Coordinator drop-S3 latency: ${droppedAt !== null ? ((droppedAt - killAt) / 1000).toFixed(2) + 's' : 'N/A'}`);
  console.log(`S3 re-registration latency (from restart cmd): ${registeredAt !== null ? ((registeredAt - restartAt) / 1000).toFixed(2) + 's' : 'N/A'}`);
  console.log(`Client migration latency: ${(disconnectedAt !== null && migratedAt !== null) ? ((migratedAt - disconnectedAt) / 1000).toFixed(3) + 's' : 'N/A'}`);
  console.log(`Peer shadow reconvergence latency (from restart): ${peerReconvergedAt !== null ? ((peerReconvergedAt - restartAt) / 1000).toFixed(2) + 's' : 'N/A'}`);
  console.log(`S3 initial-NPC ballooning check: S3 max auth during reconvergence = ${maxS3Auth} (should rebalance from 0, NOT start at 15+)`);
}

main().catch(e => { console.error(e); process.exit(1); });
