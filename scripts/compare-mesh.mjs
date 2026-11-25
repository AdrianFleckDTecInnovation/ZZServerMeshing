// Run prebuilt release servers serially; reverse order on alternating repetitions.
import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { buildReference, sourceRevision } from './build-benchmark-reference.mjs';

const { values } = parseArgs({ options: {
  reference: { type: 'string' }, candidate: { type: 'string' }, out: { type: 'string', default: 'target/browser-comparison.json' },
  npcs: { type: 'string', default: '30,3000,9000' }, seconds: { type: 'string', default: '20' },
  repeats: { type: 'string', default: '3' }, 'candidate-first': { type: 'boolean', default: false },
  'max-growth-percent': { type: 'string' }, 'noise-ms': { type: 'string', default: '0' },
} });
if (!/^[1-9]\d*$/.test(values.repeats) || !Number.isSafeInteger(Number(values.repeats))) {
  throw new Error('--repeats must be a positive integer');
}
const loads = values.npcs.split(',').map(value => {
  const text = value.trim();
  if (!/^\d+$/.test(text) || Number(text) > 0xffffffff) throw new Error(`Invalid NPC count: ${value}`);
  return Number(text);
});
if (new Set(loads).size !== loads.length) throw new Error('--npcs contains duplicate load levels');
if (!/^\d+$/.test(values.seconds) || Number(values.seconds) < 10 || Number(values.seconds) > 3600) {
  throw new Error('--seconds must be an integer between 10 and 3600');
}
const maxGrowth = values['max-growth-percent'] === undefined ? Infinity : Number(values['max-growth-percent']);
const noise = Number(values['noise-ms']);
if ((values['max-growth-percent'] !== undefined && !Number.isFinite(maxGrowth)) || !(maxGrowth >= 0)
    || !Number.isFinite(noise) || noise < 0) throw new Error('Invalid regression tolerance');
const original = !values.reference;
values.reference ??= buildReference();
if (!values.candidate) {
  const target = resolve(process.env.CARGO_TARGET_DIR || 'target');
  execFileSync('cargo', ['build', '--release', '--locked', '-p', 'sim-server', '-p', 'coordinator',
    '--target-dir', target], { stdio: 'inherit' });
  values.candidate = join(target, 'release');
}
const launcher = fileURLToPath(new URL('./local-mesh.mjs', import.meta.url));
// Keep the foreground development mesh available throughout a repeated comparison.
const meshEnv = { COORD_WS_PORT: '17710', COORD_HTTP_PORT: '17711', SIM_WS_BASE: '17717',
  SIM_METRICS_BASE: '18100', SIM_UDP_BASE: '19100', ...process.env };
const suffix = process.platform === 'win32' ? '.exe' : '';
const binaryHashes = Object.fromEntries(['reference', 'candidate'].map(variant => [variant,
  Object.fromEntries(['sim-server', 'coordinator'].map(name =>
    [name, createHash('sha256').update(readFileSync(join(values[variant], name + suffix))).digest('hex')]))]));
const report = { version: 1, reference: original ? sourceRevision : 'provided release binaries',
  binary_sha256: binaryHashes, runs: [], summary: [] };
for (let rep = 1; rep <= Number(values.repeats); rep++) {
  for (const npcs of loads) {
    const referenceFirst = (rep % 2 === 1) !== values['candidate-first'];
    for (const variant of referenceFirst ? ['reference', 'candidate'] : ['candidate', 'reference']) {
      const run = spawnSync(process.execPath, [launcher, 'bench', '--servers', '3', '--npcs', String(npcs),
        '--seconds', values.seconds, '--bin-dir', values[variant]], { encoding: 'utf8', env: meshEnv,
        stdio: ['ignore', 'pipe', 'inherit'] });
      if (run.error) throw run.error;
      if (run.status !== 0) throw new Error(`Benchmark failed (${variant}, ${npcs} NPCs)`);
      const data = JSON.parse(run.stdout);
      report.runs.push({ rep, variant, ...data });
      writeFileSync(values.out, JSON.stringify(report, null, 2) + '\n');
      console.error(`${variant} ${npcs} rep ${rep} slowest p95 ${Math.max(...data.servers.map(s => s.tick_p95_ms))} ms`);
    }
  }
}
function median(xs) {
  xs.sort((a, b) => a - b);
  return (xs[Math.floor((xs.length - 1) / 2)] + xs[Math.floor(xs.length / 2)]) / 2;
}
for (const load of loads) {
  for (const variant of ['reference', 'candidate']) {
    const rows = report.runs.filter(r => r.npcs === load && r.variant === variant);
    const worst = (key, fn) => median(rows.map(r => fn(...r.servers.map(s => s[key]))));
    report.summary.push({ npcs: load, variant,
      slowest_p50_ms: worst('tick_p50_ms', Math.max), slowest_p95_ms: worst('tick_p95_ms', Math.max),
      slowest_p99_ms: worst('tick_p99_ms', Math.max), min_tick_hz: worst('tick_hz', Math.min),
      min_snapshot_hz: worst('snapshot_hz', Math.min) });
  }
}
writeFileSync(values.out, JSON.stringify(report, null, 2) + '\n');
console.table(report.summary);
report.comparison = loads.map(load => {
  const [reference, candidate] = report.summary.filter(row => row.npcs === load);
  const comparison = { npcs: load };
  for (const percentile of ['p50', 'p95', 'p99']) {
    const metric = `slowest_${percentile}_ms`;
    const change = candidate[metric] - reference[metric];
    comparison[`${percentile}_delta_ms`] = Number(change.toFixed(3));
    comparison[`${percentile}_delta_pct`] = Number((100 * change / reference[metric]).toFixed(1));
    if (percentile !== 'p50' && change > Math.max(noise, reference[metric] * maxGrowth / 100)) {
      process.exitCode = 1;
      console.error(`${load} NPCs exceed the configured ${percentile} regression tolerance`);
    }
  }
  return comparison;
});
writeFileSync(values.out, JSON.stringify(report, null, 2) + '\n');
console.table(report.comparison);
