import type { Scenario, ScenarioResult, TestContext, WorldSnapshot } from '../types.js';

export const meshVerify: Scenario = {
  name: 'mesh-verify',
  description: 'Deep verification of binary protocol, frame integrity, and mesh data flow',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

    // ── Step 1: Verify first frame has valid tick ──
    ctx.log('Waiting for initial snapshot...');
    const firstSnap = ctx.getLatestSnapshot();
    if (!firstSnap) {
      return {
        name: 'mesh-verify',
        status: 'fail',
        summary: 'No snapshot received from server',
        details: ['getLatestSnapshot() returned null — binary protocol may be broken'],
        durationMs: 0,
      };
    }

    if (firstSnap.tick <= 0) {
      details.push(`FAIL: first frame tick=${firstSnap.tick}, expected > 0`);
      return {
        name: 'mesh-verify',
        status: 'fail',
        summary: `First frame has invalid tick=${firstSnap.tick}`,
        details,
        durationMs: 0,
      };
    }
    details.push(`First frame: tick=${firstSnap.tick} entities=${firstSnap.entities.length}`);
    ctx.log(`First frame: tick=${firstSnap.tick} entities=${firstSnap.entities.length}`);

    // Ensure there are entities to test with — spawn some if the world is empty
    if (firstSnap.entities.length < 6) {
      ctx.log('World has few entities, spawning 20 NPCs for verification...');
      await ctx.spawn(20);
      await ctx.sleep(2000);
    }

    // ── Step 2: Verify entity field validity ──
    ctx.log('Checking entity field validity...');
    const snap = ctx.getLatestSnapshot();
    if (!snap || snap.entities.length === 0) {
      return {
        name: 'mesh-verify',
        status: 'fail',
        summary: 'No entities in snapshot after spawn',
        details,
        durationMs: 0,
      };
    }

    let invalidIds = 0;
    let invalidAuthority = 0;
    let invalidType = 0;
    for (const e of snap.entities) {
      if (!UUID_RE.test(e.id)) invalidIds++;
      if (e.authority_server <= 0) invalidAuthority++;
      if (e.entity_type !== 'Npc' && e.entity_type !== 'Player') invalidType++;
    }

    details.push(`Entity field check: ${snap.entities.length} entities`);
    details.push(`  invalid UUIDs: ${invalidIds}`);
    details.push(`  invalid authority_server (<=0): ${invalidAuthority}`);
    details.push(`  invalid entity_type: ${invalidType}`);

    if (invalidIds > 0 || invalidAuthority > 0) {
      return {
        name: 'mesh-verify',
        status: 'fail',
        summary: `Entity fields invalid: ${invalidIds} bad IDs, ${invalidAuthority} bad authority`,
        details,
        durationMs: 0,
      };
    }

    // ── Step 3: Verify entity count stability over 10 frames ──
    ctx.log('Checking entity count stability over 10 frames...');
    const counts: number[] = [];
    for (let i = 0; i < 10; i++) {
      await ctx.sleep(60);
      const s = ctx.getLatestSnapshot();
      if (s) {
        counts.push(s.entities.length);
        const serverCounts: Record<number, number> = {};
        for (const e of s.entities) {
          serverCounts[e.authority_server] = (serverCounts[e.authority_server] || 0) + 1;
        }
        ctx.log(`tick=${s.tick} entities=${s.entities.length} servers=${Object.keys(serverCounts).join(',')}`);
      }
    }

    if (counts.length < 5) {
      details.push('WARN: received fewer than 5 frames in stability window');
    }

    const minCount = Math.min(...counts);
    const maxCount = Math.max(...counts);
    const fluctuation = maxCount - minCount;
    details.push(`Count stability: min=${minCount} max=${maxCount} fluctuation=${fluctuation}`);

    // Allow some fluctuation from rebalancing, but flag wild swings
    const wildFluctuation = fluctuation > minCount * 0.5 && fluctuation > 10;
    if (wildFluctuation) {
      details.push('WARN: entity count fluctuating wildly (>50% swing)');
    }

    // ── Step 4: Verify multiple authority servers appear ──
    ctx.log('Checking for multi-server authority...');
    const latestSnap = ctx.getLatestSnapshot();
    const serverSet = new Set<number>();
    if (latestSnap) {
      for (const e of latestSnap.entities) {
        serverSet.add(e.authority_server);
      }
    }

    details.push(`Authority servers visible: [${[...serverSet].sort().join(', ')}]`);

    const multiServer = serverSet.size > 1;
    if (!multiServer && ctx.serverCount > 1) {
      details.push('WARN: only 1 authority server visible — shadow replication may not be working');
    }

    // ── Step 5: Verify ticks are monotonically advancing ──
    ctx.log('Checking tick monotonicity over 20 frames...');
    const ticks: number[] = [];
    let tickMonotonic = true;
    for (let i = 0; i < 20; i++) {
      await ctx.sleep(55);
      const s = ctx.getLatestSnapshot();
      if (s) {
        if (ticks.length > 0 && s.tick < ticks[ticks.length - 1]) {
          tickMonotonic = false;
          details.push(`FAIL: tick went backwards: ${ticks[ticks.length - 1]} -> ${s.tick}`);
        }
        ticks.push(s.tick);
      }
    }

    if (ticks.length >= 2) {
      details.push(`Tick range: ${ticks[0]} -> ${ticks[ticks.length - 1]} (${ticks.length} samples, monotonic=${tickMonotonic})`);
    }

    if (!tickMonotonic) {
      return {
        name: 'mesh-verify',
        status: 'fail',
        summary: 'Ticks are not monotonically advancing',
        details,
        durationMs: 0,
      };
    }

    // ── Step 6: Verify entity positions change (NPCs are moving) ──
    ctx.log('Checking for entity movement over 20 frames...');
    const startSnap = ctx.getLatestSnapshot();
    const startPositions = new Map<string, [number, number, number]>();
    if (startSnap) {
      for (const e of startSnap.entities) {
        startPositions.set(e.id, [...e.position]);
      }
    }

    await ctx.sleep(1200); // ~24 ticks at 20Hz

    const endSnap = ctx.getLatestSnapshot();
    let movedCount = 0;
    let checkedCount = 0;
    if (endSnap) {
      for (const e of endSnap.entities) {
        const start = startPositions.get(e.id);
        if (!start) continue;
        checkedCount++;
        const dx = Math.abs(e.position[0] - start[0]);
        const dz = Math.abs(e.position[2] - start[2]);
        if (dx > 0.01 || dz > 0.01) {
          movedCount++;
        }
      }
    }

    details.push(`Movement: ${movedCount}/${checkedCount} entities moved in 1.2s`);

    if (checkedCount > 0 && movedCount === 0) {
      details.push('WARN: no entities moved — NPC simulation may be stalled');
    }

    // ── Final verdict ──
    let status: ScenarioResult['status'] = 'pass';
    let warnings: string[] = [];

    if (wildFluctuation) warnings.push('entity count unstable');
    if (!multiServer && ctx.serverCount > 1) warnings.push('single authority server');
    if (checkedCount > 0 && movedCount === 0) warnings.push('no movement');

    if (warnings.length > 0) {
      status = 'warn';
    }

    const summary = status === 'pass'
      ? `Mesh OK: ${serverSet.size} servers, ${latestSnap?.entities.length ?? 0} entities, ticks monotonic, movement confirmed`
      : `Mesh issues: ${warnings.join(', ')}`;

    return {
      name: 'mesh-verify',
      status,
      summary,
      details,
      durationMs: 0,
    };
  },
};
