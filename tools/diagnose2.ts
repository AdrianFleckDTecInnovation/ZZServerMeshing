/**
 * Focused diagnosis: dump raw entity data from consecutive frames
 * to find the position jump root cause.
 */
import WebSocket from 'ws';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const ENTITY_SIZE = 43;
const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push(i.toString(16).padStart(2, '0'));

function uuidFromBytes(buf: Buffer, offset: number): string {
  return (
    HEX[buf[offset]]     + HEX[buf[offset+1]] +
    HEX[buf[offset+2]]   + HEX[buf[offset+3]] + '-' +
    HEX[buf[offset+4]]   + HEX[buf[offset+5]] + '-' +
    HEX[buf[offset+6]]   + HEX[buf[offset+7]] + '-' +
    HEX[buf[offset+8]]   + HEX[buf[offset+9]] + '-' +
    HEX[buf[offset+10]]  + HEX[buf[offset+11]] +
    HEX[buf[offset+12]]  + HEX[buf[offset+13]] +
    HEX[buf[offset+14]]  + HEX[buf[offset+15]]
  );
}

interface Entity { id: string; x: number; z: number; server: number; }

const ws = new WebSocket(GW_URL);
let prevEntities: Map<string, Entity> | null = null;
let frameCount = 0;

ws.on('message', (data: Buffer | string, isBinary: boolean) => {
  if (!isBinary || frameCount > 5) return;

  const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
  if (buf[0] !== 0x01) return; // only full snapshots

  const tick = buf.readUInt32LE(1);
  const entityCount = buf.readUInt32LE(9);

  const entities = new Map<string, Entity>();
  let offset = 27;
  for (let i = 0; i < entityCount; i++) {
    const id = uuidFromBytes(buf, offset);
    const x = buf.readFloatLE(offset + 16);
    const z = buf.readFloatLE(offset + 24);
    const server = buf.readUInt16LE(offset + 41);
    entities.set(id, { id, x, z, server });
    offset += ENTITY_SIZE;
  }

  if (prevEntities) {
    // Find jumps
    let jumps = 0;
    for (const [id, e] of entities) {
      const prev = prevEntities.get(id);
      if (!prev) continue;
      const dx = e.x - prev.x;
      const dz = e.z - prev.z;
      const dist = Math.sqrt(dx * dx + dz * dz);
      if (dist > 10) {
        jumps++;
        if (jumps <= 3) {
          console.log(`JUMP at tick ${tick}: ${id.substring(0,8)}`);
          console.log(`  prev: x=${prev.x.toFixed(2)} z=${prev.z.toFixed(2)} S${prev.server}`);
          console.log(`  curr: x=${e.x.toFixed(2)} z=${e.z.toFixed(2)} S${e.server}`);
          console.log(`  dist: ${dist.toFixed(1)}`);
        }
      }
    }
    console.log(`Frame ${frameCount} tick=${tick}: ${entityCount} entities, ${jumps} jumps\n`);
  } else {
    console.log(`Frame ${frameCount} tick=${tick}: ${entityCount} entities (baseline)\n`);

    // Dump first 3 entities for inspection
    offset = 27;
    for (let i = 0; i < Math.min(3, entityCount); i++) {
      const rawHex = buf.subarray(offset, offset + ENTITY_SIZE).toString('hex');
      const id = uuidFromBytes(buf, offset);
      const x = buf.readFloatLE(offset + 16);
      const y = buf.readFloatLE(offset + 20);
      const z = buf.readFloatLE(offset + 24);
      const server = buf.readUInt16LE(offset + 41);
      console.log(`  Entity ${i}: ${id} x=${x.toFixed(2)} y=${y.toFixed(2)} z=${z.toFixed(2)} S${server}`);
      console.log(`    raw: ${rawHex}`);
      offset += ENTITY_SIZE;
    }
    console.log('');
  }

  prevEntities = entities;
  frameCount++;

  if (frameCount > 5) {
    ws.close();
    process.exit(0);
  }
});

ws.on('error', (e) => { console.error('Error:', e.message); process.exit(1); });
setTimeout(() => { console.log('Timeout'); process.exit(1); }, 10000);
