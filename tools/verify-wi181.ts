/**
 * WI-181 verification script.
 *
 * Runs the four scenarios from docs/mesh-architect/work-items/018-client-reassign.md
 * against the live UNO Q cluster at https://192.168.0.208:7871.
 *
 * Scenarios:
 *   A: Crash the server the client is attached to; client migrates to a peer.
 *   B: Stop all sim-servers; client keeps retrying; restart one; client reconnects.
 *   C: Stop the coordinator; client's WS still up; restart coordinator; then
 *      force the client's WS to drop and verify fallback to last-known URL
 *      then recovery via /assign.
 *   D: Fresh page load with healthy cluster — regression check.
 *
 * The script is intentionally non-destructive to cluster state: it restores
 * all services at the end of each scenario so later scenarios have a clean
 * starting point.
 *
 * Usage:
 *   npx tsx verify-wi181.ts
 */

import { chromium, type Browser, type Page, type ConsoleMessage } from 'playwright';
import { execSync } from 'child_process';

const CLIENT_URL = 'https://192.168.0.208:7871';

const BOARDS: Record<string, { ssh: string; ip: string; port: number }> = {
  S1: { ssh: 'unoq', ip: '192.168.0.15', port: 7878 },
  S2: { ssh: 'unoq2', ip: '192.168.0.173', port: 7879 },
  S3: { ssh: 'unoq3', ip: '192.168.0.86', port: 7880 },
};

const COORD = { ssh: 'zzttpi', ip: '192.168.0.208', port: 7871 };

function sh(cmd: string): string {
  try {
    return execSync(cmd, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  } catch (e: any) {
    return `ERR ${e.status}: ${e.stderr?.toString() ?? ''}`;
  }
}

function wait(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function startSim(boardKey: string): string {
  const b = BOARDS[boardKey];
  const id = boardKey.slice(1); // "1", "2", "3"
  // Start via ssh — mirrors start-unoq.sh's nohup launch. Use setsid +
  // explicit stdin redirect so the process fully detaches from the SSH
  // session (otherwise it can die when the outer ssh exits).
  const cmd = `ssh ${b.ssh} "setsid bash -c 'nohup ~/mesh/sim-server --server-id ${id} --port ${b.port} --udp-port ${9100 + parseInt(id, 10)} --metrics-port ${8100 + parseInt(id, 10)} --coordinator 192.168.0.208:7870 --advertise-host ${b.ip} --initial-npcs 15 --tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt --tls-key /home/arduino/mesh/certs/mesh-coordinator.key </dev/null > ~/mesh/sim${id}.log 2>&1 &'; sleep 0.3; pgrep -f /home/arduino/mesh/sim-server | head -1"`;
  return sh(cmd);
}

function killSim(boardKey: string): string {
  const b = BOARDS[boardKey];
  return sh(`ssh ${b.ssh} 'pkill -9 -f /home/arduino/mesh/sim-server'`);
}

function killAllSims(): string {
  const results: string[] = [];
  for (const k of Object.keys(BOARDS)) {
    results.push(`${k}: ${killSim(k)}`);
  }
  return results.join(' | ');
}

function startCoord(): string {
  return sh(`ssh ${COORD.ssh} 'systemctl --user start mesh-coordinator'`);
}

function stopCoord(): string {
  return sh(`ssh ${COORD.ssh} 'systemctl --user stop mesh-coordinator'`);
}

async function waitForCluster(expectedServers: number, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const json = sh(`curl -sk --max-time 3 https://${COORD.ip}:${COORD.port}/servers`);
      const parsed = JSON.parse(json);
      if (parsed.servers?.length === expectedServers) return true;
    } catch {
      // keep trying
    }
    await wait(500);
  }
  return false;
}

interface CapturedLogs {
  all: string[];
  assignFresh: string[];
  assignSame: string[];
  assignUnavailable: string[];
  serverDirected: string[];
  disconnects: string[];
  connects: string[];
  initialAssign: string[];
}

function makeCapturedLogs(): CapturedLogs {
  return {
    all: [],
    assignFresh: [],
    assignSame: [],
    assignUnavailable: [],
    serverDirected: [],
    disconnects: [],
    connects: [],
    initialAssign: [],
  };
}

function wireConsole(page: Page, logs: CapturedLogs): void {
  page.on('console', (msg: ConsoleMessage) => {
    const text = msg.text();
    const line = `[${msg.type()}] ${text}`;
    logs.all.push(line);
    if (text.includes('/assign returned new URL')) logs.assignFresh.push(text);
    else if (text.includes('/assign returned same URL')) logs.assignSame.push(text);
    else if (text.includes('/assign unavailable')) logs.assignUnavailable.push(text);
    else if (text.includes('server-directed URL')) logs.serverDirected.push(text);
    else if (text.includes('Disconnected from server')) logs.disconnects.push(text);
    else if (text.includes('Connected to server')) logs.connects.push(text);
    else if (text.includes('Assigned to server:')) logs.initialAssign.push(text);
  });

  page.on('pageerror', (err) => {
    logs.all.push(`[pageerror] ${err.message}`);
  });
}

async function getEntityCount(page: Page): Promise<number> {
  // __diag is exposed by NetworkClient.connect() via initDiag().
  // Latest perFrame record carries the WorldView entity count at that tick.
  return await page.evaluate(() => {
    const diag = (window as any).__diag;
    if (!diag?.perFrame?.length) return 0;
    return diag.perFrame[diag.perFrame.length - 1].entities ?? 0;
  });
}

async function getFrameCount(page: Page): Promise<number> {
  return await page.evaluate(() => {
    const diag = (window as any).__diag;
    return diag?.frames ?? 0;
  });
}

async function scenarioA(browser: Browser): Promise<{ pass: boolean; notes: string[]; logs: CapturedLogs }> {
  const notes: string[] = [];
  const logs = makeCapturedLogs();

  // Ensure all 3 servers are live
  const ok = await waitForCluster(3, 15000);
  if (!ok) {
    notes.push('precondition fail: cluster not at 3 servers');
    return { pass: false, notes, logs };
  }

  const page = await browser.newPage({ ignoreHTTPSErrors: true });
  wireConsole(page, logs);

  console.log('[A] Opening client...');
  await page.goto(CLIENT_URL, { waitUntil: 'domcontentloaded', timeout: 15000 });

  // Wait for initial connection + first frames
  await wait(4000);

  // Get which server the client is connected to
  const initialLog = logs.initialAssign[0] ?? '';
  const initialUrlMatch = initialLog.match(/wss?:\/\/[0-9.]+:(\d+)/);
  if (!initialUrlMatch) {
    notes.push(`could not determine initial server from logs. initialAssign=${JSON.stringify(logs.initialAssign)}`);
    await page.close();
    return { pass: false, notes, logs };
  }
  const initialPort = parseInt(initialUrlMatch[1], 10);
  const initialBoard =
    initialPort === BOARDS.S1.port ? 'S1' : initialPort === BOARDS.S2.port ? 'S2' : 'S3';
  notes.push(`client initially connected to ${initialBoard} (port ${initialPort})`);

  const countBefore = await getEntityCount(page);
  notes.push(`entity count before kill: ${countBefore}`);

  // Kill that server
  console.log(`[A] Killing ${initialBoard}...`);
  const killOut = killSim(initialBoard);
  notes.push(`kill output: ${killOut}`);

  // Wait for reconnection — max 20s (exponential backoff can push to ~10s delay, + /assign time)
  const tReconnectStart = Date.now();
  let reconnectedToNew = false;
  while (Date.now() - tReconnectStart < 25000) {
    await wait(500);
    if (logs.assignFresh.length > 0 && logs.connects.length >= 2) {
      reconnectedToNew = true;
      break;
    }
  }

  const reconnectTimeMs = Date.now() - tReconnectStart;
  notes.push(`reconnect delay: ${reconnectTimeMs}ms`);
  notes.push(`disconnects: ${logs.disconnects.length}, connects: ${logs.connects.length}`);
  notes.push(`/assign returned new URL: ${logs.assignFresh.length} time(s)`);
  notes.push(`/assign returned same URL: ${logs.assignSame.length} time(s)`);
  notes.push(`/assign unavailable: ${logs.assignUnavailable.length} time(s)`);

  if (reconnectedToNew) {
    notes.push(`SUCCESS: ${logs.assignFresh[0]}`);
  }

  // Additional wait for frames to flow in so we can confirm live data
  await wait(3000);
  const countAfter = await getEntityCount(page);
  notes.push(`entity count after reconnect: ${countAfter}`);

  await page.close();

  // Restart the killed server for scenario chaining
  console.log(`[A] Restarting ${initialBoard}...`);
  startSim(initialBoard);
  await waitForCluster(3, 15000);

  const pass = reconnectedToNew && countAfter > 0;
  return { pass, notes, logs };
}

async function scenarioB(browser: Browser): Promise<{ pass: boolean; notes: string[]; logs: CapturedLogs }> {
  const notes: string[] = [];
  const logs = makeCapturedLogs();

  // Ensure all 3 alive
  const ok = await waitForCluster(3, 15000);
  if (!ok) {
    notes.push('precondition fail: cluster not at 3 servers');
    return { pass: false, notes, logs };
  }

  const page = await browser.newPage({ ignoreHTTPSErrors: true });
  wireConsole(page, logs);

  console.log('[B] Opening client...');
  await page.goto(CLIENT_URL, { waitUntil: 'domcontentloaded', timeout: 15000 });
  await wait(4000);

  const countBefore = await getEntityCount(page);
  notes.push(`entity count (healthy): ${countBefore}`);

  // Kill all sim-servers
  console.log('[B] Killing ALL sim-servers...');
  killAllSims();

  // Wait until client logs the "assign unavailable" fallback at least once.
  // Coordinator returns 503 when no servers are registered.
  const tStart = Date.now();
  let sawUnavailable = false;
  while (Date.now() - tStart < 25000) {
    await wait(500);
    if (logs.assignUnavailable.length > 0) {
      sawUnavailable = true;
      break;
    }
  }
  notes.push(`saw /assign unavailable: ${sawUnavailable} (count=${logs.assignUnavailable.length})`);

  // Restart just S1
  console.log('[B] Restarting S1...');
  startSim('S1');
  const clusterOk = await waitForCluster(1, 15000);
  notes.push(`cluster back to 1 server: ${clusterOk}`);

  // Wait for client to pick it up
  const tRecoverStart = Date.now();
  let recovered = false;
  while (Date.now() - tRecoverStart < 25000) {
    await wait(500);
    if (logs.assignFresh.length > 0 || logs.connects.length >= 2) {
      recovered = true;
      break;
    }
  }
  const recoveryMs = Date.now() - tRecoverStart;
  notes.push(`recovered after restart in ${recoveryMs}ms`);
  notes.push(`assignFresh=${logs.assignFresh.length} assignSame=${logs.assignSame.length} connects=${logs.connects.length}`);

  await wait(3000);
  const countAfter = await getEntityCount(page);
  notes.push(`entity count after recovery: ${countAfter}`);

  await page.close();

  // Restart S2 and S3 for scenario chaining
  console.log('[B] Restarting S2 and S3 to restore cluster...');
  startSim('S2');
  startSim('S3');
  await waitForCluster(3, 15000);

  const pass = sawUnavailable && recovered && countAfter > 0;
  return { pass, notes, logs };
}

async function scenarioC(browser: Browser): Promise<{ pass: boolean; notes: string[]; logs: CapturedLogs }> {
  const notes: string[] = [];
  const logs = makeCapturedLogs();

  const ok = await waitForCluster(3, 15000);
  if (!ok) {
    notes.push('precondition fail: cluster not at 3 servers');
    return { pass: false, notes, logs };
  }

  const page = await browser.newPage({ ignoreHTTPSErrors: true });
  wireConsole(page, logs);

  console.log('[C] Opening client (coordinator healthy)...');
  await page.goto(CLIENT_URL, { waitUntil: 'domcontentloaded', timeout: 15000 });
  await wait(4000);

  const countBeforeCoordStop = await getEntityCount(page);
  notes.push(`entity count before coord stop: ${countBeforeCoordStop}`);

  // Stop the coordinator. Sim-servers stay up; client's WS to sim-server stays up.
  console.log('[C] Stopping coordinator...');
  stopCoord();
  await wait(3000);

  // The WS should still be alive because only the coord went down. Verify by checking frames still flow.
  const countWhileCoordDown = await getEntityCount(page);
  notes.push(`entity count while coord down: ${countWhileCoordDown} (should still be > 0 — WS to sim-server is independent)`);

  // Now force the sim-server's client WS connection to drop so scheduleReconnect fires.
  // Easiest way: kill the sim-server the client is on. But then /assign can't reach
  // the coordinator — so the fallback should kick in.
  // We need to determine which server the client is on.
  const initialLog = logs.initialAssign[0] ?? '';
  const initialPortMatch = initialLog.match(/wss?:\/\/[0-9.]+:(\d+)/);
  if (!initialPortMatch) {
    notes.push(`could not determine initial port. initialAssign=${JSON.stringify(logs.initialAssign)}`);
    startCoord();
    await page.close();
    return { pass: false, notes, logs };
  }
  const initialPort = parseInt(initialPortMatch[1], 10);
  const boardKey = initialPort === BOARDS.S1.port ? 'S1' : initialPort === BOARDS.S2.port ? 'S2' : 'S3';
  notes.push(`killing ${boardKey} (${initialPort}) to force WS close while coord is down`);
  killSim(boardKey);

  // Expect: WS closes -> scheduleReconnect -> /assign fails (coord down) -> fallback to last-known URL (this same dead board)
  // -> WebSocket construction fails again -> next backoff retry.
  const tFallbackStart = Date.now();
  let sawFallback = false;
  while (Date.now() - tFallbackStart < 15000) {
    await wait(500);
    if (logs.assignUnavailable.length > 0) {
      sawFallback = true;
      break;
    }
  }
  notes.push(`saw /assign unavailable fallback: ${sawFallback} (count=${logs.assignUnavailable.length})`);

  // Bring coord back
  console.log('[C] Restarting coordinator...');
  startCoord();
  // coord takes ~1-2s to start and sim-servers to re-register with it.
  // But we killed one sim-server — restart it too.
  startSim(boardKey);
  await waitForCluster(3, 20000);

  // Client's next backoff cycle should now succeed via /assign OR via
  // fallback-to-same-URL. Recovery = new "Connected to server" event after
  // the stop. Track starting state so we detect the NEW connect.
  const connectsAtRecovery = logs.connects.length;
  const tRecoverStart = Date.now();
  let recovered = false;
  while (Date.now() - tRecoverStart < 25000) {
    await wait(500);
    if (logs.connects.length > connectsAtRecovery) {
      recovered = true;
      break;
    }
  }
  const recoveryMs = Date.now() - tRecoverStart;
  notes.push(`recovered after coord+sim restart in ${recoveryMs}ms`);
  notes.push(`assignFresh=${logs.assignFresh.length} assignSame=${logs.assignSame.length} assignUnavail=${logs.assignUnavailable.length} connects=${logs.connects.length}`);

  await wait(3000);
  const countAfter = await getEntityCount(page);
  notes.push(`entity count after recovery: ${countAfter}`);

  await page.close();

  const pass = sawFallback && recovered && countAfter > 0;
  return { pass, notes, logs };
}

async function scenarioD(browser: Browser): Promise<{ pass: boolean; notes: string[]; logs: CapturedLogs }> {
  const notes: string[] = [];
  const logs = makeCapturedLogs();

  const ok = await waitForCluster(3, 15000);
  if (!ok) {
    notes.push('precondition fail: cluster not at 3 servers');
    return { pass: false, notes, logs };
  }

  const page = await browser.newPage({ ignoreHTTPSErrors: true });
  wireConsole(page, logs);

  console.log('[D] Opening fresh client...');
  const t0 = Date.now();
  await page.goto(CLIENT_URL, { waitUntil: 'domcontentloaded', timeout: 15000 });
  await wait(3000);
  const renderTime = Date.now() - t0;

  const count = await getEntityCount(page);
  notes.push(`entity count after ${renderTime}ms: ${count}`);
  notes.push(`initial assign: ${logs.initialAssign[0] ?? '(none captured)'}`);
  notes.push(`connects: ${logs.connects.length}, disconnects: ${logs.disconnects.length}`);
  notes.push(`first 5 log lines:`);
  for (const l of logs.all.slice(0, 5)) notes.push(`  ${l}`);

  await page.close();

  const pass = count > 0 && logs.initialAssign.length === 1 && logs.disconnects.length === 0;
  return { pass, notes, logs };
}

async function main() {
  console.log(`=== WI-181 verification ===`);
  console.log(`Client: ${CLIENT_URL}`);
  console.log(`Boards: ${Object.keys(BOARDS).join(', ')}`);
  console.log();

  // Sanity check initial cluster state
  console.log('[init] Verifying initial cluster state...');
  const ready = await waitForCluster(3, 15000);
  if (!ready) {
    console.error('FAIL: cluster is not at 3 servers at script start. Run ./start-unoq.sh first.');
    process.exit(2);
  }

  const browser = await chromium.launch({ headless: true });

  const results: { name: string; pass: boolean; notes: string[]; logs: CapturedLogs }[] = [];

  console.log('\n========== Scenario D (regression baseline) ==========\n');
  const d = await scenarioD(browser);
  results.push({ name: 'D: regression baseline', ...d });

  console.log('\n========== Scenario A (crashed-server migration) ==========\n');
  const a = await scenarioA(browser);
  results.push({ name: 'A: crashed-server migration', ...a });

  console.log('\n========== Scenario B (all dead, one returns) ==========\n');
  const b = await scenarioB(browser);
  results.push({ name: 'B: all dead, one returns', ...b });

  console.log('\n========== Scenario C (coordinator blip + fallback) ==========\n');
  const c = await scenarioC(browser);
  results.push({ name: 'C: coordinator blip + fallback', ...c });

  await browser.close();

  console.log('\n\n========================================');
  console.log('              SUMMARY');
  console.log('========================================\n');
  for (const r of results) {
    console.log(`${r.pass ? 'PASS' : 'FAIL'}  ${r.name}`);
    for (const n of r.notes) console.log(`       ${n}`);
    console.log();
  }

  // Print selected console lines per scenario for evidence.
  console.log('\n========================================');
  console.log('       CONSOLE LOG EVIDENCE');
  console.log('========================================\n');
  for (const r of results) {
    console.log(`--- ${r.name} ---`);
    const evidence = [
      ...r.logs.initialAssign.map((l) => `  [initial] ${l}`),
      ...r.logs.disconnects.map((l) => `  [disconnect] ${l}`),
      ...r.logs.assignFresh.map((l) => `  [assign-fresh] ${l}`),
      ...r.logs.assignSame.map((l) => `  [assign-same] ${l}`),
      ...r.logs.assignUnavailable.map((l) => `  [assign-unavail] ${l}`),
      ...r.logs.serverDirected.map((l) => `  [server-directed] ${l}`),
      ...r.logs.connects.map((l) => `  [connect] ${l}`),
    ];
    for (const line of evidence) console.log(line);
    console.log();
  }

  const allPass = results.every((r) => r.pass);
  process.exit(allPass ? 0 : 1);
}

main().catch((e) => {
  console.error('FATAL:', e);
  process.exit(3);
});
