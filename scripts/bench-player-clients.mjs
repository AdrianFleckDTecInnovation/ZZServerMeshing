import { once } from 'node:events';
import { setTimeout as sleep } from 'node:timers/promises';
import { isMainThread, parentPort, Worker, workerData } from 'node:worker_threads';
import { decodeBinaryFrame } from '../client/src/binary-protocol.ts';

const levels = [3, 16, 32, 64, 128, 256, 384, 512];
const pulses = 16;
const intervalMs = 137; // Sample different phases of the 50 ms simulation tick.
const rampTarget = { echo_p95_ms: 100, min_snapshot_hz: 18, min_server_hz: 18 };
const round = n => Number(n.toFixed(3));

function percentiles(samples) {
  samples.sort((a, b) => a - b);
  const at = p => samples.length ? round(samples[Math.ceil(samples.length * p) - 1]) : null;
  return { p50: at(.50), p95: at(.95), p99: at(.99) };
}

function runDriver({ servers, host, index, drivers }) {
  const failed = new AbortController();
  const signal = failed.signal;
  const clients = [];
  const traffic = { frames: 0, full: 0, records: 0, removals: 0, bytes: 0 };
  function move(client, x, timed) {
    const seq = ++client.seq;
    if (timed) client.pending.set(seq, performance.now());
    client.socket.send(JSON.stringify({ type: 'MoveInput', entity_id: client.id, direction: [x, 0, 0], seq }));
  }
  function connect(server) {
    const socket = new WebSocket(`ws://${host}:${server.ws}`);
    const c = { socket, id: null, seq: 0, frames: 0, pending: new Map(), delays: [] };
    socket.binaryType = 'arraybuffer';
    socket.onmessage = ({ data }) => {
      try {
        if (typeof data === 'string') {
          const msg = JSON.parse(data);
          if (msg.type === 'PlayerAssigned') c.id = msg.player_id;
          if (msg.type === 'Reconnect') throw new Error('Player reassignment during fixed-placement probe');
          return;
        }
        const frame = decodeBinaryFrame(data);
        if (!frame) throw new Error('Invalid browser snapshot');
        const ack = new Uint8Array(data.slice(0, 9));
        ack[0] = 3;
        socket.send(ack);
        c.frames++;
        traffic.frames++;
        traffic.full += Number(frame.type === 'full');
        traffic.records += frame.entities.length;
        traffic.removals += frame.removals.length;
        traffic.bytes += data.byteLength;
        const seq = frame.entities.find(e => e.id === c.id)?.last_input_seq;
        if (c.pending.has(seq)) {
          c.delays.push(performance.now() - c.pending.get(seq));
          c.pending.delete(seq);
        }
      } catch (error) { failed.abort(error); }
    };
    socket.onerror = socket.onclose = () => failed.abort(new Error(`Player connection lost on S${server.id}`));
    return c;
  }
  async function join(count) {
    for (let id = index + clients.length * drivers; id < count; id += drivers) {
      clients.push(connect(servers[id % servers.length]));
    }
    const deadline = performance.now() + 10000;
    while (clients.some(c => !c.id || c.frames < 3)) {
      if (performance.now() > deadline) throw new Error(`Players on load driver ${index} timed out`);
      await sleep(20, undefined, { signal });
    }
    return clients.map(c => c.id);
  }
  async function measure() {
    if (!clients.length) return null;
    const startFrames = clients.map(c => c.frames);
    const previous = { ...traffic };
    const timerDelay = [];
    for (const c of clients) {
      c.pending.clear();
      c.delays.length = 0;
    }
    const start = performance.now();
    let sent = 0;
    for (let i = 0; i < pulses; i++) {
      if (performance.now() - start >= 4000) break;
      for (const c of clients) move(c, i % 2 === 0 ? 1 : -1, true);
      sent += clients.length;
      const due = performance.now() + intervalMs;
      await sleep(intervalMs, undefined, { signal });
      timerDelay.push(Math.max(0, performance.now() - due));
    }
    const elapsed = (performance.now() - start) / 1000;
    const minHz = Math.min(...clients.map((c, i) => (c.frames - startFrames[i]) / elapsed));
    const received = Object.fromEntries(Object.keys(traffic).map(k => [k, traffic[k] - previous[k]]));
    const drainUntil = performance.now() + 500;
    while (clients.some(c => c.pending.size) && performance.now() < drainUntil) {
      await sleep(20, undefined, { signal });
    }
    return { sent, delays: clients.flatMap(c => c.delays), timerDelay, minHz,
      minEchoes: Math.min(...clients.map(c => c.delays.length)),
      received, snapshotMiBs: received.bytes / elapsed / 1048576, elapsed };
  }
  parentPort.on('message', async ({ count, warmup }) => {
    try {
      signal.throwIfAborted();
      if (warmup) {
        for (const client of clients) move(client, -1, false);
        parentPort.postMessage({ result: true });
        return;
      }
      const result = count === undefined ? await measure() : await join(count);
      signal.throwIfAborted();
      parentPort.postMessage({ result });
    } catch (error) { parentPort.postMessage({ error: error.message }); }
  });
}

if (!isMainThread) runDriver(workerData);

export async function benchmarkPlayers({ servers, host, total, players, metrics, signal: parent }) {
  const drivers = Number(process.env.MESH_BENCH_DRIVERS ?? 4);
  if (!Number.isInteger(drivers) || drivers < 1 || drivers > 16) {
    throw new Error('MESH_BENCH_DRIVERS must be an integer between 1 and 16');
  }
  const failed = new AbortController();
  const signal = AbortSignal.any([failed.signal, parent]);
  const workers = [];
  let closing = false;
  const rows = [];
  let stopped;
  async function dispatch(message, stageSignal = signal) {
    return Promise.all(workers.map(async worker => {
      const reply = once(worker, 'message', { signal: stageSignal });
      worker.postMessage(message);
      const [data] = await reply;
      if (data.error) throw new Error(data.error);
      return data.result;
    }));
  }
  try {
    for (let index = 0; index < drivers; index++) {
      const worker = new Worker(new URL(import.meta.url), { workerData: { host, servers, index, drivers } });
      workers.push(worker);
      worker.on('error', e => failed.abort(e));
      worker.on('exit', () => { if (!closing) failed.abort(new Error(`Load driver ${index} exited early`)); });
    }
    for (const count of (players ? [players] : levels)) {
      // A saturated workstation must still return the lower, completed load levels.
      const timeout = AbortSignal.timeout(20000);
      const stageSignal = AbortSignal.any([signal, timeout]);
      let samples, before, start;
      try {
        if (players) {
          for (let batch = 64; batch < count; batch += 64) await dispatch({ count: batch }, stageSignal);
        }
        const ids = (await dispatch({ count }, stageSignal)).flat();
        if (players) {
          await dispatch({ warmup: true }, stageSignal);
          // Three seconds covers a complete NPC direction-change period.
          await sleep(3200, undefined, { signal: stageSignal });
        }
        before = await metrics();
        start = performance.now();
        if (ids.length !== count || new Set(ids).size !== count ||
            before.reduce((n, m) => n + m.connected_clients, 0) !== count ||
            before.reduce((n, m) => n + m.entity_count, 0) !== total + count) {
          throw new Error(`Unexpected mesh population at ${count} players`);
        }
        samples = (await dispatch({}, stageSignal)).filter(Boolean);
      } catch (error) {
        if (!timeout.aborted || signal.aborted) throw error;
        stopped = { clients: count, reason: 'load drivers exceeded the 20 s stage deadline' };
        break;
      }
      const after = await metrics();
      const windowSeconds = (performance.now() - start) / 1000;
      if (after.reduce((n, m) => n + m.connected_clients, 0) !== count ||
          after.reduce((n, m) => n + m.entity_count, 0) !== total + count) {
        throw new Error(`Mesh population changed while measuring ${count} players`);
      }
      const delays = samples.flatMap(s => s.delays);
      const serverMetrics = after.map((m, i) => {
        const prior = before[i];
        const ticks = m.tick - prior.tick;
        if (m.session_id !== prior.session_id || ticks <= 0) throw new Error('Server restarted or made no tick progress');
        return { server: m.server_id, tick_hz: round(ticks / windowSeconds),
          ...(m.work && prior.work ? {
            tick_mean_ms: round((m.work.total_us - prior.work.total_us) / (1000 * ticks)),
            snapshot_mean_ms: round((m.work.client_snapshot_us - prior.work.client_snapshot_us) / (1000 * ticks)),
          } : {}),
          last_tick_shadows: m.shadow_count, last_tick_udp_bytes: m.udp_bytes_out,
          rolling_200_tick_p95_ms: Object.fromEntries(Object.entries(m.phases).map(([k, v]) => [k, v.p95])) };
      });
      const received = Object.fromEntries(Object.keys(samples[0].received).map(k =>
        [k, samples.reduce((sum, s) => sum + s.received[k], 0)]));
      const row = { clients: count, planned: count * pulses,
        sent: samples.reduce((n, s) => n + s.sent, 0), echoed: delays.length,
        min_client_echoes: Math.min(...samples.map(s => s.minEchoes)), echo_ms: percentiles(delays),
        min_snapshot_hz: round(Math.min(...samples.map(s => s.minHz))),
        snapshot_MiB_s: round(samples.reduce((n, s) => n + s.snapshotMiBs, 0)),
        received,
        measurement_seconds: round(Math.max(...samples.map(s => s.elapsed))),
        driver_timer_late_ms: percentiles(samples.flatMap(s => s.timerDelay)), server_metrics: serverMetrics };
      rows.push(row);
      if (row.echoed !== row.planned || row.echo_ms.p95 > rampTarget.echo_p95_ms ||
          row.min_snapshot_hz < rampTarget.min_snapshot_hz || serverMetrics.some(m => m.tick_hz < rampTarget.min_server_hz)) {
        stopped = { clients: count, reason: 'measured row missed the local ramp target' };
        break;
      }
    }
    return { servers: servers.length, npcs: total, fixed_players: players, moving_warmup_ms: players ? 3200 : 0,
      load_drivers: drivers, pulses_per_client: pulses,
      interval_ms: intervalMs, ramp_target: rampTarget, stopped, rows };
  } finally {
    closing = true;
    await Promise.all(workers.map(w => w.terminate()));
  }
}
