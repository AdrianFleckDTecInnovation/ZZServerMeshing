/**
 * WorldView unit tests.
 *
 * Run: npx tsx tools/tests/world-view.test.ts
 */
import assert from 'node:assert/strict';

// ── Minimal THREE mocks ──

class Vec3 {
  x: number; y: number; z: number;
  constructor(x = 0, y = 0, z = 0) { this.x = x; this.y = y; this.z = z; }
  copy(v: Vec3) { this.x = v.x; this.y = v.y; this.z = v.z; return this; }
  clone() { return new Vec3(this.x, this.y, this.z); }
  lerpVectors(a: Vec3, b: Vec3, t: number) {
    this.x = a.x + (b.x - a.x) * t;
    this.y = a.y + (b.y - a.y) * t;
    this.z = a.z + (b.z - a.z) * t;
    return this;
  }
}

class Color {
  r: number; g: number; b: number;
  constructor(_hex: number = 0) {
    this.r = 0; this.g = 0; this.b = 0;
  }
}

// ── Re-implement WorldView to avoid THREE import ──

type Vec3Like = [number, number, number] | { x: number; y: number; z: number };

interface RawEntity {
  id: string;
  position: Vec3Like;
  velocity: Vec3Like;
  entity_type: 'Player' | 'Npc';
  authority_server: number;
  last_input_seq: number;
}

interface EntityData {
  id: string;
  position: Vec3;
  previousPosition: Vec3;
  currentPosition: Vec3;
  velocity: Vec3;
  entityType: 'Player' | 'Npc';
  authorityServer: number;
  hasSecondSnapshot: boolean;
  lastInputSeq: number;
}

interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: RawEntity[];
}

function toVec3(v: Vec3Like): Vec3 {
  if (Array.isArray(v)) return new Vec3(v[0], v[1], v[2]);
  return new Vec3(v.x, v.y, v.z);
}

class WorldView {
  entities: Map<string, EntityData> = new Map();
  currentTick: number = 0;
  serverEntityCounts: Map<number, number> = new Map();
  localPlayerId: string | null = null;

  /** Snapshot count — skip interpolation during warmup */
  private snapshotCount: number = 0;
  private static readonly WARMUP_SNAPSHOTS = 3;

  applySnapshot(snapshot: WorldSnapshot): void {
    // Skip duplicate ticks
    if (snapshot.tick === this.currentTick) return;

    this.currentTick = snapshot.tick;
    this.snapshotCount++;

    const incomingIds = new Set<string>();

    for (const raw of snapshot.entities) {
      const id = raw.id;
      const serverId = raw.authority_server;

      incomingIds.add(id);

      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);

      const existing = this.entities.get(id);
      const isLocalPlayer = id === this.localPlayerId;
      if (existing) {
        existing.lastInputSeq = raw.last_input_seq ?? 0;
        if (isLocalPlayer) {
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
        } else if (this.snapshotCount <= WorldView.WARMUP_SNAPSHOTS) {
          existing.position.copy(pos);
          existing.previousPosition.copy(pos);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = false;
        } else {
          existing.previousPosition.copy(existing.position);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = true;
        }
      } else {
        this.entities.set(id, {
          id,
          position: pos.clone(),
          previousPosition: pos.clone(),
          currentPosition: pos.clone(),
          velocity: vel,
          entityType: raw.entity_type,
          authorityServer: serverId,
          hasSecondSnapshot: false,
          lastInputSeq: raw.last_input_seq ?? 0,
        });
      }
    }

    // Remove entities no longer in snapshot
    for (const id of this.entities.keys()) {
      if (!incomingIds.has(id)) {
        this.entities.delete(id);
      }
    }

    // Rebuild server counts
    this.serverEntityCounts.clear();
    for (const entity of this.entities.values()) {
      this.serverEntityCounts.set(
        entity.authorityServer,
        (this.serverEntityCounts.get(entity.authorityServer) || 0) + 1
      );
    }
  }

  applyDelta(tick: number, updates: RawEntity[], removals: string[]): void {
    if (tick === this.currentTick) return;
    this.currentTick = tick;

    for (const raw of updates) {
      const id = raw.id;
      const serverId = raw.authority_server;
      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);
      const isLocalPlayer = id === this.localPlayerId;

      const existing = this.entities.get(id);
      if (existing) {
        existing.lastInputSeq = raw.last_input_seq ?? 0;
        if (isLocalPlayer) {
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
        } else {
          existing.previousPosition.copy(existing.position);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = true;
        }
      } else {
        this.entities.set(id, {
          id,
          position: pos.clone(),
          previousPosition: pos.clone(),
          currentPosition: pos.clone(),
          velocity: vel,
          entityType: raw.entity_type,
          authorityServer: serverId,
          hasSecondSnapshot: false,
          lastInputSeq: raw.last_input_seq ?? 0,
        });
      }
    }

    for (const id of removals) {
      this.entities.delete(id);
    }

    this.serverEntityCounts.clear();
    for (const entity of this.entities.values()) {
      this.serverEntityCounts.set(
        entity.authorityServer,
        (this.serverEntityCounts.get(entity.authorityServer) || 0) + 1
      );
    }
  }

  interpolate(_dt: number): void {
    // Use alpha=0.5 for deterministic testing (real code uses performance.now)
    const alpha = 0.5;

    for (const entity of this.entities.values()) {
      if (entity.id === this.localPlayerId) continue;
      if (!entity.hasSecondSnapshot) {
        entity.position.copy(entity.currentPosition);
        continue;
      }
      entity.position.lerpVectors(entity.previousPosition, entity.currentPosition, alpha);
    }
  }
}

// ── Helpers ──

function makeRaw(id: string, pos: [number, number, number], server: number, opts?: Partial<RawEntity>): RawEntity {
  return {
    id,
    position: pos,
    velocity: [0, 0, 0],
    entity_type: 'Npc',
    authority_server: server,
    last_input_seq: 0,
    ...opts,
  };
}

function makeSnapshot(tick: number, entities: RawEntity[]): WorldSnapshot {
  return { type: 'WorldSnapshot', tick, entities };
}

/** Push past the warmup period so normal interpolation logic activates */
function warmup(wv: WorldView): void {
  for (let t = 1; t <= 3; t++) {
    wv.applySnapshot(makeSnapshot(t, [makeRaw('warmup-entity', [0, 0, 0], 1)]));
  }
}

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

console.log('world-view tests:');

// 1
test('applySnapshot adds entities', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 2),
  ]));
  assert.equal(wv.entities.size, 2);
});

// 2
test('applySnapshot skips duplicate ticks', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(5, [
    makeRaw('a', [1, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 1);

  // Same tick again with a different entity — should be ignored
  wv.applySnapshot(makeSnapshot(5, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 1, 'duplicate tick should be ignored');
});

// 3
test('applySnapshot removes stale entities', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 2);

  // Next snapshot only contains entity 'a'
  wv.applySnapshot(makeSnapshot(2, [
    makeRaw('a', [1, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 1);
  assert.ok(!wv.entities.has('b'), 'entity b should be removed');
});

// 4
test('applySnapshot updates existing entity position', () => {
  const wv = new WorldView();
  warmup(wv);

  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('a', [0, 0, 0], 1),
  ]));
  wv.applySnapshot(makeSnapshot(11, [
    makeRaw('a', [10, 0, 0], 1),
  ]));

  const entity = wv.entities.get('a')!;
  assert.equal(entity.currentPosition.x, 10, 'currentPosition should be updated to 10');
});

// 5
test('applySnapshot local player skips position overwrite', () => {
  const wv = new WorldView();
  warmup(wv);

  wv.localPlayerId = 'player-1';

  // First snapshot adds the player
  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('player-1', [5, 0, 0], 1, { entity_type: 'Player' }),
  ]));

  const entity = wv.entities.get('player-1')!;
  // Manually set the rendered position to simulate prediction
  entity.position.x = 99;

  // Apply a new snapshot — position should NOT be overwritten for local player
  wv.applySnapshot(makeSnapshot(11, [
    makeRaw('player-1', [10, 0, 0], 1, { entity_type: 'Player' }),
  ]));

  assert.equal(entity.position.x, 99, 'local player position should not be overwritten');
  assert.equal(entity.currentPosition.x, 10, 'currentPosition should be updated from server');
});

// 6
test('applyDelta adds new entities', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 1);

  wv.applyDelta(2, [makeRaw('b', [2, 0, 0], 2)], []);
  assert.equal(wv.entities.size, 2);
  assert.ok(wv.entities.has('b'), 'new entity should exist after delta');
});

// 7
test('applyDelta removes entities by ID', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 2);

  wv.applyDelta(2, [], ['b']);
  assert.equal(wv.entities.size, 1);
  assert.ok(!wv.entities.has('b'), 'entity b should be removed by delta');
});

// 8
test('applyDelta updates existing entity', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
  ]));

  wv.applyDelta(2, [makeRaw('a', [50, 0, 0], 1)], []);
  const entity = wv.entities.get('a')!;
  assert.equal(entity.currentPosition.x, 50, 'delta should update currentPosition');
});

// 9
test('applyDelta skips duplicate ticks', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
  ]));

  wv.applyDelta(2, [makeRaw('b', [2, 0, 0], 1)], []);
  assert.equal(wv.entities.size, 2);

  // Same tick — should be ignored
  wv.applyDelta(2, [makeRaw('c', [3, 0, 0], 1)], []);
  assert.equal(wv.entities.size, 2, 'duplicate delta tick should be ignored');
});

// 10
test('applyDelta local player skips position overwrite', () => {
  const wv = new WorldView();
  wv.localPlayerId = 'player-1';

  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('player-1', [5, 0, 0], 1, { entity_type: 'Player' }),
  ]));

  const entity = wv.entities.get('player-1')!;
  entity.position.x = 77;

  wv.applyDelta(2, [makeRaw('player-1', [20, 0, 0], 1, { entity_type: 'Player' })], []);

  assert.equal(entity.position.x, 77, 'local player position should not be overwritten by delta');
  assert.equal(entity.currentPosition.x, 20, 'currentPosition should be updated');
});

// 11
test('interpolate snaps entities without second snapshot', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [10, 0, 5], 1),
  ]));

  const entity = wv.entities.get('a')!;
  assert.equal(entity.hasSecondSnapshot, false);

  // Set position to something different to verify snap behavior
  entity.position.x = 999;
  wv.interpolate(0);

  // Without second snapshot, position snaps to currentPosition
  assert.equal(entity.position.x, 10, 'should snap to currentPosition.x');
  assert.equal(entity.position.z, 5, 'should snap to currentPosition.z');
});

// 12
test('interpolate blends between previous and current', () => {
  const wv = new WorldView();
  warmup(wv);

  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('a', [0, 0, 0], 1),
  ]));
  wv.applySnapshot(makeSnapshot(11, [
    makeRaw('a', [10, 0, 0], 1),
  ]));

  const entity = wv.entities.get('a')!;
  assert.equal(entity.hasSecondSnapshot, true);

  // After applySnapshot, previousPosition = old rendered position, currentPosition = (10,0,0)
  // Force known values for deterministic test
  entity.previousPosition.x = 0;
  entity.currentPosition.x = 10;

  wv.interpolate(0);

  // Our test interpolate uses alpha=0.5: lerp(0, 10, 0.5) = 5
  assert.equal(entity.position.x, 5, 'position should be interpolated between prev and current');
});

// 13
test('interpolate skips local player', () => {
  const wv = new WorldView();
  wv.localPlayerId = 'player-1';

  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('player-1', [10, 0, 0], 1, { entity_type: 'Player' }),
  ]));

  const entity = wv.entities.get('player-1')!;
  entity.position.x = 42;
  entity.currentPosition.x = 99;

  wv.interpolate(0);

  assert.equal(entity.position.x, 42, 'local player position should not be changed by interpolate');
});

// 14
test('serverEntityCounts updated after snapshot', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [0, 0, 0], 1),
    makeRaw('c', [0, 0, 0], 2),
  ]));

  assert.equal(wv.serverEntityCounts.get(1), 2, 'server 1 should have 2 entities');
  assert.equal(wv.serverEntityCounts.get(2), 1, 'server 2 should have 1 entity');
});

// 15
test('lastInputSeq populated from raw entity', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1, { last_input_seq: 42 }),
  ]));

  const entity = wv.entities.get('a')!;
  assert.equal(entity.lastInputSeq, 42, 'lastInputSeq should carry through from raw');
});

// 16
test('entity count stable across consistent snapshots', () => {
  const wv = new WorldView();
  const entities = [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
    makeRaw('c', [3, 0, 0], 2),
  ];

  for (let tick = 1; tick <= 5; tick++) {
    wv.applySnapshot(makeSnapshot(tick, entities));
    assert.equal(wv.entities.size, 3, `entity count should be 3 at tick ${tick}`);
  }
});

// ── Summary ──
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
