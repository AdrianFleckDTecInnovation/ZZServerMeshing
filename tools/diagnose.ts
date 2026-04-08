/**
 * Raw diagnostic tool — connects to gateway, dumps exactly what arrives,
 * and detects the actual problems: flickering entities, missing servers,
 * entity count swings, bad data.
 */
import WebSocket from 'ws';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const FRAMES_TO_COLLECT = 60; // 3 seconds at 20Hz

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

interface Entity {
  id: string;
  x: number; y: number; z: number;
  vx: number; vy: number; vz: number;
  type: number;
  server: number;
}

interface Frame {
  frameNum: number;
  time: number;
  rawType: number;
  rawSize: number;
  isBinary: boolean;
  tick: number;
  entityCount: number;
  entities: Entity[];
  serverCounts: Map<number, number>;
}

const frames: Frame[] = [];
let frameNum = 0;

console.log(`Connecting to ${GW_URL}...`);
const ws = new WebSocket(GW_URL);

ws.on('open', () => console.log('Connected. Collecting frames...\n'));

ws.on('message', (data: Buffer | string, isBinary: boolean) => {
  if (frameNum >= FRAMES_TO_COLLECT) return;

  const time = Date.now();

  if (!isBinary && typeof data === 'string') {
    // Text frame
    try {
      const msg = JSON.parse(data);
      console.log(`Frame ${frameNum}: TEXT ${msg.type || 'unknown'}`);
    } catch {
      console.log(`Frame ${frameNum}: TEXT (unparseable, ${data.length} bytes)`);
    }
    frameNum++;
    return;
  }

  const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as any);
  const rawType = buf[0];
  const rawSize = buf.length;

  if (rawType === 0x01) {
    // Full snapshot
    if (buf.length < 27) {
      console.log(`Frame ${frameNum}: FULL but too short (${buf.length} bytes)`);
      frameNum++;
      return;
    }

    const tick = buf.readUInt32LE(1) + buf.readUInt32LE(5) * 0x100000000;
    const entityCount = buf.readUInt32LE(9);
    const expectedSize = 27 + entityCount * ENTITY_SIZE;

    if (buf.length !== expectedSize) {
      console.log(`Frame ${frameNum}: FULL size mismatch! expected=${expectedSize} got=${buf.length} entities=${entityCount}`);
    }

    const entities: Entity[] = [];
    const serverCounts = new Map<number, number>();
    let offset = 27;
    for (let i = 0; i < entityCount && offset + ENTITY_SIZE <= buf.length; i++) {
      const e: Entity = {
        id: uuidFromBytes(buf, offset),
        x: buf.readFloatLE(offset + 16),
        y: buf.readFloatLE(offset + 20),
        z: buf.readFloatLE(offset + 24),
        vx: buf.readFloatLE(offset + 28),
        vy: buf.readFloatLE(offset + 32),
        vz: buf.readFloatLE(offset + 36),
        type: buf[offset + 40],
        server: buf.readUInt16LE(offset + 41),
      };
      entities.push(e);
      serverCounts.set(e.server, (serverCounts.get(e.server) || 0) + 1);
      offset += ENTITY_SIZE;
    }

    frames.push({ frameNum, time, rawType, rawSize, isBinary: true, tick, entityCount, entities, serverCounts });
    frameNum++;

  } else if (rawType === 0x02) {
    // Delta
    const updateCount = buf.readUInt32LE(9);
    const removalCount = buf.readUInt32LE(13);
    console.log(`Frame ${frameNum}: DELTA updates=${updateCount} removals=${removalCount}`);
    frameNum++;
  } else {
    console.log(`Frame ${frameNum}: UNKNOWN type=0x${rawType.toString(16)} size=${rawSize}`);
    frameNum++;
  }

  if (frameNum >= FRAMES_TO_COLLECT) {
    analyze();
  }
});

ws.on('error', (e) => { console.error('WS error:', e.message); process.exit(1); });
ws.on('close', () => { console.log('Disconnected'); if (frames.length > 0) analyze(); process.exit(0); });

// Timeout
setTimeout(() => {
  console.log(`\nTimeout — only got ${frameNum} frames`);
  if (frames.length > 0) analyze();
  process.exit(1);
}, 15000);

function analyze() {
  ws.close();
  console.log(`\n========== DIAGNOSIS (${frames.length} binary frames) ==========\n`);

  if (frames.length === 0) {
    console.log('No binary frames received!');
    process.exit(1);
  }

  // Frame timing
  const intervals: number[] = [];
  for (let i = 1; i < frames.length; i++) {
    intervals.push(frames[i].time - frames[i - 1].time);
  }
  const avgInterval = intervals.reduce((a, b) => a + b, 0) / intervals.length;
  console.log(`Frame rate: ${(1000 / avgInterval).toFixed(1)} Hz (avg interval: ${avgInterval.toFixed(0)}ms)`);
  console.log(`  min: ${Math.min(...intervals)}ms  max: ${Math.max(...intervals)}ms`);

  // Entity count per frame
  const counts = frames.map(f => f.entityCount);
  console.log(`\nEntity count: min=${Math.min(...counts)} max=${Math.max(...counts)} swing=${Math.max(...counts) - Math.min(...counts)}`);

  // Per-server count per frame
  const allServers = new Set<number>();
  for (const f of frames) for (const s of f.serverCounts.keys()) allServers.add(s);

  console.log(`\nPer-server entity counts (frame by frame):`);
  for (const sid of [...allServers].sort()) {
    const perFrame = frames.map(f => f.serverCounts.get(sid) || 0);
    const min = Math.min(...perFrame);
    const max = Math.max(...perFrame);
    const zeros = perFrame.filter(c => c === 0).length;
    console.log(`  S${sid}: min=${min} max=${max} swing=${max - min} zeros=${zeros}/${frames.length}`);
    if (zeros > 0 && max > 0) {
      console.log(`    *** S${sid} FLICKERS — present in some frames, absent in others!`);
      // Show the first few transitions
      let transitions = 0;
      for (let i = 1; i < perFrame.length && transitions < 5; i++) {
        if ((perFrame[i] === 0) !== (perFrame[i - 1] === 0)) {
          console.log(`    frame ${frames[i].frameNum}: ${perFrame[i - 1]} → ${perFrame[i]} entities`);
          transitions++;
        }
      }
    }
  }

  // Entity ID stability: which entities appear/disappear
  console.log(`\nEntity ID stability:`);
  let flickerEvents = 0;
  const entityPresence = new Map<string, boolean[]>();
  for (let i = 0; i < frames.length; i++) {
    const ids = new Set(frames[i].entities.map(e => e.id));
    for (const id of ids) {
      if (!entityPresence.has(id)) entityPresence.set(id, new Array(frames.length).fill(false));
      entityPresence.get(id)![i] = true;
    }
  }

  let stableCount = 0;
  let flickerCount = 0;
  for (const [id, presence] of entityPresence) {
    // Check if entity appears and disappears
    let transitions = 0;
    for (let i = 1; i < presence.length; i++) {
      if (presence[i] !== presence[i - 1]) transitions++;
    }
    if (transitions <= 1) {
      stableCount++; // appeared once and stayed, or disappeared once
    } else {
      flickerCount++;
      flickerEvents += transitions;
      if (flickerCount <= 5) {
        console.log(`  ${id.substring(0, 8)}... FLICKERS (${transitions} transitions)`);
      }
    }
  }
  console.log(`  Stable entities: ${stableCount}`);
  console.log(`  Flickering entities: ${flickerCount} (${flickerEvents} transitions total)`);

  // Position validity
  let nanCount = 0;
  let oobCount = 0;
  for (const f of frames) {
    for (const e of f.entities) {
      if (isNaN(e.x) || isNaN(e.y) || isNaN(e.z)) nanCount++;
      if (Math.abs(e.x) > 600 || Math.abs(e.z) > 600) oobCount++;
    }
  }
  console.log(`\nPosition validity:`);
  console.log(`  NaN positions: ${nanCount}`);
  console.log(`  Out of bounds (>600): ${oobCount}`);

  // Position jumps
  let jumpCount = 0;
  let totalChecks = 0;
  for (let i = 1; i < frames.length; i++) {
    const prevMap = new Map(frames[i - 1].entities.map(e => [e.id, e]));
    for (const e of frames[i].entities) {
      const prev = prevMap.get(e.id);
      if (!prev) continue;
      totalChecks++;
      const dx = e.x - prev.x;
      const dz = e.z - prev.z;
      const dist = Math.sqrt(dx * dx + dz * dz);
      if (dist > 10) { // very generous threshold
        jumpCount++;
        if (jumpCount <= 3) {
          console.log(`  JUMP: ${e.id.substring(0, 8)}... moved ${dist.toFixed(1)} units in 1 frame`);
        }
      }
    }
  }
  console.log(`\nPosition jumps (>10 units): ${jumpCount}/${totalChecks}`);

  // Verdict
  console.log(`\n========== VERDICT ==========`);
  const problems: string[] = [];
  if (flickerCount > 0) problems.push(`${flickerCount} flickering entities`);
  if (nanCount > 0) problems.push(`${nanCount} NaN positions`);
  if (jumpCount > 0) problems.push(`${jumpCount} position jumps`);
  if (Math.max(...counts) - Math.min(...counts) > 5) problems.push(`entity count swings by ${Math.max(...counts) - Math.min(...counts)}`);
  if (1000 / avgInterval < 15) problems.push(`low frame rate: ${(1000 / avgInterval).toFixed(1)} Hz`);

  if (problems.length === 0) {
    console.log('PASS — no issues detected');
  } else {
    console.log('FAIL:');
    for (const p of problems) console.log(`  - ${p}`);
  }

  process.exit(problems.length > 0 ? 1 : 0);
}
