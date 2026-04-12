/**
 * Binary snapshot protocol decoder — supports full snapshots and deltas.
 *
 * Frame type byte (first byte):
 *   0x01 = Full Snapshot
 *   0x02 = Delta
 *
 * Full Snapshot (type 0x01):
 *   [type:u8][tick:u64][entity_count:u32][metrics:14bytes]
 *   [entity × N: 43 bytes each]
 *
 * Delta (type 0x02):
 *   [type:u8][tick:u64][update_count:u32][removal_count:u32][metrics:14bytes]
 *   [updated entities × N: 43 bytes each]
 *   [removed UUIDs × M: 16 bytes each]
 *
 * Entity (47 bytes):
 *   [uuid:16][pos:3×f32][vel:3×f32][type:u8][authority:u16][last_input_seq:u32]
 */

import type { WorldSnapshot, RawEntity, GatewayMetrics } from './world-view';

const FRAME_FULL = 0x01;
const FRAME_DELTA = 0x02;
const ENTITY_SIZE = 47;

// Pre-allocated hex lookup table
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
    last_input_seq: view.getUint32(offset + 43, true),
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

export interface BinaryFrame {
  type: 'full' | 'delta';
  tick: number;
  metrics: GatewayMetrics;
  /** Full snapshot: all entities. Delta: only updated entities. */
  entities: RawEntity[];
  /** Delta only: IDs of removed entities. */
  removals: string[];
}

export function decodeBinaryFrame(buffer: ArrayBuffer): BinaryFrame | null {
  if (buffer.byteLength < 1) return null;

  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);
  const frameType = bytes[0];

  if (frameType === FRAME_FULL) {
    // [type:1][tick:8][count:4][metrics:14] = 27 byte header
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
    // [type:1][tick:8][updates:4][removals:4][metrics:14] = 31 byte header
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

/** Convert a BinaryFrame into a WorldSnapshot for backward compat. */
export function frameToSnapshot(frame: BinaryFrame): WorldSnapshot {
  return {
    type: 'WorldSnapshot',
    tick: frame.tick,
    entities: frame.entities,
    _metrics: frame.metrics,
  };
}
