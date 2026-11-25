import { setTimeout as sleep } from 'node:timers/promises';
import { decodeBinaryFrame } from '../client/src/binary-protocol.ts';

const levels = [3, 16, 32, 64, 128, 256];
const pulses = 16;
const intervalMs = 137; // Sample different phases of the 50 ms simulation tick.
const round = n => Number(n.toFixed(3));

export async function benchmarkPlayers({ servers, host, total, metrics, signal: parent }) {
  const failed = new AbortController();
  const signal = AbortSignal.any([parent, failed.signal]);
  const clients = [];
  let closing = false;
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
    socket.onerror = socket.onclose = () => {
      if (!closing) failed.abort(new Error(`Player connection lost on S${server.id}`));
    };
    return c;
  }
  const rows = [];
  try {
    for (const count of levels) {
      while (clients.length < count) clients.push(connect(servers[clients.length % servers.length]));
      const deadline = performance.now() + 5000;
      while (clients.some(c => !c.id || c.frames < 3)) {
        if (performance.now() > deadline) throw new Error(`Only ${clients.filter(c => c.id && c.frames >= 3).length}/${count} players ready`);
        await sleep(20, undefined, { signal });
      }
      const load = await metrics();
      if (new Set(clients.map(c => c.id)).size !== count ||
          load.reduce((n, m) => n + m.connected_clients, 0) !== count ||
          load.reduce((n, m) => n + m.entity_count, 0) !== total + count) {
        throw new Error(`Unexpected mesh population at ${count} players`);
      }
      const startFrames = clients.map(c => c.frames);
      const startBytes = clients.reduce((n,c) => n+c.bytes, 0);
      for (const c of clients) { c.pending.clear(); c.delays.length = 0; }
      const start = performance.now();
      for (let i = 0; i < pulses; i++) {
        for (const c of clients) {
          c.pending.set(++c.seq, performance.now());
          c.socket.send(JSON.stringify({ type: 'MoveInput', entity_id: c.id,
            direction: [i % 2 === 0 ? 1 : -1, 0, 0], seq: c.seq }));
        }
        await sleep(intervalMs, undefined, { signal });
      }
      const elapsed = (performance.now() - start) / 1000;
      const minHz = Math.min(...clients.map((c, i) => (c.frames - startFrames[i]) / elapsed));
      const bytes = clients.reduce((n,c) => n+c.bytes, 0) - startBytes;
      const drainUntil = performance.now() + 500;
      while (clients.some(c => c.pending.size) && performance.now() < drainUntil) {
        await sleep(20, undefined, { signal });
      }
      const delays = clients.flatMap(c => c.delays).sort((a,b) => a-b);
      const percentile = p => delays.length ? round(delays[Math.ceil(delays.length * p)-1]) : null;
      rows.push({ clients: count, sent: count*pulses, echoed: delays.length,
        min_client_echoes: Math.min(...clients.map(c => c.delays.length)),
        echo_ms: { p50: percentile(.50), p95: percentile(.95), p99: percentile(.99) },
        min_snapshot_hz: round(minHz), snapshot_MiB_s: round(bytes / elapsed / 1048576),
        measurement_seconds: round(elapsed) });
      signal.throwIfAborted();
    }
    return { servers: servers.length, npcs: total, pulses_per_client: pulses, interval_ms: intervalMs, rows };
  } finally {
    closing = true;
    for (const c of clients) c.socket.close();
  }
}
