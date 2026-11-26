import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { cpus, availableParallelism, platform, release } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const root = fileURLToPath(new URL('../', import.meta.url));
const opt = { cwd: root, windowsHide: true, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 };
const { values } = parseArgs({ options: {
  measure: { type: 'boolean' }, report: { type: 'string' },
} });
const published = resolve(root, values.report ?? 'docs/benchmarks/mesh-v1.json');
const read = path => JSON.parse(readFileSync(path, 'utf8'));
const run = (command, args) => execFileSync(command, args.map(String), opt).trim();
const git = (...args) => run('git', args);
const code = ['Cargo.lock', 'crates/is-mesh/Cargo.toml', 'crates/zz-mesh/Cargo.toml',
  'examples/infinite-stars/Cargo.toml', ':(glob)crates/is-mesh/**/*.rs', 'crates/is-mesh/src/schema.sql',
  ':(glob)crates/zz-mesh/**/*.rs', ':(glob)examples/infinite-stars/**/*.rs',
  'scripts/bench-cores.mjs', 'scripts/bench-quick.mjs', 'scripts/verify-mesh-v1.mjs'];
// Git blob IDs are independent of checkout line endings and remain valid after a rebase.
const fingerprint = () => createHash('sha256').update(git('ls-files', '--stage', '--', ...code)).digest('hex');

function verifyMeasurements(r) {
  assert.equal(r.schema, 1);
  assert.equal(r.source_sha256, fingerprint(), 'Release evidence belongs to different source blobs');
  assert.match(r.revision, /^[a-f0-9]{40}$/);
  assert.equal(r.short.length, 3);
  for (const [n, sample] of [...r.short, r.stability].entries()) {
    assert.equal(sample.players, 2048);
    assert.equal(sample.workers, 16);
    assert.equal(sample.partitions, 4);
    assert.equal(sample.island_size, 16);
    assert.equal(sample.cpu_ids.length, 4);
    assert.equal(sample.ticks, n === 3 ? 1200 : 80);
    assert.equal(sample.paced_20hz, n === 3);
    assert.equal(sample.inputs_acked, sample.players * sample.ticks / 4);
    assert.equal(sample.peer_updates_applied, sample.players * sample.ticks);
    assert.equal(sample.cross_island_subscriptions, 0);
    assert.ok(sample.tick_ms.p95 <= 40 && sample.tick_ms.p99 <= 50 &&
      sample.ticks_over_50ms / sample.ticks <= 0.01, 'Tick or deadline budget exceeded in run ' + (n + 1));
    assert.ok(sample.resident_bytes > 0 && sample.resident_bytes <= 512 * 1024 ** 2);
    assert.ok(sample.process_cpu_seconds > 0);
  }
  assert.ok(r.stability.wall_seconds_with_setup >= 60);
  for (const [index, p] of r.subscribers.profiles.entries()) {
    assert.equal(p.players_per_island, [1,16][index]);
    assert.equal(p.confirmed_inputs, 1024);
    assert.equal(p.enter_records, 1024 * [1,17][index]);
    assert.equal(p.adapter_threads, 1);
  }
  assert.equal(r.subscribers.profiles.length, 2);
  assert.equal(r.private_updates.component_projections, 0);
  assert.equal(r.private_updates.peer_updates_applied, 0);
  assert.equal(r.private_updates.gameplay_player_updates, 10000 * 40);
  assert.equal(r.hot_group.expensive_group_players, 127);
  assert.equal(r.hot_group.effect_checks, 127 * 1024 * 40);
  assert.equal(r.durable.store, 'file');
  for (const name of ['strong','weak','topology']) {
    const scale = r.scaling[name];
    assert.deepEqual(scale.summary.map(s => s.threads), [1,2,4,8]);
    assert.equal(scale.samples.length, 12);
    for (const sample of scale.samples) {
      const expected = name === 'weak' ? 256 * sample.threads : 4096;
      assert.equal(sample.players, expected);
      assert.equal(sample.cpu_ids.length, sample.threads);
      assert.equal(sample.peer_updates, expected * 32);
      assert.equal(sample.inputs_acked, expected * 8);
      assert.equal(sample.cross_island_subscriptions, 0);
    }
    const throughput = s => s.players / s.p50_ms;
    const serial = scale.samples.filter(s => s.threads === 1);
    assert.ok([2,4,8].some(n => scale.samples.filter(s => s.threads === n)
      .every(s => throughput(s) > throughput(serial.find(ref => ref.run === s.run)))),
      'No consistently faster parallel level in ' + name);
  }
  const {separate, dense, merge_step, split_step} = r.density.matched_layouts[0];
  assert.ok(separate.end_islands > dense.end_islands);
  assert.ok(merge_step.end_islands < split_step.end_islands);
}

function checkTracker() {
  const doc = readFileSync(join(root,'docs/mesh-v1-abnahme.md'), 'utf8');
  const rows = doc.split(/\r?\n/).filter(line => /^\| M\d+ \|/.test(line));
  assert.equal(rows.length, 23, 'Expected M01 through M23 exactly once');
  rows.forEach((line, i) => {
    const [,id,status,criterion,api,proof,result] = line.split('|').map(s => s.trim());
    assert.equal(id, 'M' + String(i+1).padStart(2,'0'));
    assert.equal(status, 'BESTANDEN', id + ' still needs release evidence');
    assert.ok(criterion && api && proof && result && !result.includes('ausstehend'), id);
  });
}

function measure() {
  assert.equal(git('status', '--porcelain', '--', ...code), '', 'Commit the measured source first');
  const target = resolve(root, 'target');
  mkdirSync(target, { recursive:true });
  const dir = mkdtempSync(join(target,'mesh-v1-run-'));
  const build = run('cargo', ['build','--locked','--release','-p','infinite-stars','--bins',
    '--message-format=json-render-diagnostics']);
  const binaries = new Map(build.split('\n').map(line => JSON.parse(line)).filter(v=>v.executable)
    .map(v=>[v.target.name,v.executable]));
  const profile = (name,args) => {
    console.log(name, args.join(' '));
    const path = join(dir,name+'.json');
    run(binaries.get('mesh-profile'), [...args,'--output',path]);
    const { partition_profiles, stages, lane_samples_ms, ...sample } = read(path);
    return sample;
  };
  const workload = ['--players',2048,'--island-size',16,'--workers',16,'--partitions',4,'--cpu-budget',4];
  const r = {schema:1, revision:git('rev-parse','HEAD'), source_sha256:fingerprint(),
    utc:new Date().toISOString(), cpu:cpus()[0].model, logical_cpus:availableParallelism(),
    os:platform()+' '+release(), rust:run('rustc',['-V']), node:process.version,
    build:'cargo build --locked --release -p infinite-stars --bins', warmup_ticks:4,
    rustflags:process.env.CARGO_ENCODED_RUSTFLAGS ?? process.env.RUSTFLAGS ?? '',
    raw_directory:dir.split(/[\\/]/).pop()};
  if (process.platform === 'win32') {
    r.topology = JSON.parse(run('powershell.exe',['-NoProfile','-Command',
      'Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors | ConvertTo-Json -Compress']));
  } else if (process.platform === 'linux') {
    r.topology = JSON.parse(run('lscpu',['--json'])).lscpu;
  }
  r.short = [1,2,3].map(n=>profile('short-'+n,[...workload,'--ticks',80]));
  r.stability = profile('stability',[...workload,'--ticks',1200,'--pace-20hz']);
  r.private_updates = profile('private',['--players',10000,'--workers',32,'--partitions',4,
    '--ticks',40,'--active-percent',0,'--status-effects',128]);
  r.hot_group = profile('hot-group',['--players',128,'--island-size',128,'--workers',2,'--ticks',40,
    '--cpu-budget',1,'--status-effects',1024,'--hot-group']);
  r.durable = profile('durable',['--players',256,'--workers',2,'--ticks',40,'--cpu-budget',2,
    '--database',join(dir,'technical.sqlite')]);
  const sight = join(dir,'subscriber.json');
  run(binaries.get('sight-bench'),[1024,sight,'--push']);
  r.subscribers = read(sight);
  execFileSync(process.execPath, ['scripts/bench-quick.mjs','--profile','scaling','--ticks','32',
    '--runs','3','--workers','16','--cpu-budget','8','--output',dir], {...opt, stdio:'inherit'});
  r.scaling = Object.fromEntries(['strong','weak','topology'].map(name=>{
    const {workers,summary,samples} = read(join(dir,'scaling-'+name+'.json'));
    return [name,{workers,summary,samples}];
  }));
  r.density = read(join(dir,'scaling-dense.json'));
  const output = join(dir,'report.json');
  writeFileSync(output,JSON.stringify(r,null,2)+'\n');
  console.log(output);
  verifyMeasurements(r);
  if (values.report) writeFileSync(published, readFileSync(output));
}
if (values.measure) measure();
else {
  checkTracker();
  verifyMeasurements(read(published));
  console.log('M01–M23 und die gemessenen Freigabegrenzen sind bestätigt.');
}
