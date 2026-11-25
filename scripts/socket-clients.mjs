import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

export async function socketClients({ binary, servers, players, seconds, drivers, inputHz, cadence, sites, spacing, extent, logs, launch, metrics, signal }) {
  const startFile = join(logs, 'start-ms');
  const runs = Array.from({ length: drivers }, (_, i) => {
    const ready = join(logs, `driver-${i}.ready`);
    const output = join(logs, `driver-${i}.json`);
    const done = launch(`socket driver ${i}`, binary, ['--first-port', servers[0].ws, '--servers', servers.length,
      '--players', players, '--seconds', seconds, '--input-hz', inputHz, '--ready', ready, '--start-file', startFile,
      '--output', output, '--drivers', drivers, '--driver', i], { task: true, env: { TOKIO_WORKER_THREADS: '2' } });
    return { ready, output, done };
  });
  const deadline = performance.now() + 115000;
  while (runs.some(r => !existsSync(r.ready))) {
    if (performance.now() > deadline) throw new Error('Timed out waiting for all socket clients');
    await sleep(25, undefined, { signal });
  }
  const start = Date.now() + 1000;
  writeFileSync(startFile, String(start));
  await sleep(Math.max(1, start - Date.now()), undefined, { signal });
  const begin = performance.now();
  const first = await metrics();
  await sleep(Math.max(1, seconds * 1000 - (performance.now() - begin)), undefined, { signal });
  const last = await metrics();
  const elapsed = (performance.now() - begin) / 1000;
  const finishDeadline = AbortSignal.any([signal, AbortSignal.timeout(30000)]);
  await Promise.race([
    Promise.all(runs.map(r => r.done)),
    new Promise((_, reject) => finishDeadline.addEventListener('abort', () => reject(finishDeadline.reason), { once: true })),
  ]);
  signal.throwIfAborted();
  const rows = runs.map(r => JSON.parse(readFileSync(r.output, 'utf8')));
  const sum = k => rows.reduce((n, row) => n + row[k], 0);
  const sorted = k => rows.flatMap(row => row[k]).sort((a, b) => a - b);
  const rates = sorted('frame_rates'), delays = sorted('input_us');
  const q = (xs, p) => xs[Math.ceil(xs.length * p / 100) - 1] ?? null;
  const result = {
    schema: 1, players: sum('players'), sim_servers: servers.length, drivers, measurement_seconds: seconds,
    target_snapshot_hz: 20 / cadence, target_input_hz: inputHz,
    transport: rows[0].transport, target_os: rows[0].target_os, islands: sites, island_spacing: spacing, world_half_extent: extent,
    expected_inputs: sum('expected_inputs'), sent_inputs: sum('sent_inputs'), confirmed_inputs: delays.length,
    players_with_peer: sum('players_with_peer'), players_with_confirmed_motion: sum('players_with_confirmed_motion'),
    bytes_received: sum('bytes_received'), skipped_snapshot_ticks: sum('skipped_snapshot_ticks'),
    receive_hz_min: rates[0], receive_hz_p05: q(rates, 5), receive_hz_p50: q(rates, 50),
    input_p50_us: q(delays, 50), input_p95_us: q(delays, 95), input_p99_us: q(delays, 99),
    authoritative_entities: first.reduce((n, s) => n + s.entity_count, 0),
    connected_clients: first.reduce((n, s) => n + s.connected_clients, 0),
    driver_cpu_cores: sum('driver_cpu_cores'),
    server_cpu_cores: last.map((s,i)=>(s.process_cpu_ns-first[i].process_cpu_ns) / (elapsed * 1e9)),
    schedule_cpu_cores: last.map((s,i)=>(s.work.schedule_cpu_ns-first[i].work.schedule_cpu_ns) / (elapsed * 1e9)),
    udp_queued_mbit_s: last.reduce((n,s,i)=>n+s.work.udp_queued_bytes-first[i].work.udp_queued_bytes,0) * 8 / (1e6 * elapsed),
    client_payload_mbit_s: sum('bytes_received') * 8 / (1e6 * seconds),
    snapshot_queue_full: last.reduce((n, s, i) => n + s.work.snapshot_queue_full - first[i].work.snapshot_queue_full, 0),
    server_hz: last.map((s, i) => (s.tick - first[i].tick) / elapsed),
    server_phase_p95_ms: last.map(s => Object.fromEntries(Object.entries(s.phases).map(([k, v]) => [k, v.p95]))),
  };
  result.met_interactive_budget = result.authoritative_entities === players && result.connected_clients === players && delays.length === result.expected_inputs
    && result.players_with_peer === players && result.players_with_confirmed_motion === players
    && result.receive_hz_min >= 18 / cadence && Math.min(...result.server_hz) >= 18 && result.input_p95_us <= 150000;
  writeFileSync(join(logs, 'socket-clients.json'), JSON.stringify(result, null, 2));
  return result;
}
