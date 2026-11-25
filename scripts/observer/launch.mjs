import { spawn, spawnSync } from 'node:child_process';
import { createSocket } from 'node:dgram';
import { createServer } from 'node:http';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { receive, Telemetry } from './telemetry.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const host = '127.0.0.1';
const { values } = parseArgs({
  options: {
    players: { type: 'string', default: '96' },
    port: { type: 'string', default: '4174' },
    'cycle-seconds': { type: 'string', default: '64' },
    duration: { type: 'string', default: '0' },
    stop: { type: 'boolean' },
    help: { type: 'boolean', short: 'h' },
  },
});
function integer(key, low, high) {
  const value = Number(values[key]);
  if (!/^\d+$/.test(values[key]) || !Number.isSafeInteger(value) || value < low || value > high)
    throw Error(`--${key} erwartet ${low} bis ${high}`);
  return value;
}

async function main() {
  if (values.help) {
    console.log(
      'npm run observe -- [--port 4174] [--players 96] [--cycle-seconds 64] [--duration SEKUNDEN] [--stop]',
    );
    return;
  }
  const port = integer('port', 0, 65535);
  if (values.stop) {
    const response = await fetch(`http://${host}:${port}/api/stop`, {
      method: 'POST',
      headers: { 'X-Mesh-Observer': '1' },
      signal: AbortSignal.timeout(3000),
    });
    if (!response.ok) throw Error(`Stoppen fehlgeschlagen (HTTP ${response.status})`);
    console.log('Test wird beendet.');
    return;
  }
  const count = integer('players', 12, 120);
  if (count % 6) throw Error('--players muss durch sechs teilbar sein');
  const cycle = integer('cycle-seconds', 8, 240),
    duration = integer('duration', 0, 86400);
  const assets = new Map();
  for (const [name, mime] of [
    ['index.html', 'text/html'],
    ['monitor.css', 'text/css'],
    ['app.mjs', 'text/javascript'],
    ['map.mjs', 'text/javascript'],
  ]) {
    assets.set(name === 'index.html' ? '/' : '/' + name, [
      mime + '; charset=utf-8',
      readFileSync(new URL('ui/' + name, import.meta.url)),
    ]);
  }
  let live = '{"status":"starting"}',
    lastViewer = performance.now(),
    ending = false;
  const children = [],
    closed = [],
    workers = new Map();
  const senders = new Map();
  const target = join(root, 'target');
  let timer, forceTimer, stagingDir;
  const socks = [];
  const clients = Array.from({ length: count }, (_, i) => ({
    id: i + 1,
    tick: 0,
    frames: 0,
    bytes: 0,
    own: null,
    neighbors: [],
    sender: 0,
  }));
  const telemetry = new Telemetry();
  const server = createServer((req, res) => {
    const port = server.address()?.port;
    const authorities = [host + ':' + port, 'localhost:' + port];
    res.setHeader('Cache-Control', 'no-store');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader(
      'Content-Security-Policy',
      "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
    );
    if (!authorities.includes(req.headers.host)) {
      res.writeHead(403).end();
      return;
    }
    if (req.method === 'POST' && req.url === '/api/stop') {
      const origin = req.headers.origin;
      if (
        req.headers['x-mesh-observer'] !== '1' ||
        (origin && origin !== 'http://' + req.headers.host)
      ) {
        res.writeHead(403).end();
        return;
      }
      res
        .writeHead(200, { 'Content-Type': 'application/json', Connection: 'close' })
        .end('{"status":"stopped"}', stop);
      return;
    }
    if (req.method !== 'GET') {
      res.writeHead(405).end();
      return;
    }
    if (req.url === '/api/state') {
      lastViewer = performance.now();
      res.writeHead(200, { 'Content-Type': 'application/json' }).end(live);
    } else if (assets.has(req.url)) {
      const [mime, bytes] = assets.get(req.url);
      res.writeHead(200, { 'Content-Type': mime }).end(bytes);
    } else res.writeHead(404).end();
  });
  function stop() {
    if (ending) return;
    ending = true;
    clearInterval(timer);
    if (server.listening) {
      server.close();
      server.closeAllConnections();
    }
    for (const s of socks) {
      try {
        s.close();
      } catch {
        /* bind failed */
      }
    }
    for (const child of children) child.stdin.end();
    forceTimer = setTimeout(() => {
      for (const child of children)
        if (child.exitCode === null && child.signalCode === null) child.kill();
    }, 3000);
    forceTimer.unref();
  }
  function fail(error) {
    if (ending) return;
    console.error(error.message);
    process.exitCode = 1;
    stop();
  }
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
  try {
    await new Promise((ok, no) => {
      server.once('error', no);
      server.listen(port, host, ok);
    });
    server.on('error', fail);
    const build = spawnSync(
      'cargo',
      [
        'build',
        '--locked',
        '--release',
        '-p',
        'infinite-stars',
        '--example',
        'universe_monitor',
        '--message-format=json-render-diagnostics',
      ],
      {
        cwd: root,
        windowsHide: true,
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'inherit'],
        maxBuffer: 16 * 1024 * 1024,
      },
    );
    if (build.error) throw build.error;
    if (build.status !== 0) throw Error('Rust-Build fehlgeschlagen');
    const artifact = build.stdout
      .trim()
      .split('\n')
      .map((line) => JSON.parse(line))
      .find((row) => row.executable && row.target?.name === 'universe_monitor');
    if (!artifact) throw Error('Observer-Binary fehlt im Cargo-Build');
    let binary = artifact.executable;
    mkdirSync(target, { recursive: true });
    stagingDir = mkdtempSync(join(target, 'mesh-observer-'));
    if (process.platform === 'win32') {
      const staged = join(stagingDir, basename(binary));
      copyFileSync(binary, staged);
      binary = staged;
    }
    const init = spawnSync(binary, ['--init', stagingDir], { windowsHide: true, stdio: 'inherit' });
    if (init.error) throw init.error;
    if (init.status !== 0) throw Error('Die Testwelt konnte nicht initialisiert werden');
    const ports = [];
    for (const client of clients) {
      if (ending) return;
      const socket = createSocket('udp4');
      socks.push(socket);
      socket.on('message', (packet, source) => {
        if (source.address === host && source.port === client.sender) receive(packet, client, count);
      });
      await new Promise((ok, no) => {
        socket.once('error', no);
        socket.bind(0, host, ok);
      });
      socket.on('error', fail);
      ports.push(socket.address().port);
    }
    if (ending) return;
    const startup = [];
    for (let slot = 1; slot <= 4; slot++) {
      const child = spawn(binary, [stagingDir, String(slot)], {
        cwd: root,
        windowsHide: true,
        stdio: ['pipe', 'pipe', 'inherit'],
      });
      children.push(child);
      const ready = Promise.withResolvers();
      startup.push(ready.promise);
      closed.push(
        new Promise((resolve) =>
          child.once('close', (code) => {
            const error = Error(`Worker W${slot} wurde beendet (Exit ${code ?? 'Signal'})`);
            ready.reject(error);
            if (!ending) fail(error);
            resolve();
          }),
        ),
      );
      child.stdin.on('error', fail);
      child.once('error', fail);
      const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
      lines.on('line', (line) => {
        if (ending) return;
        try {
          const event = JSON.parse(line);
          if (event.type === 'ready') {
            senders.set(slot, event.port);
            for (const id of event.clients) {
              if (!clients[id - 1] || clients[id - 1].sender)
                throw Error('Doppelte oder unbekannte Authority');
              clients[id - 1].sender = event.port;
            }
            ready.resolve();
          } else if (event.type === 'sample') {
            const { topology, ...worker } = event;
            workers.set(slot, worker);
            if (slot !== 1 || topology?.players.length !== count || workers.size !== 4) return;
            for (const player of topology.players) {
              const client = clients[player.id - 1];
              const next = senders.get(player.worker);
              if (client.sender !== next) {
                client.sender = next;
                client.tick = 0;
                client.own = null;
                client.neighbors = [];
              }
            }
            const all = [...workers.values()].sort((a, b) => a.id - b.id);
            const frame = {
              tick: worker.tick,
              seconds: worker.seconds,
              phase: worker.phase,
              cycle: worker.cycle,
              traveler: worker.traveler,
              handoffPlayer: worker.handoffPlayer,
              players: topology.players,
              workers: all,
              peerLinks: topology.routes.length,
              routes: topology.routes,
              stepMs: Math.max(...all.map((w) => w.stepMs)),
              peerApplied: all.reduce((total, w) => total + w.applied, 0),
            };
            const state = telemetry.sample(frame, clients);
            live = JSON.stringify({ status: 'live', cycleSeconds: cycle, ...state });
          }
        } catch (error) {
          fail(error);
        }
      });
      child.stdin.write(JSON.stringify({ ports, cycle_seconds: cycle }) + '\n');
    }
    await Promise.all(startup);
    if (ending) return;
    if (!clients.every((client) => client.sender)) throw Error('Spieler ohne Authority');
    for (const child of children) child.stdin.write('start\n');
    const started = performance.now();
    timer = setInterval(() => {
      if (
        (duration && performance.now() - started >= duration * 1000) ||
        performance.now() - lastViewer > 600_000
      )
        stop();
    }, 500);
    console.log(`Mesh-Monitor  http://${host}:${server.address().port}/`);
    console.log(`${count} Loopback-Testclients · 4 mTLS-Workerprozesse · Ctrl+C oder „Test beenden“`);
    await Promise.all(closed);
  } finally {
    stop();
    await Promise.all(closed);
    clearTimeout(forceTimer);
    // All processes have released their executable, SQLite database and test certificates.
    if (
      stagingDir &&
      dirname(stagingDir) === target &&
      basename(stagingDir).startsWith('mesh-observer-')
    )
      rmSync(stagingDir, { recursive: true, force: true, maxRetries: 5 });
  }
}
main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
