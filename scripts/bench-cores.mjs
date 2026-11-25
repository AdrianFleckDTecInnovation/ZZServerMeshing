import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { availableParallelism, cpus, platform, release } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({
  options: {
    players: { type: 'string', default: '10000' },
    workers: { type: 'string' },
    ticks: { type: 'string', default: '32' },
    runs: { type: 'string', default: '2' },
    output: { type: 'string', default: 'target/core-scaling.json' },
    native: { type: 'boolean' },
    help: { type: 'boolean', short: 'h' },
  },
});
if (values.help) {
  console.log(
    'npm run bench:cores -- [--players 10000] [--workers 32] [--ticks 32] [--runs 2] [--native] [--output target/core-scaling.json]',
  );
  process.exit(0);
}
function integer(key, low, high) {
  const n = Number(values[key]);
  if (!Number.isSafeInteger(n) || n < low || n > high)
    throw Error(`--${key} erwartet ${low} bis ${high}`);
  return n;
}
const players = integer('players', 16, 65536),
  ticks = integer('ticks', 20, 10000),
  runs = integer('runs', 1, 10);
if (players % 16) throw Error('--players muss durch die Inselgröße 16 teilbar sein');
const logical = availableParallelism();
const automatic = 2 ** Math.floor(Math.log2(Math.min(logical, players / 16, 128)));
const workers = values.workers === undefined ? 2 * automatic : integer('workers', 2, 256);
if (workers % 2) throw Error('--workers muss gerade sein');
const partitions = Array.from(
  { length: Math.min(workers / 2, players / 16) },
  (_, i) => i + 1,
).filter((n) => workers % (2 * n) === 0 && (values.workers !== undefined || n <= logical));
const env = { ...process.env };
if (values.native) {
  if (env.CARGO_ENCODED_RUSTFLAGS !== undefined) {
    env.CARGO_ENCODED_RUSTFLAGS = [env.CARGO_ENCODED_RUSTFLAGS, '-Ctarget-cpu=native']
      .filter(Boolean)
      .join('\x1f');
  } else env.RUSTFLAGS = [env.RUSTFLAGS, '-Ctarget-cpu=native'].filter(Boolean).join(' ');
  env.CARGO_TARGET_DIR = join(resolve(root, env.CARGO_TARGET_DIR || 'target'), 'cpu-native');
}
const opts = { cwd: root, env, encoding: 'utf8', windowsHide: true, maxBuffer: 32 * 1024 * 1024 };
const build = execFileSync(
  'cargo',
  [
    'build',
    '--locked',
    '--release',
    '-p',
    'infinite-stars',
    '--bin',
    'mesh-profile',
    '--message-format=json-render-diagnostics',
  ],
  opts,
);
const binary = build
  .trim()
  .split('\n')
  .map((s) => JSON.parse(s))
  .find((s) => s.target?.name === 'mesh-profile' && s.executable).executable;
const median = (list) => {
  const v = [...list].sort((a, b) => a - b);
  return (v[Math.floor((v.length - 1) / 2)] + v[Math.floor(v.length / 2)]) / 2;
};
const samples = [];
for (let r = 0; r < runs; r++) {
  for (const threads of r % 2 ? [...partitions].reverse() : partitions) {
    const args = [
      '--players',
      players,
      '--island-size',
      16,
      '--workers',
      workers,
      '--partitions',
      threads,
      '--ticks',
      ticks,
    ].map(String);
    const data = JSON.parse(execFileSync(binary, args, opts));
    const profiles = data.partition_profiles ?? [data];
    samples.push({
      run: r + 1,
      threads,
      p50_ms: data.tick_ms.p50,
      p95_ms: data.tick_ms.p95,
      max_ms: data.tick_ms.max,
      over_50ms: data.ticks_over_50ms,
      cpu_ms_per_tick: profiles.reduce((n, p) => n + p.worker_cpu_ms_per_tick, 0),
      allocations_per_tick: profiles.reduce(
        (n, p) => n + Object.values(p.stages).reduce((s, x) => s + x.rust_allocations_per_tick, 0),
        0,
      ),
      inputs_acked: data.inputs_acked,
      peer_updates: data.peer_updates_applied,
      cross_island_subscriptions: data.cross_island_subscriptions,
    });
  }
}
const summary = partitions.map((threads) => {
  const set = samples.filter((s) => s.threads === threads);
  return {
    threads,
    p50_ms: median(set.map((s) => s.p50_ms)),
    p95_ms: median(set.map((s) => s.p95_ms)),
    worst_run_p95_ms: Math.max(...set.map((s) => s.p95_ms)),
    max_ms: Math.max(...set.map((s) => s.max_ms)),
    over_50ms: set.reduce((n, s) => n + s.over_50ms, 0),
    cpu_ms_per_tick: median(set.map((s) => s.cpu_ms_per_tick)),
  };
});
const lscpu =
  platform() === 'linux'
    ? spawnSync('lscpu', ['--json'], { ...opts, env: { ...env, LC_ALL: 'C' } })
    : null;
const topology =
  lscpu?.status === 0
    ? Object.fromEntries(
        JSON.parse(lscpu.stdout)
          .lscpu.filter(({ field }) =>
            [
              'Socket(s):',
              'Core(s) per socket:',
              'Thread(s) per core:',
              'NUMA node(s):',
              'L3 cache:',
            ].includes(field),
          )
          .map(({ field, data }) => [field.slice(0, -1), data]),
      )
    : undefined;
const report = {
  schema: 1,
  cpu: cpus()[0]?.model,
  os: `${platform()} ${release()}`,
  logical_cpus_available: logical,
  topology,
  rustc: execFileSync('rustc', ['-V'], opts).trim(),
  target_cpu_native: Boolean(values.native),
  workers,
  players,
  island_size: 16,
  ticks_per_run: ticks,
  runs,
  first_threads_with_headroom:
    summary.find((s) => s.worst_run_p95_ms <= 35 && s.over_50ms === 0)?.threads ?? null,
  summary,
  samples,
};
const output = resolve(root, values.output);
mkdirSync(dirname(output), { recursive: true });
writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
console.table(summary);
console.log(
  `Threads mit Reserve (jeder Lauf p95 ≤ 35 ms, alle Ticks ≤ 50 ms): ${report.first_threads_with_headroom ?? 'keine gemessene Stufe'}. ${output}`,
);
