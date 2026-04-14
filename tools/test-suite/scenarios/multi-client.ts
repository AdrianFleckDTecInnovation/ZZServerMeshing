import type { Scenario, ScenarioResult, TestContext, EntityState, WorldSnapshot } from '../types.js';
import WebSocket from 'ws';

// Minimal binary decoder for the second client connection
const ENTITY_SIZE = 47;
const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push(i.toString(16).padStart(2, '0'));

function uuidFromBuf(buf: Buffer, off: number): string {
  return HEX[buf[off]]+HEX[buf[off+1]]+HEX[buf[off+2]]+HEX[buf[off+3]]+'-'+
    HEX[buf[off+4]]+HEX[buf[off+5]]+'-'+HEX[buf[off+6]]+HEX[buf[off+7]]+'-'+
    HEX[buf[off+8]]+HEX[buf[off+9]]+'-'+HEX[buf[off+10]]+HEX[buf[off+11]]+
    HEX[buf[off+12]]+HEX[buf[off+13]]+HEX[buf[off+14]]+HEX[buf[off+15]];
}

function decodeSnapshot(data: Buffer): { tick: number; entities: EntityState[] } | null {
  if (data.length < 27 || data[0] !== 0x01) return null;
  const tick = data.readUInt32LE(1) + data.readUInt32LE(5) * 0x100000000;
  const count = data.readUInt32LE(9);
  const entities: EntityState[] = [];
  let off = 27;
  for (let i = 0; i < count && off + ENTITY_SIZE <= data.length; i++) {
    entities.push({
      id: uuidFromBuf(data, off),
      position: [data.readFloatLE(off+16), data.readFloatLE(off+20), data.readFloatLE(off+24)],
      velocity: [data.readFloatLE(off+28), data.readFloatLE(off+32), data.readFloatLE(off+36)],
      entity_type: data[off+40] === 1 ? 'Player' : 'Npc',
      authority_server: data.readUInt16LE(off+41),
      last_input_seq: data.readUInt32LE(off+43),
    });
    off += ENTITY_SIZE;
  }
  return { tick, entities };
}

function connectAndListen(url: string): Promise<{
  ws: WebSocket;
  getSnapshot: () => { tick: number; entities: EntityState[] } | null;
  waitReady: () => Promise<void>;
}> {
  const opts = url.startsWith('wss') ? { rejectUnauthorized: false } : {};
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, opts);
    let latest: { tick: number; entities: EntityState[] } | null = null;
    let readyResolve: (() => void) | null = null;

    ws.on('open', () => {
      resolve({
        ws,
        getSnapshot: () => latest,
        waitReady: () => new Promise<void>(r => {
          if (latest) r();
          else readyResolve = r;
        }),
      });
    });
    ws.on('error', reject);
    ws.on('message', (data: Buffer | string, isBinary: boolean) => {
      if (isBinary || Buffer.isBuffer(data)) {
        const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
        const snap = decodeSnapshot(buf);
        if (snap) {
          latest = snap;
          if (readyResolve) { readyResolve(); readyResolve = null; }
        }
      }
    });
  });
}

export const multiClient: Scenario = {
  name: 'multi-client',
  description: 'Two clients on different servers see a consistent world view',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    if (ctx.serverCount < 2) {
      return { name: 'multi-client', status: 'fail', summary: 'Need at least 2 servers', details, durationMs: 0 };
    }

    // Connect client 1 via the test context (already connected)
    // Connect client 2 to a different server via coordinator assignment
    const url1 = ctx.serverUrl;
    ctx.log('Requesting second server assignment from coordinator...');

    // Get second server URL from coordinator
    const wsProto = url1.startsWith('wss') ? 'wss' : 'ws';
    let url2: string;
    try {
      const coordUrl = process.env.COORDINATOR_URL ||
        url1.replace(/^wss?/, url1.startsWith('wss') ? 'https' : 'http').replace(/:\d+$/, ':7871');
      const resp = await fetch(`${coordUrl}/servers`);
      const data: { servers: { server_id: number; host: string; ws_port: number }[] } = await resp.json();
      // Build ws/wss URLs and find one different from client 1
      const serverUrls = data.servers.map(s => `${wsProto}://${s.host}:${s.ws_port}`);
      const other = serverUrls.find(u => u !== url1);
      if (other) {
        url2 = other;
      } else {
        return { name: 'multi-client', status: 'fail', summary: 'Only 1 server available', details, durationMs: 0 };
      }
    } catch {
      const port1 = parseInt(url1.match(/:(\d+)$/)?.[1] ?? '7878');
      const port2 = port1 === 7878 ? 7879 : 7878;
      url2 = url1.replace(`:${port1}`, `:${port2}`);
    }

    ctx.log(`Client 1 on ${url1}, Client 2 on ${url2}`);
    details.push(`Client 1: ${url1}`);
    details.push(`Client 2: ${url2}`);

    let client2;
    try {
      client2 = await connectAndListen(url2);
    } catch (e: any) {
      return { name: 'multi-client', status: 'fail', summary: `Cannot connect client 2 to ${url2}: ${e.message}`, details, durationMs: 0 };
    }

    // Wait for both to receive snapshots
    await client2.waitReady();
    await ctx.sleep(1000);

    // Compare entity sets over 5 samples
    let matchCount = 0;
    let totalSamples = 0;
    let maxDiff = 0;

    for (let i = 0; i < 5; i++) {
      await ctx.sleep(200);
      const snap1 = ctx.getLatestSnapshot();
      const snap2 = client2.getSnapshot();
      if (!snap1 || !snap2) continue;
      totalSamples++;

      const ids1 = new Set(snap1.entities.map(e => e.id));
      const ids2 = new Set(snap2.entities.map(e => e.id));

      // Find entities unique to each client (excluding their own player entities which are different)
      const npcs1 = new Set(snap1.entities.filter(e => e.entity_type === 'Npc').map(e => e.id));
      const npcs2 = new Set(snap2.entities.filter(e => e.entity_type === 'Npc').map(e => e.id));

      let missing = 0;
      for (const id of npcs1) { if (!npcs2.has(id)) missing++; }
      for (const id of npcs2) { if (!npcs1.has(id)) missing++; }

      if (missing > maxDiff) maxDiff = missing;
      if (missing <= 2) matchCount++; // allow small timing differences

      ctx.log(`Sample ${i}: client1=${snap1.entities.length} client2=${snap2.entities.length} NPC diff=${missing} ticks=${snap1.tick}/${snap2.tick}`);
      details.push(`Sample ${i}: c1=${snap1.entities.length} c2=${snap2.entities.length} NPC_diff=${missing}`);
    }

    client2.ws.close();

    if (totalSamples === 0) {
      return { name: 'multi-client', status: 'fail', summary: 'No snapshots received', details, durationMs: 0 };
    }

    const consistency = (matchCount / totalSamples) * 100;
    details.push(`Consistency: ${matchCount}/${totalSamples} samples matched (max NPC diff: ${maxDiff})`);

    return {
      name: 'multi-client',
      status: consistency >= 60 ? 'pass' : consistency >= 40 ? 'warn' : 'fail',
      summary: `${consistency.toFixed(0)}% consistent across 2 clients, max NPC diff=${maxDiff}`,
      details,
      durationMs: 0,
    };
  },
};
