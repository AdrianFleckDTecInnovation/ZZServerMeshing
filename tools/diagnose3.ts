/**
 * Per-frame jump counter — find WHICH frames have jumps.
 */
import WebSocket from 'ws';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const ENTITY_SIZE = 43;
const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push(i.toString(16).padStart(2, '0'));

function uuidFromBytes(buf: Buffer, offset: number): string {
  return (
    HEX[buf[offset]] + HEX[buf[offset+1]] + HEX[buf[offset+2]] + HEX[buf[offset+3]] + '-' +
    HEX[buf[offset+4]] + HEX[buf[offset+5]] + '-' +
    HEX[buf[offset+6]] + HEX[buf[offset+7]] + '-' +
    HEX[buf[offset+8]] + HEX[buf[offset+9]] + '-' +
    HEX[buf[offset+10]] + HEX[buf[offset+11]] + HEX[buf[offset+12]] + HEX[buf[offset+13]] + HEX[buf[offset+14]] + HEX[buf[offset+15]]
  );
}

interface E { id: string; x: number; z: number; server: number; }

const ws = new WebSocket(GW_URL);
let prev: Map<string, E> | null = null;
let frame = 0;
let prevTick = 0;

ws.on('message', (data: Buffer | string, isBinary: boolean) => {
  if (frame > 60) return;
  if (!isBinary) { frame++; return; }

  const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
  if (buf[0] !== 0x01) { frame++; return; }

  const tick = buf.readUInt32LE(1);
  const count = buf.readUInt32LE(9);
  const entities = new Map<string, E>();
  let offset = 27;
  for (let i = 0; i < count; i++) {
    const id = uuidFromBytes(buf, offset);
    entities.set(id, {
      id,
      x: buf.readFloatLE(offset + 16),
      z: buf.readFloatLE(offset + 24),
      server: buf.readUInt16LE(offset + 41),
    });
    offset += ENTITY_SIZE;
  }

  if (prev) {
    let jumps = 0;
    let checks = 0;
    const jumpExamples: string[] = [];
    for (const [id, e] of entities) {
      const p = prev.get(id);
      if (!p) continue;
      checks++;
      const dist = Math.sqrt((e.x - p.x) ** 2 + (e.z - p.z) ** 2);
      if (dist > 10) {
        jumps++;
        if (jumpExamples.length < 2) {
          jumpExamples.push(`${id.substring(0,8)} S${p.server}→S${e.server} (${p.x.toFixed(0)},${p.z.toFixed(0)})→(${e.x.toFixed(0)},${e.z.toFixed(0)}) dist=${dist.toFixed(0)}`);
        }
      }
    }
    const tickDelta = tick - prevTick;
    console.log(`frame=${frame} tick=${tick} Δtick=${tickDelta} entities=${count} jumps=${jumps}/${checks}${jumps > 0 ? ' !!!' : ''}`);
    for (const ex of jumpExamples) console.log(`  ${ex}`);
  }

  prev = entities;
  prevTick = tick;
  frame++;

  if (frame > 60) { ws.close(); process.exit(0); }
});

ws.on('error', (e) => { console.error(e.message); process.exit(1); });
setTimeout(() => { ws.close(); process.exit(0); }, 15000);
