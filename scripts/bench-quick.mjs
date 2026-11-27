import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { arch, cpus, platform, release } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({options:{
  profile:{type:'string',default:'quick'}, players:{type:'string'}, workers:{type:'string'},
  ticks:{type:'string'}, runs:{type:'string'}, 'cpu-budget':{type:'string'},
  driver:{type:'string'}, scenario:{type:'string'}, output:{type:'string'}
}});
const target = join(root, 'target');
mkdirSync(target, { recursive: true });
const temp = mkdtempSync(join(target, 'quick-profile-'));
const env = { COORD_WS_PORT: '18710', COORD_HTTP_PORT: '18711', SIM_WS_BASE: '18717',
  SIM_METRICS_BASE: '18810', SIM_UDP_BASE: '18910', ...process.env };
const opts = { cwd: root, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 32 * 1024 * 1024 };
const start = performance.now();
try {
  const run = (file,args) => execFileSync(file,args,{...opts,stdio:'inherit'});
  const cargo = (...args) => run('cargo',args);
  if (values.profile === 'production') {
    if (!values.driver || !values.scenario || !values.output) throw Error('Production requires --driver (external ZZSession load driver), --scenario and --output');
    const scenario = JSON.parse(readFileSync(resolve(values.scenario),'utf8'));
    if (!Number.isInteger(scenario.authenticated_players) || scenario.authenticated_players < 10000 ||
        !Array.isArray(scenario.physical_hosts) || new Set(scenario.physical_hosts).size < 2 ||
        !['reconnect','worker-failure','migration','dense','separated'].every(s=>scenario.workloads?.includes(s)))
      throw Error('Production scenario requires 10,000 authenticated players, distinct physical hosts and all five workloads');
    const result = join(temp,'production.json');
    run(resolve(values.driver),['--scenario',resolve(values.scenario),'--output',result]);
    const data=JSON.parse(readFileSync(result,'utf8'));
    if (data.transport !== 'Aeronet' || !Number.isSafeInteger(data.authenticated_players) || data.authenticated_players < scenario.authenticated_players ||
        data.passed !== true || !['tick_p50_ms','tick_p95_ms','tick_p99_ms','tick_max_ms','deadline_misses','cpu_cores_used',
          'resident_bytes','subscriber_bytes','queue_peak','queue_oldest_ms','wire_bytes_per_second',
          'serialization_ms','migration_ms','recovery_ms','slowest_client_hz','duration_seconds'].every(k=>Number.isFinite(data[k]) && data[k]>=0) ||
        !(data.tick_p50_ms<=data.tick_p95_ms && data.tick_p95_ms<=data.tick_p99_ms && data.tick_p99_ms<=data.tick_max_ms) ||
        data.duration_seconds<=0 || data.slowest_client_hz<=0 || !Number.isSafeInteger(data.confirmed_inputs) ||
        data.confirmed_inputs !== data.scheduled_inputs || data.confirmed_inputs <= 0) throw Error('External production evidence is incomplete or failed');
    const destination=resolve(values.output);
    mkdirSync(dirname(destination),{recursive:true});
    writeFileSync(destination,JSON.stringify({scenario,result:data},null,2)+'\n');
  } else if (values.profile === 'scaling') {
    const output=resolve(values.output || target);mkdirSync(output,{recursive:true});
    const largeWorlds=Number(values.workers||16);
    for (const [mode,worlds,name] of [['strong',largeWorlds,'strong'],['weak',largeWorlds,'weak'],['strong',Math.min(256,2*largeWorlds),'topology']]) {
      const args=['scripts/bench-cores.mjs','--mode',mode,'--players',values.players || (mode==='strong'?'4096':'256'),
        '--workers',String(worlds),'--ticks',values.ticks || '24','--runs',values.runs || '3',
        '--output',join(output,`scaling-${name}.json`)];
      if (values['cpu-budget']) args.push('--cpu-budget',values['cpu-budget']);
      run(process.execPath,args);
    }
    cargo('run','--release','--locked','-p','infinite-stars','--bin','cluster-bench','--', join(output,'scaling-dense.json'),'--crowd','256');
  } else if (values.profile === 'development') {
    const out=resolve(values.output || target);
    mkdirSync(out,{recursive:true});
    cargo('test','--locked','-p','is-mesh','--test','external_integration');
    cargo('run','--locked','-p','is-mesh','--example','session_game');
    cargo('run','--release','--locked','-p','infinite-stars','--bin','sight-bench','--',
      values.players || '1024',join(out,'subscriber-development.json'),'--push');
    cargo('run','--release','--locked','-p','infinite-stars','--bin','mesh-profile','--','--players',
      values.players || '1024','--workers',values.workers || '8','--partitions','2','--ticks',values.ticks || '40',
      '--status-effects','128','--output',join(out,'tick-development.json'));
  } else if (values.profile === 'quick') {
    cargo('test','--locked','-p','is-mesh','--test','external_integration');
    cargo('run','--locked','-p','is-mesh','--example','session_game');
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
  } else throw Error('Expected quick, development, scaling or production');
} finally {
  rmSync(temp, { recursive: true, force: true });
}
