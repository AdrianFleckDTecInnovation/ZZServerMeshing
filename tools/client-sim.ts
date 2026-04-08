/**
 * Headless client simulator — does EXACTLY what the browser client does:
 * receives binary frames, decodes them, applies to a WorldView simulation,
 * runs interpolation, and detects flicker + warping + position jumps.
 *
 * This catches bugs that server-side diagnostics miss.
 */
import WebSocket from 'ws';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const DURATION_MS = 10000;
const ENTITY_SIZE = 43;

const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push(i.toString(16).padStart(2, '0'));
function uuidFromBytes(buf: Buffer, off: number): string {
  return HEX[buf[off]]+HEX[buf[off+1]]+HEX[buf[off+2]]+HEX[buf[off+3]]+'-'+
    HEX[buf[off+4]]+HEX[buf[off+5]]+'-'+HEX[buf[off+6]]+HEX[buf[off+7]]+'-'+
    HEX[buf[off+8]]+HEX[buf[off+9]]+'-'+
    HEX[buf[off+10]]+HEX[buf[off+11]]+HEX[buf[off+12]]+HEX[buf[off+13]]+HEX[buf[off+14]]+HEX[buf[off+15]];
}

// ── Simulated WorldView (mirrors client/src/world-view.ts EXACTLY) ──

interface Entity {
  id: string;
  position: [number, number, number];
  previousPosition: [number, number, number];
  currentPosition: [number, number, number];
  velocity: [number, number, number];
  entityType: string;
  authorityServer: number;
  hasSecondSnapshot: boolean;
}

interface RawEntity {
  id: string;
  position: [number, number, number];
  velocity: [number, number, number];
  entity_type: string;
  authority_server: number;
}

const entities = new Map<string, Entity>();
let currentTick = 0;
let lastSnapshotTime = Date.now();
let measuredIntervalMs = 50;
let prevSnapshotTime = 0;

function applySnapshot(tick: number, rawEntities: RawEntity[]) {
  // Skip duplicate ticks
  if (tick === currentTick) return false;

  currentTick = tick;
  const now = Date.now();
  if (prevSnapshotTime > 0) {
    const interval = now - prevSnapshotTime;
    if (interval > 10 && interval < 500) {
      measuredIntervalMs = measuredIntervalMs * 0.8 + interval * 0.2;
    }
  }
  prevSnapshotTime = now;
  lastSnapshotTime = now;

  const incomingIds = new Set<string>();

  for (const raw of rawEntities) {
    incomingIds.add(raw.id);

    const existing = entities.get(raw.id);
    if (existing) {
      existing.previousPosition = [...existing.currentPosition];
      existing.currentPosition = [...raw.position];
      existing.velocity = [...raw.velocity];
      existing.entityType = raw.entity_type;
      existing.authorityServer = raw.authority_server;
      existing.hasSecondSnapshot = true;
    } else {
      entities.set(raw.id, {
        id: raw.id,
        position: [...raw.position],
        previousPosition: [...raw.position],
        currentPosition: [...raw.position],
        velocity: [...raw.velocity],
        entityType: raw.entity_type,
        authorityServer: raw.authority_server,
        hasSecondSnapshot: false,
      });
    }
  }

  // Remove entities not in snapshot — THIS IS WHERE FLICKER HAPPENS
  for (const id of entities.keys()) {
    if (!incomingIds.has(id)) {
      entities.delete(id);
    }
  }

  return true;
}

// ── Binary decoder (mirrors client/src/binary-protocol.ts) ──

function decodeFrame(buf: Buffer): { tick: number; entities: RawEntity[] } | null {
  if (buf.length < 1) return null;
  const type = buf[0];

  if (type === 0x01) {
    if (buf.length < 27) return null;
    const tick = buf.readUInt32LE(1) + buf.readUInt32LE(5) * 0x100000000;
    const count = buf.readUInt32LE(9);
    const ents: RawEntity[] = [];
    let off = 27;
    for (let i = 0; i < count && off + ENTITY_SIZE <= buf.length; i++) {
      ents.push({
        id: uuidFromBytes(buf, off),
        position: [buf.readFloatLE(off+16), buf.readFloatLE(off+20), buf.readFloatLE(off+24)],
        velocity: [buf.readFloatLE(off+28), buf.readFloatLE(off+32), buf.readFloatLE(off+36)],
        entity_type: buf[off+40] === 1 ? 'Player' : 'Npc',
        authority_server: buf.readUInt16LE(off+41),
      });
      off += ENTITY_SIZE;
    }
    return { tick, entities: ents };
  }

  if (type === 0x02) {
    if (buf.length < 31) return null;
    const tick = buf.readUInt32LE(1) + buf.readUInt32LE(5) * 0x100000000;
    const updateCount = buf.readUInt32LE(9);
    const removalCount = buf.readUInt32LE(13);
    const ents: RawEntity[] = [];
    let off = 31;
    for (let i = 0; i < updateCount && off + ENTITY_SIZE <= buf.length; i++) {
      ents.push({
        id: uuidFromBytes(buf, off),
        position: [buf.readFloatLE(off+16), buf.readFloatLE(off+20), buf.readFloatLE(off+24)],
        velocity: [buf.readFloatLE(off+28), buf.readFloatLE(off+32), buf.readFloatLE(off+36)],
        entity_type: buf[off+40] === 1 ? 'Player' : 'Npc',
        authority_server: buf.readUInt16LE(off+41),
      });
      off += ENTITY_SIZE;
    }
    // For delta: we'd need to handle removals, but with FULL_SNAPSHOT_INTERVAL=1 there are no deltas
    return { tick, entities: ents };
  }

  return null;
}

// ── Tracking ──

interface FrameRecord {
  frameNum: number;
  tick: number;
  time: number;
  entityCount: number;
  serverCounts: Map<number, number>;
  applied: boolean; // false if skipped (dupe tick)
}

const frameRecords: FrameRecord[] = [];
let frameNum = 0;

// Per-entity tracking for position jumps
const prevPositions = new Map<string, [number, number, number]>();

// Problem counters
let totalFlickerIn = 0;  // entity appeared (was absent last frame)
let totalFlickerOut = 0; // entity disappeared (was present last frame)
let totalPosJumps = 0;   // position moved > threshold between applied frames
let totalPosChecks = 0;
let worstJump = 0;
let skippedDupes = 0;
let framesApplied = 0;
const prevEntityIds = new Set<string>();

console.log(`Connecting to ${GW_URL}...`);
const ws = new WebSocket(GW_URL);
const startTime = Date.now();

ws.on('open', () => console.log(`Connected. Simulating client for ${DURATION_MS / 1000}s...\n`));

ws.on('message', (data: Buffer | string, isBinary: boolean) => {
  if (Date.now() - startTime > DURATION_MS + 2000) return;

  if (!isBinary) return; // skip text frames (PlayerAssigned)

  const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
  const frame = decodeFrame(buf);
  if (!frame) return;

  const applied = applySnapshot(frame.tick, frame.entities);
  if (!applied) {
    skippedDupes++;
    frameNum++;
    return;
  }

  framesApplied++;

  // Server counts
  const serverCounts = new Map<number, number>();
  for (const e of entities.values()) {
    serverCounts.set(e.authorityServer, (serverCounts.get(e.authorityServer) || 0) + 1);
  }

  // Flicker detection: entities that appeared or disappeared
  const currentIds = new Set(entities.keys());
  if (framesApplied > 1) {
    for (const id of currentIds) {
      if (!prevEntityIds.has(id)) totalFlickerIn++;
    }
    for (const id of prevEntityIds) {
      if (!currentIds.has(id)) totalFlickerOut++;
    }
  }

  // Position jump detection (from WorldView's perspective)
  if (framesApplied > 2) { // skip first 2 applied frames for warmup
    for (const [id, entity] of entities) {
      const prev = prevPositions.get(id);
      if (!prev) continue;
      totalPosChecks++;
      // Check jump between previousPosition (what client renders at alpha=0)
      // and what it was last frame
      const dx = entity.currentPosition[0] - prev[0];
      const dz = entity.currentPosition[2] - prev[2];
      const dist = Math.sqrt(dx * dx + dz * dz);
      // At 10Hz, 3 units/sec → max 0.3 units per frame. Use 2.0 as generous threshold.
      if (dist > 2.0) {
        totalPosJumps++;
        if (dist > worstJump) worstJump = dist;
      }
    }
  }

  // Save state for next frame
  prevEntityIds.clear();
  for (const id of currentIds) prevEntityIds.add(id);
  prevPositions.clear();
  for (const [id, e] of entities) {
    prevPositions.set(id, [...e.currentPosition]);
  }

  frameRecords.push({
    frameNum, tick: frame.tick, time: Date.now(),
    entityCount: entities.size, serverCounts, applied: true,
  });

  // Live output every 20 frames
  if (framesApplied % 20 === 0) {
    const dist = [...serverCounts.entries()].sort((a,b) => a[0]-b[0]).map(([s,c]) => `S${s}:${c}`).join(' ');
    console.log(`  [${framesApplied}] tick=${frame.tick} entities=${entities.size} ${dist} | flicker_in=${totalFlickerIn} flicker_out=${totalFlickerOut} jumps=${totalPosJumps}`);
  }

  frameNum++;
});

ws.on('error', (e) => { console.error('WS error:', e.message); process.exit(1); });

setTimeout(() => {
  ws.close();

  console.log(`\n${'='.repeat(60)}`);
  console.log(`  CLIENT SIMULATION RESULTS`);
  console.log(`${'='.repeat(60)}\n`);

  console.log(`Frames received: ${frameNum}`);
  console.log(`Frames applied:  ${framesApplied}`);
  console.log(`Duplicate ticks skipped: ${skippedDupes}`);
  console.log(`Measured interval: ${measuredIntervalMs.toFixed(0)}ms (${(1000/measuredIntervalMs).toFixed(1)} Hz)`);
  console.log('');
  console.log(`Entity flicker-in events:  ${totalFlickerIn} (entities appearing)`);
  console.log(`Entity flicker-out events: ${totalFlickerOut} (entities disappearing)`);
  console.log(`Position jumps (>2 units): ${totalPosJumps}/${totalPosChecks} (${totalPosChecks > 0 ? (totalPosJumps/totalPosChecks*100).toFixed(2) : 0}%)`);
  console.log(`Worst jump: ${worstJump.toFixed(1)} units`);

  // Per-server stability
  if (frameRecords.length > 0) {
    const allServers = new Set<number>();
    for (const r of frameRecords) for (const s of r.serverCounts.keys()) allServers.add(s);
    console.log('\nPer-server stability:');
    for (const sid of [...allServers].sort()) {
      const counts = frameRecords.map(r => r.serverCounts.get(sid) || 0);
      const zeros = counts.filter(c => c === 0).length;
      console.log(`  S${sid}: min=${Math.min(...counts)} max=${Math.max(...counts)} zeros=${zeros}/${counts.length}`);
    }
  }

  console.log(`\n${'='.repeat(60)}`);
  const problems: string[] = [];
  if (totalFlickerIn > 5) problems.push(`${totalFlickerIn} flicker-in events`);
  if (totalFlickerOut > 5) problems.push(`${totalFlickerOut} flicker-out events`);
  if (totalPosJumps > 0) problems.push(`${totalPosJumps} position jumps (worst: ${worstJump.toFixed(1)})`);
  if (framesApplied < 50) problems.push(`only ${framesApplied} frames in ${DURATION_MS/1000}s`);

  if (problems.length === 0) {
    console.log('PASS — client simulation shows no issues');
    process.exit(0);
  } else {
    console.log('FAIL:');
    for (const p of problems) console.log(`  - ${p}`);
    process.exit(1);
  }
}, DURATION_MS);
