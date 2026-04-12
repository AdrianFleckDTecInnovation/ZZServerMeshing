/**
 * Client-side prediction unit tests.
 *
 * Run: npx tsx tools/tests/prediction.test.ts
 */
import assert from 'node:assert/strict';

// ── Minimal Vector3 mock (matches THREE.Vector3 interface used by prediction) ──

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

// ── Re-implement PredictionManager to avoid THREE import ──

interface PendingInput {
  seq: number;
  direction: [number, number, number];
  dt: number;
}

interface EntityData {
  id: string;
  position: Vec3;
  currentPosition: Vec3;
  velocity: Vec3;
  entityType: 'Player' | 'Npc';
  authorityServer: number;
  lastInputSeq: number;
}

const PLAYER_SPEED = 5.0;
const MAX_PENDING = 128;
const CORRECTION_BLEND = 0.2;

class PredictionManager {
  private seq = 0;
  private pending: PendingInput[] = [];

  pushInput(direction: [number, number, number], entity: EntityData, dt: number): number {
    this.seq++;
    this.pending.push({ seq: this.seq, direction, dt });
    if (this.pending.length > MAX_PENDING) this.pending.shift();
    const len = Math.sqrt(direction[0] ** 2 + direction[1] ** 2 + direction[2] ** 2);
    if (len > 0) {
      entity.position.x += (direction[0] / len) * PLAYER_SPEED * dt;
      entity.position.z += (direction[2] / len) * PLAYER_SPEED * dt;
    }
    return this.seq;
  }

  reconcile(entity: EntityData, ackedSeq: number): void {
    this.pending = this.pending.filter((p) => p.seq > ackedSeq);
    let rx = entity.currentPosition.x;
    let rz = entity.currentPosition.z;
    for (const input of this.pending) {
      const [dx, , dz] = input.direction;
      const len = Math.sqrt(dx * dx + dz * dz);
      if (len > 0) {
        rx += (dx / len) * PLAYER_SPEED * input.dt;
        rz += (dz / len) * PLAYER_SPEED * input.dt;
      }
    }
    entity.position.x += (rx - entity.position.x) * CORRECTION_BLEND;
    entity.position.z += (rz - entity.position.z) * CORRECTION_BLEND;
    entity.position.y = 0;
  }

  get currentSeq(): number { return this.seq; }
  get pendingCount(): number { return this.pending.length; }
}

// ── Helpers ──

function makePlayer(): EntityData {
  return {
    id: 'test-player',
    position: new Vec3(),
    currentPosition: new Vec3(),
    velocity: new Vec3(),
    entityType: 'Player',
    authorityServer: 1,
    lastInputSeq: 0,
  };
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

console.log('prediction tests:');

test('pushInput increments seq', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  assert.equal(pm.pushInput([1, 0, 0], p, 1/60), 1);
  assert.equal(pm.pushInput([1, 0, 0], p, 1/60), 2);
  assert.equal(pm.pushInput([1, 0, 0], p, 1/60), 3);
  assert.equal(pm.currentSeq, 3);
});

test('pushInput applies local prediction', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  pm.pushInput([1, 0, 0], p, 1/60);
  assert.ok(p.position.x > 0, `expected x > 0, got ${p.position.x}`);
  assert.ok(Math.abs(p.position.z) < 0.001);
});

test('pushInput normalizes direction', () => {
  const pm1 = new PredictionManager();
  const p1 = makePlayer();
  pm1.pushInput([1, 0, 0], p1, 1/60);

  const pm2 = new PredictionManager();
  const p2 = makePlayer();
  pm2.pushInput([2, 0, 0], p2, 1/60);

  // Both should produce the same displacement (direction is normalized)
  assert.ok(Math.abs(p1.position.x - p2.position.x) < 0.001);
});

test('pushInput diagonal direction', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  pm.pushInput([1, 0, 1], p, 1/60);
  // Both x and z should move, by equal amounts (45 degrees)
  assert.ok(p.position.x > 0);
  assert.ok(p.position.z > 0);
  assert.ok(Math.abs(p.position.x - p.position.z) < 0.001);
});

test('pushInput zero direction no movement', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  pm.pushInput([0, 0, 0], p, 1/60);
  assert.equal(p.position.x, 0);
  assert.equal(p.position.z, 0);
});

test('pushInput caps pending buffer', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  for (let i = 0; i < 150; i++) {
    pm.pushInput([1, 0, 0], p, 1/60);
  }
  assert.equal(pm.pendingCount, MAX_PENDING);
  assert.equal(pm.currentSeq, 150);
});

test('reconcile drops acknowledged inputs', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  assert.equal(pm.pendingCount, 5);

  pm.reconcile(p, 3); // ack first 3
  assert.equal(pm.pendingCount, 2);
});

test('reconcile with all inputs acknowledged', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.reconcile(p, 5); // ack more than exists
  assert.equal(pm.pendingCount, 0);
});

test('reconcile blends toward server position', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  // Client predicted to x=10
  p.position.x = 10;
  // Server says x=8
  p.currentPosition.x = 8;

  pm.reconcile(p, 0); // no pending inputs

  // Should blend 20% toward server: 10 + (8 - 10) * 0.2 = 9.6
  assert.ok(Math.abs(p.position.x - 9.6) < 0.01, `expected ~9.6, got ${p.position.x}`);
});

test('reconcile replays unacknowledged inputs on server position', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  const dt = 1/60;

  // Push 3 inputs
  pm.pushInput([1, 0, 0], p, dt);
  pm.pushInput([1, 0, 0], p, dt);
  pm.pushInput([1, 0, 0], p, dt);

  // Server acks input 1, with position at x=0.08 (moved a bit)
  p.currentPosition.x = 0.08;
  const posBefore = p.position.x;

  pm.reconcile(p, 1); // 2 inputs remain

  // Position should have moved (blended toward server + replay of 2 inputs)
  // The exact value depends on CORRECTION_BLEND, but it should be non-zero
  assert.ok(p.position.x > 0, `position should be positive after reconcile`);
});

test('reconcile sets y to zero', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  p.position.y = 5;
  pm.reconcile(p, 0);
  assert.equal(p.position.y, 0);
});

test('currentSeq returns latest seq', () => {
  const pm = new PredictionManager();
  const p = makePlayer();
  assert.equal(pm.currentSeq, 0);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  pm.pushInput([1, 0, 0], p, 1/60);
  assert.equal(pm.currentSeq, 5);
});

// ── Summary ──
console.log(`\n${passed + failed} tests: ${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
