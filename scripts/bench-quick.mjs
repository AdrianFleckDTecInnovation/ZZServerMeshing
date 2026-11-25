import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { arch, cpus, platform, release } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const target = join(root, 'target');
mkdirSync(target, { recursive: true });
const temp = mkdtempSync(join(target, 'quick-profile-'));
const env = { COORD_WS_PORT: '18710', COORD_HTTP_PORT: '18711', SIM_WS_BASE: '18717',
  SIM_METRICS_BASE: '18810', SIM_UDP_BASE: '18910', ...process.env };
const opts = { cwd: root, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 32 * 1024 * 1024 };
const start = performance.now();
try {
  const build = execFileSync('cargo', ['build', '--locked', '--release', '-p', 'infinite-stars',
    '-p', 'coordinator', '-p', 'sim-server', '--bin', 'cluster-bench', '--bin', 'coordinator', '--bin', 'sim-server',
    '--message-format=json-render-diagnostics'], opts);
  const executables = new Map(build.trim().split('\n').map(line => JSON.parse(line))
    .filter(a => a.executable).map(a => [a.target.name, a.executable]));
  const ready = performance.now();
  const spatialFile = join(temp, 'spatial.json');
  execFileSync(executables.get('cluster-bench'), [spatialFile, '--quick'], opts);
  const afterSpatial = performance.now();
  const browser = JSON.parse(execFileSync(process.execPath, ['--experimental-strip-types', 'scripts/local-mesh.mjs',
    'bench', '--client-ramp', '--servers', '3', '--npcs', '300', '--bin-dir', dirname(executables.get('sim-server'))], opts));
  const end = performance.now();
  const seconds = ms => Number((ms / 1000).toFixed(2));
  const revision = spawnSync('git', ['rev-parse', '--verify', 'HEAD'], { cwd: root, encoding: 'utf8' });
  const report = { version: 2, revision: revision.status === 0 ? revision.stdout.trim() : null,
    measured_at: new Date().toISOString(), os: `${platform()} ${release()} ${arch()}`,
    cpu: cpus()[0]?.model, node: process.version, rustc: execFileSync('rustc', ['-V'], opts).trim(),
    profile: 'release', seconds: { build: seconds(ready-start), spatial: seconds(afterSpatial-ready),
      browser: seconds(end-afterSpatial), workload: seconds(end-ready) },
    spatial: JSON.parse(readFileSync(spatialFile, 'utf8')), browser };
  const output = join(target, 'mesh-quick.json');
  const staged = join(temp, 'report.json');
  writeFileSync(staged, JSON.stringify(report, null, 2)+'\n');
  renameSync(staged, output);
  console.table(browser.rows.map(r => ({ players: r.clients, planned: r.planned, echoed: `${r.echoed}/${r.sent}`,
    input_p95_ms: r.echo_ms.p95, min_snapshot_hz: r.min_snapshot_hz,
    min_server_hz: Math.min(...r.server_metrics.map(m => m.tick_hz)) })));
  if (browser.stopped) console.warn(`Stopped at ${browser.stopped.clients} clients: ${browser.stopped.reason}`);
  console.log(`${report.seconds.workload}s workload, ${report.seconds.build}s build. ${output}`);
} finally {
  rmSync(temp, { recursive: true, force: true });
}
