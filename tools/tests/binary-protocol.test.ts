/**
 * Binary protocol decoder tests — verifies that the client-side TypeScript decoder
 * correctly parses frames produced by the Rust gateway.
 *
 * Run: npx tsx tools/tests/binary-protocol.test.ts
 */
import assert from 'node:assert/strict';

// ── Re-implement types and decoder (extracted from client/src/) to avoid THREE dependency ──

const FRAME_FULL = 0x01;
const FRAME_DELTA = 0x02;
const ENTITY_SIZE = 43;

const HEX: string[] = [];
for (let i = 0; i < 256; i++) {
  HEX.push(i.toString(16).padStart(2, '0'));
}

function uuidFromBytes(bytes: Uint8Array, offset: number): string {
  return (
    HEX[bytes[offset]]     + HEX[bytes[offset + 1]] +
    HEX[bytes[offset + 2]] + HEX[bytes[offset + 3]] + '-' +
    HEX[bytes[offset + 4]] + HEX[bytes[offset + 5]] + '-' +
    HEX[bytes[offset + 6]] + HEX[bytes[offset + 7]] + '-' +
    HEX[bytes[offset + 8]] + HEX[bytes[offset + 9]] + '-' +
    HEX[bytes[offset + 10]] + HEX[bytes[offset + 11]] +
    HEX[bytes[offset + 12]] + HEX[bytes[offset + 13]] +
    HEX[bytes[offset + 14]] + HEX[bytes[offset + 15]]
  );
}

interface RawEntity {
  id: string;
  position: [number, number, number];
  velocity: [number, number, number];
  entity_type: 'Player' | 'Npc';
  authority_server: number;
}

interface GatewayMetrics {
  snapshotsPerSec: number;
  bandwidthKBps: number;
  rebalanceEvents: number;
  serverCount: number;
}

interface BinaryFrame {
  type: 'full' | 'delta';
  tick: number;
  metrics: GatewayMetrics;
  entities: RawEntity[];
  removals: string[];
}

function readEntity(view: DataView, bytes: Uint8Array, offset: number): RawEntity {
  return {
    id: uuidFromBytes(bytes, offset),
    position: [
      view.getFloat32(offset + 16, true),
      view.getFloat32(offset + 20, true),
      view.getFloat32(offset + 24, true),
    ],
    velocity: [
      view.getFloat32(offset + 28, true),
      view.getFloat32(offset + 32, true),
      view.getFloat32(offset + 36, true),
    ],
    entity_type: bytes[offset + 40] === 1 ? 'Player' : 'Npc',
    authority_server: view.getUint16(offset + 41, true),
  };
}

function readMetrics(view: DataView, offset: number): GatewayMetrics {
  return {
    snapshotsPerSec: view.getUint32(offset, true),
    bandwidthKBps: view.getUint32(offset + 4, true),
    rebalanceEvents: view.getUint32(offset + 8, true),
    serverCount: view.getUint16(offset + 12, true),
  };
}

function decodeBinaryFrame(buffer: ArrayBuffer): BinaryFrame | null {
  if (buffer.byteLength < 1) return null;

  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);
  const frameType = bytes[0];

  if (frameType === FRAME_FULL) {
    const tickLow = view.getUint32(1, true);
    const tickHigh = view.getUint32(5, true);
    const tick = tickLow + tickHigh * 0x100000000;
    const entityCount = view.getUint32(9, true);
    const metrics = readMetrics(view, 13);

    const entities: RawEntity[] = new Array(entityCount);
    let offset = 27;
    for (let i = 0; i < entityCount; i++) {
      entities[i] = readEntity(view, bytes, offset);
      offset += ENTITY_SIZE;
    }

    return { type: 'full', tick, metrics, entities, removals: [] };
  }

  if (frameType === FRAME_DELTA) {
    const tickLow = view.getUint32(1, true);
    const tickHigh = view.getUint32(5, true);
    const tick = tickLow + tickHigh * 0x100000000;
    const updateCount = view.getUint32(9, true);
    const removalCount = view.getUint32(13, true);
    const metrics = readMetrics(view, 17);

    const entities: RawEntity[] = new Array(updateCount);
    let offset = 31;
    for (let i = 0; i < updateCount; i++) {
      entities[i] = readEntity(view, bytes, offset);
      offset += ENTITY_SIZE;
    }

    const removals: string[] = new Array(removalCount);
    for (let i = 0; i < removalCount; i++) {
      removals[i] = uuidFromBytes(bytes, offset);
      offset += 16;
    }

    return { type: 'delta', tick, metrics, entities, removals };
  }

  return null;
}

// ── Helpers to build binary frames (mimicking Rust gateway) ──

function writeEntity(buf: Buffer, offset: number, e: {
  uuid: Buffer; // 16 bytes
  px: number; py: number; pz: number;
  vx: number; vy: number; vz: number;
  entityType: number; // 0=Npc, 1=Player
  authority: number;  // u16
}): void {
  e.uuid.copy(buf, offset);
  buf.writeFloatLE(e.px, offset + 16);
  buf.writeFloatLE(e.py, offset + 20);
  buf.writeFloatLE(e.pz, offset + 24);
  buf.writeFloatLE(e.vx, offset + 28);
  buf.writeFloatLE(e.vy, offset + 32);
  buf.writeFloatLE(e.vz, offset + 36);
  buf[offset + 40] = e.entityType;
  buf.writeUInt16LE(e.authority, offset + 41);
}

function writeMetrics(buf: Buffer, offset: number, m: GatewayMetrics): void {
  buf.writeUInt32LE(m.snapshotsPerSec, offset);
  buf.writeUInt32LE(m.bandwidthKBps, offset + 4);
  buf.writeUInt32LE(m.rebalanceEvents, offset + 8);
  buf.writeUInt16LE(m.serverCount, offset + 12);
}

function buildFullSnapshot(tick: number, entities: Parameters<typeof writeEntity>[2][], metrics: GatewayMetrics): ArrayBuffer {
  const headerSize = 1 + 8 + 4 + 14; // type + tick + count + metrics
  const buf = Buffer.alloc(headerSize + entities.length * ENTITY_SIZE);
  buf[0] = FRAME_FULL;
  buf.writeUInt32LE(tick & 0xFFFFFFFF, 1);        // tick low
  buf.writeUInt32LE(Math.floor(tick / 0x100000000), 5); // tick high
  buf.writeUInt32LE(entities.length, 9);
  writeMetrics(buf, 13, metrics);
  let offset = headerSize;
  for (const e of entities) {
    writeEntity(buf, offset, e);
    offset += ENTITY_SIZE;
  }
  return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length);
}

function buildDeltaFrame(tick: number, updates: Parameters<typeof writeEntity>[2][], removalUuids: Buffer[], metrics: GatewayMetrics): ArrayBuffer {
  const headerSize = 1 + 8 + 4 + 4 + 14; // type + tick + update_count + removal_count + metrics
  const buf = Buffer.alloc(headerSize + updates.length * ENTITY_SIZE + removalUuids.length * 16);
  buf[0] = FRAME_DELTA;
  buf.writeUInt32LE(tick & 0xFFFFFFFF, 1);
  buf.writeUInt32LE(Math.floor(tick / 0x100000000), 5);
  buf.writeUInt32LE(updates.length, 9);
  buf.writeUInt32LE(removalUuids.length, 13);
  writeMetrics(buf, 17, metrics);
  let offset = headerSize;
  for (const e of updates) {
    writeEntity(buf, offset, e);
    offset += ENTITY_SIZE;
  }
  for (const uuid of removalUuids) {
    uuid.copy(buf, offset);
    offset += 16;
  }
  return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.length);
}

// Known UUID for deterministic testing
const TEST_UUID = Buffer.from('550e8400e29b41d4a716446655440000', 'hex');
const TEST_UUID_STR = '550e8400-e29b-41d4-a716-446655440000';

const defaultMetrics: GatewayMetrics = {
  snapshotsPerSec: 20,
  bandwidthKBps: 500,
  rebalanceEvents: 3,
  serverCount: 3,
};

// ── Tests ──

let passed = 0;
let failed = 0;

function test(name: string, fn: () => void) {
  try {
    fn();
    passed++;
    console.log(`  PASS  ${name}`);
  } catch (e: any) {
    failed++;
    console.log(`  FAIL  ${name}`);
    console.log(`        ${e.message}`);
  }
}

console.log('binary-protocol tests:');

test('empty buffer returns null', () => {
  assert.equal(decodeBinaryFrame(new ArrayBuffer(0)), null);
});

test('unknown frame type returns null', () => {
  const buf = new ArrayBuffer(1);
  new Uint8Array(buf)[0] = 0xFF;
  assert.equal(decodeBinaryFrame(buf), null);
});

test('full snapshot: decode tick', () => {
  const frame = buildFullSnapshot(42, [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.notEqual(decoded, null);
  assert.equal(decoded!.type, 'full');
  assert.equal(decoded!.tick, 42);
});

test('full snapshot: decode large tick (u64)', () => {
  const largeTick = 0x1_0000_0001; // > 32 bits
  const frame = buildFullSnapshot(largeTick, [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.tick, largeTick);
});

test('full snapshot: decode metrics', () => {
  const frame = buildFullSnapshot(1, [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.metrics.snapshotsPerSec, 20);
  assert.equal(decoded!.metrics.bandwidthKBps, 500);
  assert.equal(decoded!.metrics.rebalanceEvents, 3);
  assert.equal(decoded!.metrics.serverCount, 3);
});

test('full snapshot: empty entities', () => {
  const frame = buildFullSnapshot(1, [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities.length, 0);
  assert.equal(decoded!.removals.length, 0);
});

test('full snapshot: single NPC entity', () => {
  const entity = {
    uuid: TEST_UUID,
    px: 10.5, py: 0.0, pz: -20.25,
    vx: 1.0, vy: 0.0, vz: -0.5,
    entityType: 0, // Npc
    authority: 1,
  };
  const frame = buildFullSnapshot(100, [entity], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities.length, 1);
  const e = decoded!.entities[0];
  assert.equal(e.id, TEST_UUID_STR);
  assert.ok(Math.abs(e.position[0] - 10.5) < 0.001);
  assert.ok(Math.abs(e.position[2] - (-20.25)) < 0.001);
  assert.ok(Math.abs(e.velocity[0] - 1.0) < 0.001);
  assert.equal(e.entity_type, 'Npc');
  assert.equal(e.authority_server, 1);
});

test('full snapshot: player entity type', () => {
  const entity = {
    uuid: TEST_UUID,
    px: 0, py: 0, pz: 0,
    vx: 0, vy: 0, vz: 0,
    entityType: 1, // Player
    authority: 2,
  };
  const frame = buildFullSnapshot(1, [entity], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities[0].entity_type, 'Player');
  assert.equal(decoded!.entities[0].authority_server, 2);
});

test('full snapshot: multiple entities', () => {
  const uuid2 = Buffer.from('660e8400e29b41d4a716446655440000', 'hex');
  const entities = [
    { uuid: TEST_UUID, px: 1, py: 0, pz: 2, vx: 0, vy: 0, vz: 0, entityType: 0, authority: 1 },
    { uuid: uuid2, px: 3, py: 0, pz: 4, vx: 0, vy: 0, vz: 0, entityType: 1, authority: 2 },
  ];
  const frame = buildFullSnapshot(50, entities, defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities.length, 2);
  assert.equal(decoded!.entities[0].id, TEST_UUID_STR);
  assert.equal(decoded!.entities[1].id, '660e8400-e29b-41d4-a716-446655440000');
});

test('full snapshot: frame size matches expected (header + N*43)', () => {
  const entity = {
    uuid: TEST_UUID, px: 0, py: 0, pz: 0, vx: 0, vy: 0, vz: 0, entityType: 0, authority: 1,
  };
  const frame = buildFullSnapshot(1, [entity, entity], defaultMetrics);
  // Header: 1 + 8 + 4 + 14 = 27, entities: 2 * 43 = 86
  assert.equal(frame.byteLength, 27 + 86);
});

test('delta frame: decode type and tick', () => {
  const frame = buildDeltaFrame(200, [], [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.type, 'delta');
  assert.equal(decoded!.tick, 200);
});

test('delta frame: updated entities', () => {
  const entity = {
    uuid: TEST_UUID, px: 5.0, py: 0, pz: 10.0, vx: 0, vy: 0, vz: 0, entityType: 0, authority: 1,
  };
  const frame = buildDeltaFrame(50, [entity], [], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities.length, 1);
  assert.ok(Math.abs(decoded!.entities[0].position[0] - 5.0) < 0.001);
});

test('delta frame: removed entities', () => {
  const removedUuid = Buffer.from('770e8400e29b41d4a716446655440000', 'hex');
  const frame = buildDeltaFrame(60, [], [removedUuid], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.removals.length, 1);
  assert.equal(decoded!.removals[0], '770e8400-e29b-41d4-a716-446655440000');
  assert.equal(decoded!.entities.length, 0);
});

test('delta frame: updates and removals together', () => {
  const entity = {
    uuid: TEST_UUID, px: 1, py: 0, pz: 2, vx: 0, vy: 0, vz: 0, entityType: 0, authority: 1,
  };
  const removedUuid = Buffer.from('880e8400e29b41d4a716446655440000', 'hex');
  const frame = buildDeltaFrame(70, [entity], [removedUuid], defaultMetrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.entities.length, 1);
  assert.equal(decoded!.removals.length, 1);
});

test('delta frame: metrics decoded', () => {
  const metrics = { snapshotsPerSec: 40, bandwidthKBps: 1024, rebalanceEvents: 7, serverCount: 5 };
  const frame = buildDeltaFrame(1, [], [], metrics);
  const decoded = decodeBinaryFrame(frame);
  assert.equal(decoded!.metrics.snapshotsPerSec, 40);
  assert.equal(decoded!.metrics.bandwidthKBps, 1024);
  assert.equal(decoded!.metrics.rebalanceEvents, 7);
  assert.equal(decoded!.metrics.serverCount, 5);
});

test('uuid byte encoding produces correct format', () => {
  // Verify the UUID hex format matches RFC 4122 (8-4-4-4-12)
  const uuid = uuidFromBytes(TEST_UUID, 0);
  assert.match(uuid, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  assert.equal(uuid, TEST_UUID_STR);
});

test('uuid all zeros', () => {
  const zeros = Buffer.alloc(16);
  const uuid = uuidFromBytes(zeros, 0);
  assert.equal(uuid, '00000000-0000-0000-0000-000000000000');
});

test('uuid all ones', () => {
  const ones = Buffer.alloc(16, 0xFF);
  const uuid = uuidFromBytes(ones, 0);
  assert.equal(uuid, 'ffffffff-ffff-ffff-ffff-ffffffffffff');
});

// ── Summary ──
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
