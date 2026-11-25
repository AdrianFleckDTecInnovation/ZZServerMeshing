import { arch, cpus, platform, release } from 'node:os';
import { setTimeout as sleep } from 'node:timers/promises';

// One browser-protocol client per sim. An ACK carries the same little-endian
// tick bytes as its snapshot. Counting only full/delta frames excludes JSON control traffic.
export async function benchmark({ servers, host, total, seconds, metrics, signal: parent }) {
  const failed = new AbortController();
  const signal = AbortSignal.any([parent, failed.signal]);
  let closing = false;
  const clients = [];
  try {
    for (const server of servers) {
      const socket = new WebSocket(`ws://${host}:${server.ws}`);
      const client = { socket, frames: 0, bytes: 0, full: 0, records: 0, removals: 0 };
      clients.push(client);
      socket.binaryType = 'arraybuffer';
      socket.onmessage = ({ data }) => {
        if (typeof data === 'string') return;
        const bytes = new Uint8Array(data);
        const full = bytes[0] === 1;
        const header = full ? 27 : 31;
        if (bytes.length < header || (!full && bytes[0] !== 2)) {
          failed.abort(new Error(`S${server.id} returned an invalid snapshot`));
          return;
        }
        // crates/sim-server/src/encoding.rs encodes 47 bytes per state and 16 per removed UUID.
        const view = new DataView(data);
        const records = view.getUint32(9, true);
        const removals = full ? 0 : view.getUint32(13, true);
        if (bytes.length !== header + 47 * records + 16 * removals) {
          failed.abort(new Error(`S${server.id} returned inconsistent snapshot counts`));
          return;
        }
        const ack = bytes.slice(0, 9);
        ack[0] = 3;
        socket.send(ack);
        client.frames++;
        client.bytes += bytes.length;
        client.full += Number(full);
        client.records += records;
        client.removals += removals;
      };
      socket.onerror = socket.onclose = () => {
        if (!closing) failed.abort(new Error(`Benchmark client disconnected from S${server.id}`));
      };
    }
    // Wait for the full/delta path to be active before starting the warm-up.
    const deadline = performance.now() + 15000;
    while (clients.some(c => c.frames === 0)) {
      if (performance.now() > deadline) throw new Error('Timed out waiting for snapshots from every server');
      await sleep(50, undefined, { signal });
    }
    const warmupSeconds = 5;
    await sleep(warmupSeconds * 1000, undefined, { signal });
    const checkLoad = rows => {
      const entities = rows.reduce((sum, m) => sum + m.entity_count, 0);
      if (entities !== total + clients.length || !rows.every((m, i) =>
        m.server_id === servers[i].id && m.connected_clients === 1 && m.peer_count === servers.length - 1)) {
        throw new Error(`Unexpected mesh load: ${entities} entities, expected ${total} NPCs + ${clients.length} players`);
      }
    };
    const before = await metrics();
    checkLoad(before);
    const start = performance.now();
    const counters = clients.map(({ frames, bytes, full, records, removals }) => ({ frames, bytes, full, records, removals }));
    await sleep(seconds * 1000, undefined, { signal });
    const after = await metrics();
    const elapsed = (performance.now() - start) / 1000;
    signal.throwIfAborted();
    checkLoad(after);
    const rounded = n => Number(n.toFixed(3));
    const results = after.map((m, i) => {
      const ticks = m.tick - before[i].tick;
      const frames = clients[i].frames - counters[i].frames;
      if (ticks <= 0 || frames <= 0) throw new Error(`No tick or snapshot progress on S${m.server_id}`);
      return {
        server: m.server_id, authoritative: m.entity_count, shadows: m.shadow_count, peers: m.peer_count,
        ticks, snapshots: frames, tick_hz: rounded(ticks / elapsed), snapshot_hz: rounded(frames / elapsed),
        full_snapshots: clients[i].full - counters[i].full, entity_records: clients[i].records - counters[i].records,
        removal_ids: clients[i].removals - counters[i].removals, snapshot_bytes: clients[i].bytes - counters[i].bytes,
        snapshot_KiB_s: rounded((clients[i].bytes - counters[i].bytes) / 1024 / elapsed),
        tick_p50_ms: rounded(m.phases.total_ms.p50), tick_p95_ms: rounded(m.phases.total_ms.p95),
        tick_p99_ms: rounded(m.phases.total_ms.p99), p95_budget_pct: rounded(m.tick_budget_pct),
        phase_p95_ms: Object.fromEntries(Object.entries(m.phases).map(([name, p]) => [name, rounded(p.p95)])),
        phase_p99_ms: Object.fromEntries(Object.entries(m.phases).map(([name, p]) => [name, rounded(p.p99)])),
      };
    });
    return {
      platform: `${platform()} ${release()} ${arch()}`, cpu: cpus()[0]?.model, node: process.version,
      profile: 'release', npcs: total, clients: clients.length, warmup_seconds: warmupSeconds,
      measured_seconds: rounded(elapsed), servers: results,
    };
  } catch (error) {
    throw signal.aborted ? signal.reason : error;
  } finally {
    closing = true;
    for (const { socket } of clients) socket.close();
  }
}
