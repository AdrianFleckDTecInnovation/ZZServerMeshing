import { spawn, spawnSync } from 'node:child_process';
import { createSocket } from 'node:dgram';
import { once } from 'node:events';
import { closeSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { basename, join, resolve } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { benchmark } from './mesh-benchmark.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const defaults = Object.fromEntries(readFileSync(new URL('./local-defaults.env', import.meta.url), 'utf8')
  .trim().split(/\r?\n/).map(line => line.split('=')));
const host = '127.0.0.1';

function integer(label, value, min, max) {
  const n = Number(value);
  if (!/^\d+$/.test(value) || !Number.isSafeInteger(n) || n < min || n > max) {
    throw new Error(`${label} must be an integer between ${min} and ${max}`);
  }
  return n;
}

function setting(key) {
  return process.env[key] || defaults[key];
}

function readRun(marker) {
  try { return readFileSync(marker, 'utf8').trim(); }
  catch (error) { if (error.code !== 'ENOENT') throw error; return ''; }
}

function build(release, sockets) {
  // Cargo reports the actual paths, including custom target directories and .exe suffixes.
  const args = ['build', '--locked', '-p', 'coordinator', '-p', 'sim-server',
    '--message-format=json-render-diagnostics'];
  if (release) args.push('--release');
  if (sockets) args.push('--bins', '--examples');
  const result = spawnSync('cargo', args, { cwd: root, encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 16 * 1024 * 1024 });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error('Cargo build failed');
  const artifacts = result.stdout.trim().split('\n').map(line => JSON.parse(line));
  return Object.fromEntries(artifacts.filter(a => a.executable).map(a => [a.target.name, a.executable]));
}

async function checkPorts(tcp, udp, vitePort, serviceHost) {
  for (const [kind, ports] of [['TCP', tcp], ['UDP', udp]]) {
    if (new Set(ports).size !== ports.length) throw new Error(`Overlapping ${kind} ports in configuration`);
    for (const port of ports) {
      integer(`${kind} port`, port, 1, 65535);
      const socket = kind === 'TCP' ? createServer() : createSocket('udp4');
      const listening = once(socket, 'listening');
      // Historical comparison binaries listen on all interfaces. The native
      // launcher binds its own builds and Vite to loopback.
      if (kind === 'TCP') socket.listen(port, port === vitePort ? host : serviceHost);
      else socket.bind(port, serviceHost);
      try { await listening; }
      catch (error) { throw new Error(`${kind} port ${port} unavailable (${error.code})`); }
      await new Promise(resolve => socket.close(resolve));
    }
  }
}

async function main() {
  const [mode = 'start', ...args] = process.argv.slice(2);
  const { values } = parseArgs({ args, options: {
    servers: { type: 'string' }, npcs: { type: 'string' },
    ...(mode === 'bench' ? {
      seconds: { type: 'string' }, 'bin-dir': { type: 'string' }, 'client-ramp': { type: 'boolean' },
      players: { type: 'string' }, 'socket-players': { type: 'string' }, 'socket-drivers': { type: 'string' },
      'snapshot-ticks': { type: 'string' }, 'input-hz': { type: 'string' },
      'require-budget': { type: 'boolean' }, islands: { type: 'string' },
    } : {}),
    help: { type: 'boolean', short: 'h' },
  } });
  if (values.help) {
    console.log([
      'node scripts/local-mesh.mjs start|stop|smoke|bench [--servers N] [--npcs TOTAL]',
      'bench [--seconds N] [--bin-dir RELEASE_DIR] [--client-ramp | --players N]',
      'bench --socket-players N [--socket-drivers N] [--islands N] [--snapshot-ticks 1..4] [--input-hz 1..10] [--require-budget]',
    ].join('\n'));
    return;
  }
  const client = integer('CLIENT_PORT', setting('CLIENT_PORT'), 1, 65535);
  const marker = join(root, 'target', `mesh-client-${client}.run`);
  if (mode === 'stop') {
    const run = readRun(marker);
    if (!run) { console.log('No foreground mesh is registered on this client port.'); return; }
    if (!/^mesh-run-[a-zA-Z0-9]+$/.test(run)) throw new Error(`Invalid run marker: ${marker}`);
    writeFileSync(join(root, 'target', run, 'stop'), '');
    const deadline = performance.now() + 10000;
    while (readRun(marker) === run) {
      if (performance.now() > deadline) throw new Error(`No shutdown acknowledgement from ${run}`);
      await sleep(100);
    }
    console.log('Local mesh stopped.');
    return;
  }
  if (!['start', 'smoke', 'bench'].includes(mode)) throw new Error(`Unknown command: ${mode}`);
  const count = integer('--servers', values.servers ?? setting('DEFAULT_NUM_SERVERS'), mode === 'smoke' ? 2 : 1, 65535);
  const sockets = values['socket-players'] === undefined ? 0 : integer('--socket-players', values['socket-players'], count, 20000);
  if (sockets && (count < 2 || count % 2 || count > 32 || values.players || values['client-ramp'] || values['bin-dir'])) {
    throw new Error('--socket-players requires an even count of 2..32 current workers');
  }
  const total = integer('--npcs', values.npcs ?? (sockets ? '0' : setting('DEFAULT_TOTAL_NPCS')), 0, 0xffffffff);
  const players = values.players === undefined ? undefined : integer('--players', values.players, 1, 65535);
  const cadence = integer('--snapshot-ticks', values['snapshot-ticks'] ?? '1', 1, 4);
  const inputHz = integer('--input-hz', values['input-hz'] ?? '5', 1, 10);
  const sites = sockets ? integer('--islands', values.islands ?? String(Math.min(256, Math.floor(sockets / 2))),
    count / 2, Math.floor(sockets / 2)) : 0;
  const side = Math.ceil(Math.sqrt(sites));
  const spacing = sites > 256 ? 1000 : 620;
  const extent = Math.max(5000, side * spacing / 2);
  if (players && values['client-ramp']) throw new Error('Choose a fixed --players count or --client-ramp');
  const seconds = integer('--seconds', values.seconds ?? (sockets ? '4' : '15'), sockets ? 2 : 10, sockets ? 60 : 3600);
  const port = key => integer(key, setting(key), 1, 65535);
  const ws = port('COORD_WS_PORT'), http = port('COORD_HTTP_PORT');
  const coord = `http://${host}:${http}`;
  const servers = Array.from({ length: count }, (_, i) => ({
    id: i + 1, ws: port('SIM_WS_BASE') + i + 1, udp: port('SIM_UDP_BASE') + i + 1,
    metrics: port('SIM_METRICS_BASE') + i + 1,
  }));
  const frontend = mode !== 'bench';
  const vite = join(root, 'client/node_modules/vite/bin/vite.js');
  if ((frontend && !existsSync(vite)) || (mode === 'smoke' && !existsSync(join(root, 'tools/node_modules/ws')))) {
    throw new Error('Install the local dependencies first with npm run setup');
  }
  await checkPorts([ws, http, ...(frontend ? [client] : []), ...servers.flatMap(s => [s.ws, s.metrics])],
    servers.map(s => s.udp), frontend ? client : undefined, values['bin-dir'] ? '0.0.0.0' : host);
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const bins = values['bin-dir']
    ? Object.fromEntries(['coordinator', 'sim-server'].map(name => [name, resolve(values['bin-dir'], name + suffix)]))
    : build(mode === 'bench', sockets);
  mkdirSync(join(root, 'target'), { recursive: true });
  const logs = mkdtempSync(join(root, 'target', 'mesh-run-'));
  console.error(`Service logs: ${logs}`);

  const controller = new AbortController();
  const { signal } = controller;
  let interrupted = false;
  const interrupt = () => { interrupted = true; controller.abort(); };
  process.once('SIGINT', interrupt);
  process.once('SIGTERM', interrupt);
  const children = [];
  const staged = [];
  let stopping = false;

  function launch(name, command, argv, options = {}) {
    const fd = options.task ? undefined : openSync(join(logs, `${name}.log`), 'w');
    let child;
    try {
      child = spawn(command, argv.map(String), { cwd: options.cwd ?? root, windowsHide: true,
        env: { ...process.env, ...options.env }, stdio: options.task ? 'inherit' : ['ignore', fd, fd] });
    } finally { if (fd !== undefined) closeSync(fd); }
    const done = new Promise(resolve => {
      child.once('error', error => { controller.abort(error); });
      child.once('close', (code, why) => {
        if (!stopping && (!options.task || code !== 0)) {
          controller.abort(new Error(`${name} exited (${why ?? code}). See ${logs}`));
        }
        resolve();
      });
    });
    children.push({ child, done });
    return done;
  }

  async function getJson(url) {
    const response = await fetch(url, { signal: AbortSignal.any([signal, AbortSignal.timeout(2000)]) });
    if (!response.ok) throw new Error(`${url} returned HTTP ${response.status}`);
    return response.json();
  }
  const metrics = () => Promise.all(servers.map(s => getJson(`http://${host}:${s.metrics}/metrics`)));
  async function waitFor(label, probe) {
    const deadline = performance.now() + 30000;
    let lastError;
    while (performance.now() < deadline) {
      signal.throwIfAborted();
      try { if (await probe()) return; } catch (error) { lastError = error; }
      await sleep(200, undefined, { signal });
    }
    throw new Error(`Timed out waiting for ${label} (${lastError?.message ?? 'not ready'}). See ${logs}`);
  }

  try {
    // Windows locks a running .exe. Stage each executable once so cargo test
    // and subsequent builds can replace their own artifacts during development.
    if (process.platform === 'win32') {
      for (const [name, binary] of Object.entries(bins).filter(([n]) => ['coordinator', 'sim-server', 'player-sockets'].includes(n))) {
        const copy = join(logs, basename(binary));
        copyFileSync(binary, copy);
        staged.push(copy);
        bins[name] = copy;
      }
    }
    const localOnly = { env: {
      MESH_LOOPBACK_ONLY: '1',
      ...(sockets ? {
        TOKIO_WORKER_THREADS: '2', MESH_IDLE_SNAPSHOT_TICKS: '10',
        MESH_SNAPSHOT_TICKS: String(cadence), MESH_WORLD_EXTENT: String(extent),
      } : {}),
    } };
    launch('coordinator', bins.coordinator, ['--port', ws, '--http-port', http], localOnly);
    await waitFor('coordinator', async () => Array.isArray((await getJson(`${coord}/servers`)).servers));
    for (const s of servers) {
      const npcs = Math.floor(total / count) + (s.id <= total % count ? 1 : 0);
      const spawnArgs = [];
      if (sockets) {
        const pair = Math.floor((s.id - 1) / 2);
        const points = [];
        for (let j = Math.floor(pair * sites / (count / 2)); j < Math.floor((pair + 1) * sites / (count / 2)); j++) {
          points.push([(Math.floor(j / side) - (side - 1) / 2) * spacing, 0, (j % side - (side - 1) / 2) * spacing]);
        }
        const file = join(logs, `spawns-${s.id}.json`);
        writeFileSync(file, JSON.stringify(points));
        spawnArgs.push('--spawn-points', file);
      }
      launch(`sim-${s.id}`, bins['sim-server'], ['--server-id', s.id, '--port', s.ws, '--udp-port', s.udp,
        '--metrics-port', s.metrics, '--coordinator', `${host}:${ws}`, '--advertise-host', host,
        '--initial-npcs', npcs, '--cold-start', ...spawnArgs], localOnly);
    }
    await waitFor('all mesh peers', async () => {
      const rows = await metrics();
      return rows.every((m, i) => m.server_id === servers[i].id && m.tick > 0 && m.peer_count === count - 1)
        && rows.reduce((sum, m) => sum + m.entity_count, 0) === total;
    });
    if (frontend) {
      launch('client', process.execPath, [vite, '--host', host, '--port', client, '--strictPort'], {
        cwd: join(root, 'client'), env: { MESH_COORDINATOR_URL: coord, VITE_COORDINATOR_URL: '' },
      });
      const url = `http://${host}:${client}`;
      await waitFor('client and assignment', async () => {
        const page = await fetch(url, { signal: AbortSignal.timeout(2000) });
        const assignment = await getJson(`${url}/assign`);
        return page.ok && (await page.text()).includes('/src/main.ts')
          && servers.some(s => s.ws === Number(new URL(assignment.ws_url).port));
      });
      console.error(`Client ${url}   Dashboard ${coord}/dashboard   (Ctrl+C to stop)`);
    }
    if (sockets) {
      const { socketClients } = await import('./socket-clients.mjs');
      const drivers = integer('--socket-drivers', values['socket-drivers'] ?? String(Math.min(4, Math.ceil(sockets / 2000))), 1, Math.min(sockets, 32));
      const result = await socketClients({ binary: bins['player-sockets'], servers, players: sockets,
        seconds, drivers, inputHz, cadence, sites, spacing, extent, logs, launch, metrics, signal });
      console.log(JSON.stringify(result, null, 2));
      if (values['require-budget'] && !result.met_interactive_budget) throw new Error('Socket workload exceeded the requested delivery budget');
    } else if (mode === 'bench') {
      const measure = (players || values['client-ramp']) ? (await import('./bench-player-clients.mjs')).benchmarkPlayers : benchmark;
      console.log(JSON.stringify(await measure({ servers, host, total, seconds, players, metrics, signal }), null, 2));
    } else if (mode === 'smoke') {
      const task = launch('smoke scenarios', process.execPath, ['--experimental-strip-types', 'test-suite/runner.ts', `ws://${host}:${servers[0].ws}`,
        servers[0].metrics, count, 'peer-replication,input-seq-echo', '--no-color'], {
        task: true, cwd: join(root, 'tools'), env: { COORDINATOR_URL: coord, METRICS_HOSTS: '' },
      });
      await Promise.race([task, once(signal, 'abort')]);
    } else {
      writeFileSync(marker, basename(logs));
      while (!existsSync(join(logs, 'stop'))) await sleep(200, undefined, { signal });
    }
    signal.throwIfAborted();
  } catch (error) {
    if (!interrupted) throw signal.aborted ? signal.reason : error;
  } finally {
    stopping = true;
    const closed = Promise.all(children.map(({ done }) => done));
    for (const { child } of children) child.kill('SIGTERM');
    await Promise.race([closed, sleep(2000, undefined, { ref: false })]);
    for (const { child } of children) child.kill('SIGKILL');
    await closed;
    for (const copy of staged) unlinkSync(copy);
    if (mode === 'start' && readRun(marker) === basename(logs)) unlinkSync(marker);
    process.removeListener('SIGINT', interrupt);
    process.removeListener('SIGTERM', interrupt);
  }
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
