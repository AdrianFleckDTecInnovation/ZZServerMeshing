import WebSocket from 'ws';
import type {
  RunnerConfig,
  TestContext,
  ServerMetrics,
  WorldSnapshot,
  Scenario,
  ScenarioResult,
  SpawnOptions,
} from './types.js';

// Import all scenarios
import { spawnBurst } from './scenarios/spawn-burst.js';
import { steadyScale } from './scenarios/steady-scale.js';
import { crashRecovery } from './scenarios/crash-recovery.js';
import { rebalanceStorm } from './scenarios/rebalance-storm.js';
import { spatialHotspot } from './scenarios/spatial-hotspot.js';
import { inputLatency } from './scenarios/input-latency.js';
import { smoothness } from './scenarios/smoothness.js';
import { meshVerify } from './scenarios/mesh-verify.js';
import { inputSeqEcho } from './scenarios/input-seq-echo.js';
import { peerReplication } from './scenarios/peer-replication.js';
import { forwardedInput } from './scenarios/forwarded-input.js';
import { multiClient } from './scenarios/multi-client.js';
import { entityLifecycle } from './scenarios/entity-lifecycle.js';
import { authorityTransfer } from './scenarios/authority-transfer.js';

const ALL_SCENARIOS: Scenario[] = [
  spawnBurst,
  steadyScale,
  crashRecovery,
  rebalanceStorm,
  spatialHotspot,
  inputLatency,
  smoothness,
  meshVerify,
  inputSeqEcho,
  peerReplication,
  forwardedInput,
  multiClient,
  entityLifecycle,
  authorityTransfer,
];

// ── CLI ──

function parseArgs() {
  const args = process.argv.slice(2);
  const flags = {
    json: false,
    noColor: false,
    serverUrl: 'ws://localhost:7878',
    metricsBasePort: 8101,
    serverCount: 3,
    scenarios: undefined as string[] | undefined,
  };

  const positional: string[] = [];
  for (const arg of args) {
    if (arg === '--json') flags.json = true;
    else if (arg === '--no-color') flags.noColor = true;
    else positional.push(arg);
  }

  if (positional[0]) flags.serverUrl = positional[0];
  if (positional[1]) flags.metricsBasePort = parseInt(positional[1]);
  if (positional[2]) flags.serverCount = parseInt(positional[2]);
  if (positional[3]) flags.scenarios = positional[3].split(',');

  return flags;
}

const cliFlags = parseArgs();
const JSON_OUTPUT = cliFlags.json;
const NO_COLOR = cliFlags.noColor || JSON_OUTPUT;

// Derive coordinator URL from the server URL (same host, port 7871) or env var
const coordinatorUrl = process.env.COORDINATOR_URL ||
  (() => {
    try {
      const u = new URL(cliFlags.serverUrl.replace(/^ws/, 'http'));
      const proto = u.protocol === 'https:' ? 'https' : 'http';
      return `${proto}://${u.hostname}:7871`;
    } catch { return 'http://localhost:7871'; }
  })();

// Build per-server metrics host map from env or coordinator /servers endpoint
// Format: METRICS_HOSTS="1=192.168.0.15,2=192.168.0.173,3=192.168.0.86"
const metricsHosts: Record<number, string> = {};
if (process.env.METRICS_HOSTS) {
  for (const entry of process.env.METRICS_HOSTS.split(',')) {
    const [id, host] = entry.split('=');
    if (id && host) metricsHosts[parseInt(id)] = host;
  }
}

const config: RunnerConfig = {
  serverUrl: cliFlags.serverUrl,
  metricsBasePort: cliFlags.metricsBasePort,
  serverCount: cliFlags.serverCount,
  scenarios: cliFlags.scenarios,
};

// ── Color helpers (no deps) ──

const color = {
  green: (s: string) => NO_COLOR ? s : `\x1b[32m${s}\x1b[0m`,
  yellow: (s: string) => NO_COLOR ? s : `\x1b[33m${s}\x1b[0m`,
  red: (s: string) => NO_COLOR ? s : `\x1b[31m${s}\x1b[0m`,
  cyan: (s: string) => NO_COLOR ? s : `\x1b[36m${s}\x1b[0m`,
  dim: (s: string) => NO_COLOR ? s : `\x1b[2m${s}\x1b[0m`,
  bold: (s: string) => NO_COLOR ? s : `\x1b[1m${s}\x1b[0m`,
};

function statusColor(status: string): string {
  switch (status) {
    case 'pass': return color.green('PASS');
    case 'warn': return color.yellow('WARN');
    case 'fail': return color.red('FAIL');
    default: return status;
  }
}

// ── Metrics fetcher ──

async function fetchMetrics(serverId: number, basePort: number): Promise<ServerMetrics> {
  const port = basePort + serverId - 1; // server 1 → basePort, server 2 → basePort+1, etc.
  const host = metricsHosts[serverId] || 'localhost';
  const url = `http://${host}:${port}/metrics`;
  const res = await fetch(url, { signal: AbortSignal.timeout(2000) });
  if (!res.ok) throw new Error(`Metrics fetch failed: ${res.status}`);
  return res.json() as Promise<ServerMetrics>;
}

// ── Context factory ──

// Binary protocol decoder (matches gateway binary protocol)
const ENTITY_SIZE = 47;
const FRAME_FULL = 0x01;
const FRAME_DELTA = 0x02;
const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push(i.toString(16).padStart(2, '0'));

function uuidFromBytes(buf: Buffer, offset: number): string {
  return (
    HEX[buf[offset]]     + HEX[buf[offset+1]] +
    HEX[buf[offset+2]]   + HEX[buf[offset+3]] + '-' +
    HEX[buf[offset+4]]   + HEX[buf[offset+5]] + '-' +
    HEX[buf[offset+6]]   + HEX[buf[offset+7]] + '-' +
    HEX[buf[offset+8]]   + HEX[buf[offset+9]] + '-' +
    HEX[buf[offset+10]]  + HEX[buf[offset+11]] +
    HEX[buf[offset+12]]  + HEX[buf[offset+13]] +
    HEX[buf[offset+14]]  + HEX[buf[offset+15]]
  );
}

interface BinEntity { id: string; position: [number,number,number]; velocity: [number,number,number]; entity_type: string; authority_server: number; last_input_seq: number; }

function readEntity(data: Buffer, offset: number): BinEntity {
  return {
    id: uuidFromBytes(data, offset),
    position: [data.readFloatLE(offset+16), data.readFloatLE(offset+20), data.readFloatLE(offset+24)],
    velocity: [data.readFloatLE(offset+28), data.readFloatLE(offset+32), data.readFloatLE(offset+36)],
    entity_type: data[offset+40] === 1 ? 'Player' : 'Npc',
    authority_server: data.readUInt16LE(offset+41),
    last_input_seq: data.readUInt32LE(offset+43),
  };
}

// Test runner maintains its own entity state for delta application
const entityMap = new Map<string, BinEntity>();

function decodeBinaryFrame(data: Buffer): WorldSnapshot | null {
  if (data.length < 2) return null;
  const frameType = data[0];

  if (frameType === FRAME_FULL) {
    // [type:1][tick:8][count:4][metrics:14] = 27 byte header
    const tick = data.readUInt32LE(1) + data.readUInt32LE(5) * 0x100000000;
    const entityCount = data.readUInt32LE(9);
    const snapshotsPerSec = data.readUInt32LE(13);
    const bandwidthKBps = data.readUInt32LE(17);
    const rebalanceEvents = data.readUInt32LE(21);
    const serverCount = data.readUInt16LE(25);

    entityMap.clear();
    let offset = 27;
    for (let i = 0; i < entityCount; i++) {
      const e = readEntity(data, offset);
      entityMap.set(e.id, e);
      offset += ENTITY_SIZE;
    }

    return {
      type: 'WorldSnapshot', tick,
      entities: [...entityMap.values()],
      _metrics: { snapshotsPerSec, bandwidthKBps, rebalanceEvents, serverCount },
    };
  }

  if (frameType === FRAME_DELTA) {
    // [type:1][tick:8][updates:4][removals:4][metrics:14] = 31 byte header
    const tick = data.readUInt32LE(1) + data.readUInt32LE(5) * 0x100000000;
    const updateCount = data.readUInt32LE(9);
    const removalCount = data.readUInt32LE(13);
    const snapshotsPerSec = data.readUInt32LE(17);
    const bandwidthKBps = data.readUInt32LE(21);
    const rebalanceEvents = data.readUInt32LE(25);
    const serverCount = data.readUInt16LE(29);

    let offset = 31;
    for (let i = 0; i < updateCount; i++) {
      const e = readEntity(data, offset);
      entityMap.set(e.id, e);
      offset += ENTITY_SIZE;
    }
    for (let i = 0; i < removalCount; i++) {
      const id = uuidFromBytes(data, offset);
      entityMap.delete(id);
      offset += 16;
    }

    return {
      type: 'WorldSnapshot', tick,
      entities: [...entityMap.values()],
      _metrics: { snapshotsPerSec, bandwidthKBps, rebalanceEvents, serverCount },
    };
  }

  return null;
}

function createContext(ws: WebSocket, scenarioName: string, config: RunnerConfig): TestContext {
  let latestSnapshot: WorldSnapshot | null = null;
  let tickResolvers: { target: number; resolve: () => void }[] = [];

  ws.on('message', (data: Buffer | string, isBinary: boolean) => {
    try {
      let snapshot: WorldSnapshot | null = null;

      if (isBinary || Buffer.isBuffer(data)) {
        const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
        snapshot = decodeBinaryFrame(buf);
      } else {
        const msg = JSON.parse(data.toString());
        if (msg.type === 'WorldSnapshot') {
          snapshot = msg as WorldSnapshot;
        }
      }

      if (snapshot) {
        latestSnapshot = snapshot;
        tickResolvers = tickResolvers.filter(({ target, resolve }) => {
          if (latestSnapshot!.tick >= target) {
            resolve();
            return false;
          }
          return true;
        });
      }
    } catch {}
  });

  return {
    server: ws,
    serverUrl: config.serverUrl,
    metricsBasePort: config.metricsBasePort,
    serverCount: config.serverCount,

    async getMetrics(serverId: number): Promise<ServerMetrics> {
      return fetchMetrics(serverId, config.metricsBasePort);
    },

    async getAllMetrics(): Promise<ServerMetrics[]> {
      const promises: Promise<ServerMetrics>[] = [];
      for (let i = 1; i <= config.serverCount; i++) {
        promises.push(fetchMetrics(i, config.metricsBasePort).catch(() => null as any));
      }
      return (await Promise.all(promises)).filter(Boolean);
    },

    async waitForTicks(count: number): Promise<void> {
      const currentTick = latestSnapshot?.tick ?? 0;
      const target = currentTick + count;
      if (latestSnapshot && latestSnapshot.tick >= target) return;
      return new Promise<void>((resolve) => {
        tickResolvers.push({ target, resolve });
      });
    },

    async sleep(ms: number): Promise<void> {
      return new Promise((resolve) => setTimeout(resolve, ms));
    },

    async spawn(count: number, options?: SpawnOptions): Promise<void> {
      for (let i = 0; i < count; i++) {
        const pos = options?.position ?? [
          (Math.random() - 0.5) * 800,
          0,
          (Math.random() - 0.5) * 800,
        ];
        ws.send(JSON.stringify({
          type: 'SpawnEntity',
          position: pos,
          entity_type: options?.entityType ?? 'npc',
        }));
      }
    },

    getLatestSnapshot(): WorldSnapshot | null {
      return latestSnapshot;
    },

    log(msg: string): void {
      console.log(color.dim(`  [${scenarioName}]`) + ` ${msg}`);
    },
  };
}

// ── Main runner ──

const wsOptions = config.serverUrl.startsWith('wss') ? { rejectUnauthorized: false } : {};

async function connectServer(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, wsOptions);
    ws.on('open', () => resolve(ws));
    ws.on('error', reject);
  });
}

async function cleanupSpawnedEntities(
  baselineIds: Set<string>,
  getSnapshot: () => { tick: number; entities: { id: string; entity_type: string }[] } | null,
) {
  const snap = getSnapshot();
  if (!snap) return 0;

  const toRemove = snap.entities.filter(e => !baselineIds.has(e.id) && e.entity_type === 'Npc');
  if (toRemove.length === 0) return 0;

  // Get all server addresses from coordinator so we can send RemoveEntity to each
  // (entities may have been rebalanced to other servers)
  const serverUrls: string[] = [];
  try {
    const resp = await fetch(`${coordinatorUrl}/servers`);
    const data: { servers: { host: string; ws_port: number }[] } = await resp.json();
    const wsProto = config.serverUrl.startsWith('wss') ? 'wss' : 'ws';
    for (const s of data.servers) {
      serverUrls.push(`${wsProto}://${s.host}:${s.ws_port}`);
    }
  } catch {
    // Can't reach coordinator — just use the local connection
  }

  // Connect to each server and send RemoveEntity for all extra NPCs
  const connections: WebSocket[] = [];
  for (const url of serverUrls) {
    try {
      const ws = await new Promise<WebSocket>((resolve, reject) => {
        const w = new WebSocket(url, wsOptions);
        w.on('open', () => resolve(w));
        w.on('error', reject);
        setTimeout(() => reject(new Error('timeout')), 2000);
      });
      connections.push(ws);
    } catch {}
  }

  for (const e of toRemove) {
    const msg = JSON.stringify({ type: 'RemoveEntity', entity_id: e.id });
    for (const ws of connections) {
      try { ws.send(msg); } catch {}
    }
  }

  // Wait for removals to propagate
  await new Promise(r => setTimeout(r, 1000));

  for (const ws of connections) {
    try { ws.close(); } catch {}
  }

  return toRemove.length;
}

async function runScenario(scenario: Scenario, config: RunnerConfig): Promise<ScenarioResult> {
  const ws = await connectServer(config.serverUrl);
  const ctx = createContext(ws, scenario.name, config);

  // Wait for first snapshot to establish baseline
  await ctx.waitForTicks(1);

  // Record baseline entity IDs so we can clean up after the scenario
  const baselineSnap = ctx.getLatestSnapshot();
  const baselineIds = new Set(baselineSnap?.entities.map(e => e.id) ?? []);

  const start = Date.now();
  let result: ScenarioResult;
  try {
    result = await scenario.run(ctx);
    result.durationMs = Date.now() - start;
  } catch (err: any) {
    result = {
      name: scenario.name,
      status: 'fail',
      summary: `Exception: ${err.message}`,
      details: [err.stack ?? ''],
      durationMs: Date.now() - start,
    };
  }

  // Clean up entities spawned during the scenario
  const removed = await cleanupSpawnedEntities(baselineIds, ctx.getLatestSnapshot.bind(ctx));
  if (removed > 0) {
    console.log(color.dim(`  [cleanup] removed ${removed} entities spawned during ${scenario.name}`));
  }

  ws.close();
  return result;
}

async function main() {
  console.log(color.bold('\n╔══════════════════════════════════════════╗'));
  console.log(color.bold('║    Server Meshing Test Suite              ║'));
  console.log(color.bold('╚══════════════════════════════════════════╝'));
  console.log(`  Server:       ${config.serverUrl}`);
  console.log(`  Coordinator:  ${coordinatorUrl}`);
  if (Object.keys(metricsHosts).length > 0) {
    const hostList = Object.entries(metricsHosts).map(([id, host]) => `S${id}=${host}`).join(', ');
    console.log(`  Metrics:      ${hostList} (base port ${config.metricsBasePort})`);
  } else {
    console.log(`  Metrics:      localhost ports ${config.metricsBasePort}-${config.metricsBasePort + config.serverCount - 1}`);
  }
  console.log(`  Servers:      ${config.serverCount}\n`);

  // Verify connectivity
  try {
    const ws = await connectServer(config.serverUrl);
    ws.close();
  } catch {
    console.error(color.red('Failed to connect to sim-server. Is the cluster running? (./start.sh)'));
    process.exit(1);
  }

  // Verify at least one metrics endpoint
  try {
    await fetchMetrics(1, config.metricsBasePort);
  } catch {
    console.error(color.yellow('Warning: Could not reach metrics endpoint. Server-side metrics will be unavailable.'));
  }

  // Filter scenarios
  let scenarios = ALL_SCENARIOS;
  if (config.scenarios) {
    const names = config.scenarios.map(s => s.toLowerCase());
    scenarios = ALL_SCENARIOS.filter(s => names.includes(s.name.toLowerCase()));
    if (scenarios.length === 0) {
      console.error(`No matching scenarios. Available: ${ALL_SCENARIOS.map(s => s.name).join(', ')}`);
      process.exit(1);
    }
  }

  const results: ScenarioResult[] = [];

  for (const scenario of scenarios) {
    console.log(color.bold(`\n━━━ ${scenario.name} ━━━`));
    console.log(color.dim(`  ${scenario.description}\n`));

    const result = await runScenario(scenario, config);
    results.push(result);

    // Print result
    console.log(`\n  ${statusColor(result.status)} ${result.summary} ${color.dim(`(${(result.durationMs / 1000).toFixed(1)}s)`)}`);
    for (const detail of result.details) {
      console.log(`  ${color.dim('│')} ${detail}`);
    }

    // Brief pause between scenarios to let cluster stabilize
    await new Promise(r => setTimeout(r, 2000));
  }

  // Final summary
  console.log(color.bold('\n╔══════════════════════════════════════════╗'));
  console.log(color.bold('║    Summary                               ║'));
  console.log(color.bold('╚══════════════════════════════════════════╝'));

  for (const r of results) {
    console.log(`  ${statusColor(r.status)} ${r.name}: ${r.summary}`);
  }

  const passed = results.filter(r => r.status === 'pass').length;
  const warned = results.filter(r => r.status === 'warn').length;
  const failed = results.filter(r => r.status === 'fail').length;
  console.log(`\n  ${color.green(`${passed} passed`)} ${warned ? color.yellow(`${warned} warned`) : ''} ${failed ? color.red(`${failed} failed`) : ''}\n`);

  // JSON output for automated consumption
  if (JSON_OUTPUT) {
    // Fetch final server metrics snapshot
    let serverMetrics: ServerMetrics[] = [];
    try {
      serverMetrics = await Promise.all(
        Array.from({ length: config.serverCount }, (_, i) =>
          fetchMetrics(i + 1, config.metricsBasePort).catch(() => null as any)
        )
      ).then(m => m.filter(Boolean));
    } catch {}

    const jsonReport = {
      timestamp: new Date().toISOString(),
      config: {
        serverUrl: config.serverUrl,
        serverCount: config.serverCount,
        metricsBasePort: config.metricsBasePort,
      },
      summary: { passed, warned, failed, total: results.length },
      scenarios: results.map(r => ({
        name: r.name,
        status: r.status,
        summary: r.summary,
        details: r.details,
        durationMs: r.durationMs,
      })),
      serverMetrics,
    };

    console.log('\n--- JSON REPORT ---');
    console.log(JSON.stringify(jsonReport, null, 2));
    console.log('--- END REPORT ---');
  }

  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error('Fatal:', err);
  process.exit(1);
});
