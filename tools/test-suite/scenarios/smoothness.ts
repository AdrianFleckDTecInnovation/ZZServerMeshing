import type { Scenario, ScenarioResult, TestContext } from '../types.js';

/**
 * Headless smoothness validator.
 * Connects to the server, collects consecutive snapshots, and checks:
 * 1. Entity positions change smoothly (no teleportation > threshold per tick)
 * 2. Entity count is stable (no entities vanishing/reappearing)
 * 3. All servers maintain entities (no server draining to 0 unexpectedly)
 * 4. Snapshot rate is stable (~20Hz)
 */
export const smoothness: Scenario = {
  name: 'smoothness',
  description: 'Validate entity movement is smooth with no jitter, warping, or entity count instability',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const SAMPLE_DURATION_MS = 10000; // 10 seconds of observation
    const MAX_POSITION_JUMP = 5.0; // max units an entity should move per tick at 20Hz
    // NPCs move at 1-3 units/sec → 0.05-0.15 per tick. 5.0 is very generous.

    // Collect snapshots for analysis
    interface SnapRecord {
      tick: number;
      time: number;
      entityCount: number;
      entities: Map<string, { x: number; z: number; server: number }>;
    }

    const snapshots: SnapRecord[] = [];
    let lastSnapshot = ctx.getLatestSnapshot();

    ctx.log('Collecting snapshots for 10 seconds...');

    const startTime = Date.now();
    while (Date.now() - startTime < SAMPLE_DURATION_MS) {
      await ctx.sleep(45); // slightly less than 50ms tick to catch every one
      const snap = ctx.getLatestSnapshot();
      if (!snap || snap === lastSnapshot) continue;
      if (snap.tick === (snapshots.length > 0 ? snapshots[snapshots.length - 1].tick : -1)) continue;

      const entities = new Map<string, { x: number; z: number; server: number }>();
      for (const e of snap.entities) {
        entities.set(e.id, {
          x: e.position[0],
          z: e.position[2],
          server: e.authority_server,
        });
      }

      snapshots.push({
        tick: snap.tick,
        time: Date.now(),
        entityCount: snap.entities.length,
        entities,
      });

      lastSnapshot = snap;
    }

    if (snapshots.length < 10) {
      return {
        name: 'smoothness',
        status: 'fail',
        summary: `Only received ${snapshots.length} snapshots in 10s`,
        details: ['Not enough data to analyze'],
        durationMs: 0,
      };
    }

    details.push(`Collected ${snapshots.length} snapshots over ${((Date.now() - startTime) / 1000).toFixed(1)}s`);

    // 1. Check snapshot rate
    const intervals: number[] = [];
    for (let i = 1; i < snapshots.length; i++) {
      intervals.push(snapshots[i].time - snapshots[i - 1].time);
    }
    const avgInterval = intervals.reduce((a, b) => a + b, 0) / intervals.length;
    const maxInterval = Math.max(...intervals);
    const minInterval = Math.min(...intervals);
    const snapshotRate = 1000 / avgInterval;
    details.push(`Snapshot rate: ${snapshotRate.toFixed(1)} Hz (avg interval: ${avgInterval.toFixed(0)}ms, min: ${minInterval}ms, max: ${maxInterval}ms)`);

    // 2. Check entity count stability
    const entityCounts = snapshots.map(s => s.entityCount);
    const minEntities = Math.min(...entityCounts);
    const maxEntities = Math.max(...entityCounts);
    const entityCountSwing = maxEntities - minEntities;
    details.push(`Entity count: min=${minEntities} max=${maxEntities} swing=${entityCountSwing}`);

    // 3. Check per-server stability
    const serverCounts = new Map<number, number[]>();
    for (const snap of snapshots) {
      const counts = new Map<number, number>();
      for (const e of snap.entities.values()) {
        counts.set(e.server, (counts.get(e.server) || 0) + 1);
      }
      for (const [sid, count] of counts) {
        if (!serverCounts.has(sid)) serverCounts.set(sid, []);
        serverCounts.get(sid)!.push(count);
      }
    }
    let serverDrainDetected = false;
    for (const [sid, counts] of serverCounts) {
      const min = Math.min(...counts);
      const max = Math.max(...counts);
      details.push(`  S${sid}: min=${min} max=${max} swing=${max - min}`);
      if (min === 0 && max > 5) {
        serverDrainDetected = true;
        details.push(`  *** S${sid} drained to 0 entities — unexpected!`);
      }
    }

    // 4. Check position continuity (the big one)
    let totalJumps = 0;
    let totalChecks = 0;
    let worstJump = 0;
    let worstJumpEntity = '';
    let worstJumpTick = 0;

    for (let i = 1; i < snapshots.length; i++) {
      const prev = snapshots[i - 1];
      const curr = snapshots[i];
      const tickDelta = curr.tick - prev.tick;

      // Scale threshold by tick delta (if we missed ticks, allow proportionally more movement)
      const threshold = MAX_POSITION_JUMP * Math.max(tickDelta, 1);

      for (const [id, pos] of curr.entities) {
        const prevPos = prev.entities.get(id);
        if (!prevPos) continue; // new entity, can't check

        const dx = Math.abs(pos.x - prevPos.x);
        const dz = Math.abs(pos.z - prevPos.z);
        const dist = Math.sqrt(dx * dx + dz * dz);

        totalChecks++;
        if (dist > threshold) {
          totalJumps++;
          if (dist > worstJump) {
            worstJump = dist;
            worstJumpEntity = id.substring(0, 8);
            worstJumpTick = curr.tick;
          }
        }
      }
    }

    const jumpRate = totalChecks > 0 ? (totalJumps / totalChecks * 100) : 0;
    details.push(`Position checks: ${totalChecks}, jumps detected: ${totalJumps} (${jumpRate.toFixed(2)}%)`);
    if (worstJump > 0) {
      details.push(`Worst jump: ${worstJump.toFixed(1)} units (entity ${worstJumpEntity} at tick ${worstJumpTick})`);
    }

    // 5. Check for entities flickering (appearing/disappearing)
    let flickerCount = 0;
    for (let i = 2; i < snapshots.length; i++) {
      const prev2 = snapshots[i - 2];
      const prev1 = snapshots[i - 1];
      const curr = snapshots[i];

      for (const id of curr.entities.keys()) {
        // Entity exists in i-2 and i, but not i-1 → flicker
        if (prev2.entities.has(id) && !prev1.entities.has(id)) {
          flickerCount++;
        }
      }
    }
    details.push(`Entity flicker events: ${flickerCount}`);

    // Verdict
    let status: ScenarioResult['status'] = 'pass';
    let summary: string;

    const problems: string[] = [];
    if (jumpRate > 1.0) problems.push(`${jumpRate.toFixed(1)}% position jumps`);
    if (entityCountSwing > maxEntities * 0.1) problems.push(`entity count swing: ${entityCountSwing}`);
    if (serverDrainDetected) problems.push('server drained to 0');
    if (flickerCount > 5) problems.push(`${flickerCount} flicker events`);
    if (snapshotRate < 15) problems.push(`low snapshot rate: ${snapshotRate.toFixed(1)} Hz`);
    if (maxInterval > 200) problems.push(`snapshot gap: ${maxInterval}ms`);

    if (problems.length === 0) {
      summary = `Smooth: ${snapshots.length} snapshots, ${totalChecks} position checks, 0 issues`;
    } else {
      status = problems.some(p => p.includes('jumps') || p.includes('drained')) ? 'fail' : 'warn';
      summary = problems.join(', ');
    }

    return { name: 'smoothness', status, summary, details, durationMs: 0 };
  },
};
