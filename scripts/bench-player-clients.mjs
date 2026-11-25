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
  function connect(server) {
    const socket = new WebSocket(`ws://${host}:${server.ws}`);
    const c = { socket, id: null, seq: 0, frames: 0, bytes: 0, pending: new Map(), delays: [] };
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
        c.bytes += data.byteLength;
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
    const startBytes = clients.reduce((n, c) => n + c.bytes, 0);
    const timerDelay = [];
    for (const c of clients) {
      c.pending.clear();
      c.delays.length = 0;
    }
    const start = performance.now();
    let sent = 0;
    for (let i = 0; i < pulses; i++) {
      if (performance.now() - start >= 4000) break;
      for (const c of clients) {
        c.pending.set(++c.seq, performance.now());
        c.socket.send(JSON.stringify({ type: 'MoveInput', entity_id: c.id,
          direction: [i % 2 === 0 ? 1 : -1, 0, 0], seq: c.seq }));
      }
      sent += clients.length;
      const due = performance.now() + intervalMs;
      await sleep(intervalMs, undefined, { signal });
      timerDelay.push(Math.max(0, performance.now() - due));
    }
    const elapsed = (performance.now() - start) / 1000;
    const minHz = Math.min(...clients.map((c, i) => (c.frames - startFrames[i]) / elapsed));
    const bytes = clients.reduce((n, c) => n + c.bytes, 0) - startBytes;
    const drainUntil = performance.now() + 500;
    while (clients.some(c => c.pending.size) && performance.now() < drainUntil) {
      await sleep(20, undefined, { signal });
    }
    return { sent, delays: clients.flatMap(c => c.delays), timerDelay, minHz,
      minEchoes: Math.min(...clients.map(c => c.delays.length)),
      snapshotMiBs: bytes / elapsed / 1048576, elapsed };
  }
  parentPort.on('message', async ({ count }) => {
    try {
      signal.throwIfAborted();
      const result = count === undefined ? await measure() : await join(count);
      signal.throwIfAborted();
      parentPort.postMessage({ result });
    } catch (error) { parentPort.postMessage({ error: error.message }); }
  });
}

if (!isMainThread) runDriver(workerData);

export async function benchmarkPlayers({ servers, host, total, metrics, signal: parent }) {
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
    for (const count of levels) {
      // A saturated workstation must still return the lower, completed load levels.
      const timeout = AbortSignal.timeout(20000);
      const stageSignal = AbortSignal.any([signal, timeout]);
      let samples, before, start;
      try {
        const ids = (await dispatch({ count }, stageSignal)).flat();
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
      const delays = samples.flatMap(s => s.delays);
      const serverMetrics = after.map((m, i) => ({
        server: m.server_id, tick_hz: round((m.tick - before[i].tick) / windowSeconds),
        rolling_200_tick_p95_ms: Object.fromEntries(Object.entries(m.phases).map(([k, v]) => [k, v.p95])),
      }));
      const row = { clients: count, planned: count * pulses,
        sent: samples.reduce((n, s) => n + s.sent, 0), echoed: delays.length,
        min_client_echoes: Math.min(...samples.map(s => s.minEchoes)), echo_ms: percentiles(delays),
        min_snapshot_hz: round(Math.min(...samples.map(s => s.minHz))),
        snapshot_MiB_s: round(samples.reduce((n, s) => n + s.snapshotMiBs, 0)),
        measurement_seconds: round(Math.max(...samples.map(s => s.elapsed))),
        driver_timer_late_ms: percentiles(samples.flatMap(s => s.timerDelay)), server_metrics: serverMetrics };
      rows.push(row);
      if (row.echoed !== row.planned || row.echo_ms.p95 > rampTarget.echo_p95_ms ||
          row.min_snapshot_hz < rampTarget.min_snapshot_hz || serverMetrics.some(m => m.tick_hz < rampTarget.min_server_hz)) {
        stopped = { clients: count, reason: 'measured row missed the local ramp target' };
        break;
      }
    }
    return { servers: servers.length, npcs: total, load_drivers: drivers, pulses_per_client: pulses,
      interval_ms: intervalMs, ramp_target: rampTarget, stopped, rows };
  } finally {
    closing = true;
    await Promise.all(workers.map(w => w.terminate()));
  }
}
