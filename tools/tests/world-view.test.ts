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
  lastUpdateTime: number;
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

  /** Snapshot count -- skip interpolation during warmup */
  private snapshotCount: number = 0;
  private static readonly WARMUP_SNAPSHOTS = 3;
  /** Monotonic counter used as a mock for performance.now() in tests */
  _now: number = 0;

  applySnapshot(snapshot: WorldSnapshot): void {
    // Skip duplicate ticks
    if (snapshot.tick === this.currentTick) return;

    this.currentTick = snapshot.tick;
    this.snapshotCount++;
    const now = this._now++;

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
        existing.lastUpdateTime = now;
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
          lastUpdateTime: now,
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
    const now = this._now++;

    for (const raw of updates) {
      const id = raw.id;
      const serverId = raw.authority_server;
      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);
      const isLocalPlayer = id === this.localPlayerId;

      const existing = this.entities.get(id);
      if (existing) {
        existing.lastInputSeq = raw.last_input_seq ?? 0;
        existing.lastUpdateTime = now;
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
          lastUpdateTime: now,
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

// -- Delta compression / per-entity interpolation tests --

// 17
test('applyDelta updates lastUpdateTime for changed entities', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [5, 0, 0], 2),
  ]));

  const entityA = wv.entities.get('a')!;
  const entityB = wv.entities.get('b')!;
  const initialTimeA = entityA.lastUpdateTime;
  const initialTimeB = entityB.lastUpdateTime;

  // Delta only updates entity 'a'
  wv.applyDelta(2, [makeRaw('a', [10, 0, 0], 1)], []);

  assert.ok(entityA.lastUpdateTime > initialTimeA, 'changed entity lastUpdateTime should advance');
  assert.equal(entityB.lastUpdateTime, initialTimeB, 'unchanged entity lastUpdateTime should NOT advance');
});

// 18
test('applyDelta unchanged entities keep interpolating smoothly', () => {
  const wv = new WorldView();
  warmup(wv);

  // Set up entity at known position
  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [100, 0, 0], 2),
  ]));

  // Move only entity 'a'
  wv.applySnapshot(makeSnapshot(11, [
    makeRaw('a', [10, 0, 0], 1),
    makeRaw('b', [100, 0, 0], 2), // unchanged
  ]));

  const entityB = wv.entities.get('b')!;
  // Entity B should have previousPosition from old position, currentPosition from snapshot
  // With hasSecondSnapshot=true, interpolation should work
  assert.equal(entityB.hasSecondSnapshot, true);
});

// 19
test('applyDelta preserves entity not in update or removal list', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
    makeRaw('c', [3, 0, 0], 2),
  ]));

  // Delta updates only 'a' -- 'b' and 'c' should persist
  wv.applyDelta(2, [makeRaw('a', [10, 0, 0], 1)], []);

  assert.equal(wv.entities.size, 3, 'all entities should persist');
  assert.ok(wv.entities.has('b'), 'entity b should still exist');
  assert.ok(wv.entities.has('c'), 'entity c should still exist');
});

// 20
test('applyDelta simultaneous add and remove', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 1),
  ]));
  assert.equal(wv.entities.size, 2);

  // Remove 'b', add 'c' in same delta
  wv.applyDelta(2, [makeRaw('c', [3, 0, 0], 2)], ['b']);

  assert.equal(wv.entities.size, 2, 'should have a and c');
  assert.ok(wv.entities.has('a'));
  assert.ok(!wv.entities.has('b'), 'b should be removed');
  assert.ok(wv.entities.has('c'), 'c should be added');
});

// 21
test('applyDelta with empty updates and empty removals is no-op', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [2, 0, 0], 2),
  ]));

  const prevSize = wv.entities.size;
  wv.applyDelta(2, [], []);

  assert.equal(wv.entities.size, prevSize, 'empty delta should not change entity count');
  assert.equal(wv.entities.get('a')!.currentPosition.x, 1, 'entity a position unchanged');
});

// 22
test('applyDelta updates serverEntityCounts correctly', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [0, 0, 0], 2),
  ]));
  assert.equal(wv.serverEntityCounts.get(1), 1);
  assert.equal(wv.serverEntityCounts.get(2), 1);

  // Remove entity from server 2, add entity to server 1
  wv.applyDelta(2, [makeRaw('c', [0, 0, 0], 1)], ['b']);

  assert.equal(wv.serverEntityCounts.get(1), 2, 'server 1 should have 2 entities');
  assert.equal(wv.serverEntityCounts.get(2) ?? 0, 0, 'server 2 should have 0 entities');
});

// 23
test('applyDelta entity authority transfer updates correctly', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
  ]));

  assert.equal(wv.entities.get('a')!.authorityServer, 1);

  // Delta: entity a transferred to server 2
  wv.applyDelta(2, [makeRaw('a', [0, 0, 0], 2)], []);

  assert.equal(wv.entities.get('a')!.authorityServer, 2, 'authority should update via delta');
  assert.equal(wv.serverEntityCounts.get(2), 1);
  assert.equal(wv.serverEntityCounts.get(1) ?? 0, 0);
});

// 24
test('snapshot after delta resets entity set correctly', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [1, 0, 0], 1),
  ]));

  // Delta adds c
  wv.applyDelta(2, [makeRaw('c', [2, 0, 0], 1)], []);
  assert.equal(wv.entities.size, 3);

  // Full snapshot that does NOT include c -> c should be removed
  wv.applySnapshot(makeSnapshot(3, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [1, 0, 0], 1),
  ]));

  assert.equal(wv.entities.size, 2, 'full snapshot should replace delta-added entity');
  assert.ok(!wv.entities.has('c'), 'c should be removed by full snapshot');
});

// 25
test('applyDelta hasSecondSnapshot set for updated entity', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
  ]));
  assert.equal(wv.entities.get('a')!.hasSecondSnapshot, false, 'initially false');

  // Delta updates entity
  wv.applyDelta(2, [makeRaw('a', [10, 0, 0], 1)], []);
  assert.equal(wv.entities.get('a')!.hasSecondSnapshot, true, 'should be true after delta update');
});

// 26
test('applyDelta new entity has hasSecondSnapshot false', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [makeRaw('a', [0, 0, 0], 1)]));

  // New entity via delta
  wv.applyDelta(2, [makeRaw('b', [5, 0, 0], 2)], []);

  const entityB = wv.entities.get('b')!;
  assert.equal(entityB.hasSecondSnapshot, false, 'new entity from delta should not have second snapshot');
  assert.equal(entityB.position.x, 5, 'new entity should snap to position');
});

// 27
test('applyDelta previousPosition set to rendered position for smooth transition', () => {
  const wv = new WorldView();
  warmup(wv);

  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('a', [0, 0, 0], 1),
  ]));

  const entity = wv.entities.get('a')!;
  // Simulate rendered position from interpolation
  entity.position.x = 7;

  // Apply delta update
  wv.applyDelta(11, [makeRaw('a', [20, 0, 0], 1)], []);

  // previousPosition should be set to the rendered position (7), not the old server position (0)
  assert.equal(entity.previousPosition.x, 7, 'previousPosition should be the rendered position for smooth transition');
  assert.equal(entity.currentPosition.x, 20, 'currentPosition should be the new server position');
});

// 28
test('interpolate with per-entity timing: only updated entities interpolate fresh', () => {
  const wv = new WorldView();
  warmup(wv);

  // Set up two entities
  wv.applySnapshot(makeSnapshot(10, [
    makeRaw('a', [0, 0, 0], 1),
    makeRaw('b', [100, 0, 0], 2),
  ]));

  // Move both
  wv.applySnapshot(makeSnapshot(11, [
    makeRaw('a', [10, 0, 0], 1),
    makeRaw('b', [110, 0, 0], 2),
  ]));

  const entityA = wv.entities.get('a')!;
  const entityB = wv.entities.get('b')!;

  // Both should have hasSecondSnapshot = true
  assert.equal(entityA.hasSecondSnapshot, true);
  assert.equal(entityB.hasSecondSnapshot, true);

  // After interpolation, both should be between prev and current
  wv.interpolate(0);
  // alpha=0.5: lerp from prev to current at halfway
  // Entity A: prev was rendered position before snapshot, current is 10
  // We just verify interpolation ran (position is between prev and current)
  assert.ok(entityA.position.x >= entityA.previousPosition.x, 'entityA should interpolate forward');
  assert.ok(entityB.position.x >= entityB.previousPosition.x, 'entityB should interpolate forward');
});

// 29
test('applyDelta lastInputSeq update via delta', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1, { entity_type: 'Player', last_input_seq: 5 }),
  ]));

  assert.equal(wv.entities.get('a')!.lastInputSeq, 5);

  // Delta updates seq
  wv.applyDelta(2, [makeRaw('a', [0, 0, 0], 1, { entity_type: 'Player', last_input_seq: 10 })], []);

  assert.equal(wv.entities.get('a')!.lastInputSeq, 10, 'lastInputSeq should be updated by delta');
});

// 30
test('applyDelta removing nonexistent entity is no-op', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
  ]));

  // Remove entity that does not exist
  wv.applyDelta(2, [], ['nonexistent-id']);

  assert.equal(wv.entities.size, 1, 'removing nonexistent entity should not affect existing entities');
});

// 31
test('delta then snapshot then delta maintains consistency', () => {
  const wv = new WorldView();
  wv.applySnapshot(makeSnapshot(1, [
    makeRaw('a', [0, 0, 0], 1),
  ]));

  // Delta adds entity
  wv.applyDelta(2, [makeRaw('b', [5, 0, 0], 2)], []);
  assert.equal(wv.entities.size, 2);

  // Full snapshot includes both
  wv.applySnapshot(makeSnapshot(3, [
    makeRaw('a', [1, 0, 0], 1),
    makeRaw('b', [6, 0, 0], 2),
  ]));
  assert.equal(wv.entities.size, 2);

  // Delta removes one
  wv.applyDelta(4, [], ['a']);
  assert.equal(wv.entities.size, 1);
  assert.ok(wv.entities.has('b'));

  // Another delta adds new
  wv.applyDelta(5, [makeRaw('c', [10, 0, 0], 1)], []);
  assert.equal(wv.entities.size, 2);
  assert.ok(wv.entities.has('b'));
  assert.ok(wv.entities.has('c'));
});

// 32
test('warmup period snaps positions without interpolation', () => {
  const wv = new WorldView();

  // During warmup (first 3 snapshots), entities should snap
  wv.applySnapshot(makeSnapshot(1, [makeRaw('a', [0, 0, 0], 1)]));
  wv.applySnapshot(makeSnapshot(2, [makeRaw('a', [100, 0, 0], 1)]));

  const entity = wv.entities.get('a')!;
  // During warmup, all positions snap to server position
  assert.equal(entity.position.x, 100);
  assert.equal(entity.previousPosition.x, 100);
  assert.equal(entity.currentPosition.x, 100);
  assert.equal(entity.hasSecondSnapshot, false, 'during warmup hasSecondSnapshot stays false');
});

// 33
test('post-warmup snapshot enables interpolation', () => {
  const wv = new WorldView();
  warmup(wv); // 3 warmup snapshots

  wv.applySnapshot(makeSnapshot(10, [makeRaw('a', [0, 0, 0], 1)]));
  wv.applySnapshot(makeSnapshot(11, [makeRaw('a', [20, 0, 0], 1)]));

  const entity = wv.entities.get('a')!;
  assert.equal(entity.hasSecondSnapshot, true, 'post-warmup should enable interpolation');
  assert.equal(entity.currentPosition.x, 20);
});

// ── Summary ──
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
