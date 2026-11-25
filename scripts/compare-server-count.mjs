import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { arch, cpus, platform, release } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({ options: {
  players: { type: 'string', default: '300' }, npcs: { type: 'string', default: '300' },
  pairs: { type: 'string', default: '1' }, out: { type: 'string', default: 'target/server-count.json' },
} });
function integer(input, min, max) {
  const n = Number(input);
  if (!/^\d+$/.test(input) || !Number.isSafeInteger(n) || n < min || n > max) {
    throw new Error(`Expected an integer from ${min} to ${max}, got ${input}`);
  }
  return n;
}
const counts = [...new Set(values.players.split(',').map(n => integer(n, 1, 65535)))];
const npcs = integer(values.npcs, 0, 0xffffffff);
const pairs = integer(values.pairs, 1, 20);
const output = resolve(root, values.out);
const env = { COORD_WS_PORT: '20710', COORD_HTTP_PORT: '20711', SIM_WS_BASE: '20717',
  SIM_METRICS_BASE: '20810', SIM_UDP_BASE: '20910', ...process.env };
const options = { cwd: root, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 32 * 1024 * 1024 };
const built = execFileSync('cargo', ['build', '--locked', '--release', '-p', 'sim-server', '-p', 'coordinator',
  '--bin', 'coordinator', '--bin', 'sim-server', '--message-format=json-render-diagnostics'], options);
const executables = built.trim().split('\n').map(line => JSON.parse(line)).filter(a => a.executable);
const binDir = dirname(executables.find(a => a.target.name === 'sim-server').executable);
const git = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' });
const report = { version: 1, measured_at: new Date().toISOString(), revision: git.stdout?.trim() || null,
  binaries_sha256: Object.fromEntries(executables.map(a =>
    [a.target.name, createHash('sha256').update(readFileSync(a.executable)).digest('hex')])),
  os: `${platform()} ${release()} ${arch()}`, cpu: cpus()[0]?.model, node: process.version,
  rustc: execFileSync('rustc', ['-V'], options).trim(), npcs, pairs, runs: [] };
mkdirSync(dirname(output), { recursive: true });
const start = performance.now();
for (const players of counts) {
  for (let pair = 1; pair <= pairs; pair++) {
    for (const servers of (pair % 2 ? [1, 3] : [3, 1])) {
      const data = JSON.parse(execFileSync(process.execPath, ['--experimental-strip-types', 'scripts/local-mesh.mjs',
        'bench', '--servers', String(servers), '--npcs', String(npcs), '--players', String(players),
        '--bin-dir', binDir], options));
      report.runs.push({ players, pair, ...data });
      report.workload_seconds = Number(((performance.now() - start) / 1000).toFixed(2));
      writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
      const row = data.rows[0];
      if (!row) {
        console.log(`${players} players, ${servers} sim(s): ${data.stopped.reason}`);
        continue;
      }
      const tick = Math.max(...row.server_metrics.map(m => m.tick_mean_ms));
      const snapshots = Math.max(...row.server_metrics.map(m => m.snapshot_mean_ms));
      console.log(`${players} players, ${servers} sim(s): ${row.echoed}/${row.planned} echoes, ` +
        `${row.echo_ms.p95} ms input p95, ${snapshots}/${tick} ms mean snapshot/tick on slowest for each metric, ` +
        `${row.snapshot_MiB_s} MiB/s`);
    }
  }
}
console.log(`${report.workload_seconds}s including service startups. ${output}`);
