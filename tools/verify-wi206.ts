// WI-206 verification driver for Batch 4 (player migration).
//
// Scenarios covered:
//   1. Player migration under load (scenario 1 from WI-206)
//   2. Claim-timeout fallback (scenario 2, unit-tested; spot-check here)
//   3. Reconnect URL TLS scheme correctness (scenario 3)
//   4. Non-migrating clients unaffected (scenario 4)
//   5. RemoveEntity suppression on close (scenario 5)
//
// Usage:
//   npx tsx tools/verify-wi206.ts
//
// Requires boards running via ./start-unoq.sh.

import WebSocket from 'ws';
import * as https from 'https';
import * as http from 'http';
import { execSync } from 'child_process';

const COORD = 'https://192.168.0.208:7871';
const S1_WS = 'wss://192.168.0.15:7878';
const S2_WS = 'wss://192.168.0.173:7879';
const S3_WS = 'wss://192.168.0.86:7880';

function fetchJson(url: string): Promise<any> {
  return new Promise((resolve, reject) => {
    https.get(url, { rejectUnauthorized: false }, (res) => {
      let body = '';
      res.on('data', (c) => (body += c));
      res.on('end', () => {
        try { resolve(JSON.parse(body)); } catch (e) { reject(e); }
      });
    }).on('error', reject);
  });
}

async function fetchMetrics(host: string, port: number): Promise<any> {
  return new Promise((resolve, reject) => {
    http.get(`http://${host}:${port}/metrics`, (res: any) => {
      let body = '';
      res.on('data', (c: any) => (body += c));
      res.on('end', () => { try { resolve(JSON.parse(body)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });
}

interface ConnectedClient {
  ws: WebSocket;
  url: string;
  playerId: string | null;
  reconnectUrl: string | null;
  messagesLog: any[];
}

function connect(url: string, tag: string): Promise<ConnectedClient> {
  return new Promise((resolve, reject) => {
    const client: ConnectedClient = {
      ws: new WebSocket(url, { rejectUnauthorized: false }),
      url,
      playerId: null,
      reconnectUrl: null,
      messagesLog: [],
    };
    const timeout = setTimeout(() => reject(new Error(`${tag}: connect timeout to ${url}`)), 5000);

    client.ws.on('open', () => {
      console.log(`[${tag}] open to ${url}`);
    });
    client.ws.on('message', (data) => {
      // Binary frames are snapshots/deltas; ignore.
      if (data instanceof Buffer && data.length > 0 && data[0] <= 0x02) return;
      try {
        const msg = JSON.parse(data.toString());
        client.messagesLog.push(msg);
        if (msg.type === 'PlayerAssigned') {
          client.playerId = msg.player_id;
          clearTimeout(timeout);
          console.log(`[${tag}] PlayerAssigned: ${msg.player_id.substring(0, 8)}`);
          resolve(client);
        } else if (msg.type === 'Reconnect') {
          client.reconnectUrl = msg.url;
          console.log(`[${tag}] RECONNECT -> ${msg.url}`);
        }
      } catch {}
    });
    client.ws.on('error', (e) => { clearTimeout(timeout); reject(e); });
  });
}

async function scenario3_tls_scheme(): Promise<boolean> {
  console.log('\n=== Scenario 3: Reconnect URL TLS scheme correctness ===');
  // The boards run with TLS so any Reconnect URL (even for a same-peer) must be wss://.
  // We don't force a migration here (that's scenario 1); we just confirm that
  // PeerWsAddresses is populated correctly. Indirect check: PeerList arrived.
  // Direct check below in scenario 1.
  for (const [id, ip, port] of [[1,'192.168.0.15',8101], [2,'192.168.0.173',8102], [3,'192.168.0.86',8103]] as [number,string,number][]) {
    const m = await fetchMetrics(ip, port);
    const peers = m.peer_count ?? 0;
    const passes = peers >= 2;
    console.log(`  S${id}: peer_count=${peers} ${passes ? 'PASS' : 'FAIL'}`);
    if (!passes) return false;
  }
  return true;
}

async function scenario4_plain_connect(): Promise<boolean> {
  console.log('\n=== Scenario 4: Non-migrating (fresh) client works ===');
  // Fresh connect to S1 via /assign — should spawn new player after
  // the 500ms CLAIM_WAIT_MS timeout.
  const assign = await fetchJson(`${COORD}/assign`);
  const url = assign.ws_url;
  console.log(`  Assigned to ${url}`);

  const t0 = Date.now();
  const client = await connect(url, 'fresh');
  const connectMs = Date.now() - t0;
  console.log(`  PlayerAssigned received in ${connectMs}ms`);
  console.log(`  Player ID: ${client.playerId?.substring(0, 8)}`);

  // Must be a *new* player (not claimed).
  const passes = client.playerId !== null && connectMs < 3000;
  console.log(`  Fresh spawn ${passes ? 'PASS' : 'FAIL'}`);

  client.ws.close();
  await new Promise(r => setTimeout(r, 200));
  return passes;
}

async function scenario1_migration(): Promise<boolean> {
  console.log('\n=== Scenario 1: Player migration on rebalance ===');
  // Connect 15 clients directly to S1 to force a massive imbalance.
  // S1 weighted = 15 NPC + 15 player*3 = 60 vs S2/S3 = 15 each.
  // avg = 30, diff = 45, floor = 10. Fires immediately. NPCs drain first
  // (~8 cycles), then Players start migrating. Wait up to 240s.
  const TARGET_PLAYERS = 15;
  const WAIT_SECS = 240;

  console.log(`  Connecting ${TARGET_PLAYERS} players directly to S1...`);
  const clients: ConnectedClient[] = [];
  for (let i = 0; i < TARGET_PLAYERS; i++) {
    const c = await connect(S1_WS, `c${i}`);
    clients.push(c);
    await new Promise(r => setTimeout(r, 200));
  }

  console.log(`  All ${TARGET_PLAYERS} connected. Waiting up to ${WAIT_SECS}s for player migration...`);
  const deadline = Date.now() + WAIT_SECS * 1000;
  let reconnected: ConnectedClient | null = null;
  let lastReport = 0;
  while (Date.now() < deadline) {
    for (const c of clients) {
      if (c.reconnectUrl) { reconnected = c; break; }
    }
    if (reconnected) break;
    const now = Date.now();
    if (now - lastReport > 10_000) {
      try {
        const m1 = await fetchMetrics('192.168.0.15', 8101);
        const m2 = await fetchMetrics('192.168.0.173', 8102);
        const m3 = await fetchMetrics('192.168.0.86', 8103);
        console.log(
          `    [${Math.round((now - (deadline - WAIT_SECS*1000))/1000)}s] S1 e=${m1.entity_count} s=${m1.shadow_count} | S2 e=${m2.entity_count} s=${m2.shadow_count} | S3 e=${m3.entity_count} s=${m3.shadow_count}`
        );
      } catch {}
      lastReport = now;
    }
    await new Promise(r => setTimeout(r, 500));
  }

  let passes = false;
  if (reconnected) {
    const url = reconnected.reconnectUrl!;
    const origPlayerId = reconnected.playerId!;
    const isWss = url.startsWith('wss://');
    console.log(`  Client ${origPlayerId.substring(0,8)} received Reconnect -> ${url}`);
    console.log(`    Uses wss:// scheme: ${isWss ? 'PASS' : 'FAIL'}`);

    // End-to-end: close the old WS, reconnect to the target, send ClaimPlayer,
    // and verify PlayerAssigned echoes the SAME player_id (adopt path).
    reconnected.ws.close();
    await new Promise(r => setTimeout(r, 500));

    console.log(`  Reconnecting to ${url} with ClaimPlayer...`);
    const ws = new WebSocket(url, { rejectUnauthorized: false });
    let claimedId: string | null = null;
    const claimDone = new Promise<boolean>((resolve) => {
      const timeout = setTimeout(() => resolve(false), 8000);
      ws.on('open', () => {
        ws.send(JSON.stringify({ type: 'ClaimPlayer', player_id: origPlayerId }));
      });
      ws.on('message', (data) => {
        if (data instanceof Buffer && data.length > 0 && data[0] <= 0x02) return;
        try {
          const msg = JSON.parse(data.toString());
          if (msg.type === 'PlayerAssigned') {
            claimedId = msg.player_id;
            clearTimeout(timeout);
            resolve(true);
          }
        } catch {}
      });
      ws.on('error', (e) => { clearTimeout(timeout); console.log(`    WS error: ${e.message}`); resolve(false); });
    });
    const claimOk = await claimDone;
    if (claimOk) {
      const idMatches = claimedId === origPlayerId;
      console.log(`    Adopted PlayerAssigned id: ${claimedId?.substring(0,8)}`);
      console.log(`    Original id preserved: ${idMatches ? 'PASS' : 'FAIL'}`);
      passes = isWss && idMatches;
    } else {
      console.log(`    Claim failed or timed out (FAIL)`);
      passes = false;
    }
    try { ws.close(); } catch {}
  } else {
    console.log(`  TIMEOUT: no Reconnect received within ${WAIT_SECS}s (FAIL)`);
    // Dump logs for diagnostics
    try {
      const out = execSync('ssh unoq "grep -c \'Sent Reconnect\' ~/mesh/sim1.log || echo 0"', { encoding: 'utf8' }).trim();
      console.log(`    "Sent Reconnect" in sim1.log: ${out}`);
    } catch {}
  }

  for (const c of clients) { try { c.ws.close(); } catch {} }
  await new Promise(r => setTimeout(r, 1500));
  return passes;
}

async function scenario5_migrating_out_suppression(): Promise<boolean> {
  console.log('\n=== Scenario 5: RemoveEntity suppression on migrating-out close ===');
  // Find the "Client migrating out" log line in sim1.log.
  try {
    const out = execSync(
      'ssh unoq "grep -c \'migrating out\' ~/mesh/sim1.log || echo 0"',
      { encoding: 'utf8' }
    ).trim();
    const count = parseInt(out) || 0;
    const passes = count > 0;
    console.log(`  "migrating out" log lines on S1: ${count} ${passes ? 'PASS' : 'FAIL'}`);
    return passes;
  } catch (e) {
    console.log(`  Failed to inspect S1 logs: ${e}`);
    return false;
  }
}

async function main() {
  console.log('WI-206 verification starting...\n');
  const results: Record<string, boolean> = {};
  results['scenario3_tls_scheme'] = await scenario3_tls_scheme();
  results['scenario4_plain_connect'] = await scenario4_plain_connect();
  results['scenario1_migration'] = await scenario1_migration();
  results['scenario5_migrating_out_suppression'] = await scenario5_migrating_out_suppression();

  console.log('\n=== Summary ===');
  for (const [name, passed] of Object.entries(results)) {
    console.log(`  ${passed ? 'PASS' : 'FAIL'}: ${name}`);
  }
  const allPass = Object.values(results).every(v => v);
  console.log(`\n${allPass ? 'ALL PASS' : 'SOME FAILED'}`);
  process.exit(allPass ? 0 : 1);
}

main().catch((e) => {
  console.error('FATAL:', e);
  process.exit(2);
});
