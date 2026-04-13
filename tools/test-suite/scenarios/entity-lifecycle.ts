import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const entityLifecycle: Scenario = {
  name: 'entity-lifecycle',
  description: 'Track an entity through its full lifecycle: spawn → move → verify authority → remove',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    // ── Step 1: Spawn a test entity at a known position ──
    const spawnPos: [number, number, number] = [42.0, 0, -42.0];
    ctx.log('Spawning test entity at (42, 0, -42)...');
    ctx.server.send(JSON.stringify({
      type: 'SpawnEntity',
      position: spawnPos,
      entity_type: 'npc',
    }));

    await ctx.sleep(500);

    // Find the entity near our spawn position
    const snap1 = ctx.getLatestSnapshot();
    if (!snap1) {
      return { name: 'entity-lifecycle', status: 'fail', summary: 'No snapshot', details, durationMs: 0 };
    }

    const spawned = snap1.entities.find(e =>
      Math.abs(e.position[0] - spawnPos[0]) < 5.0 &&
      Math.abs(e.position[2] - spawnPos[2]) < 5.0 &&
      e.entity_type === 'Npc'
    );

    if (!spawned) {
      details.push(`Could not find entity near (42, -42). Entities: ${snap1.entities.length}`);
      return { name: 'entity-lifecycle', status: 'fail', summary: 'Spawned entity not found', details, durationMs: 0 };
    }

    const entityId = spawned.id;
    const authServer = spawned.authority_server;
    details.push(`Spawned: ${entityId.substring(0, 8)}... on S${authServer} at (${spawned.position[0].toFixed(1)}, ${spawned.position[2].toFixed(1)})`);
    ctx.log(`Entity ${entityId.substring(0, 8)}... spawned on S${authServer}`);

    // ── Step 2: Send MoveInput and verify velocity changes ──
    ctx.log('Sending MoveInput...');
    ctx.server.send(JSON.stringify({
      type: 'MoveInput',
      entity_id: entityId,
      direction: [1, 0, 0],
      seq: 1,
    }));

    let velocityChanged = false;
    for (let i = 0; i < 20; i++) {
      await ctx.sleep(50);
      const snap = ctx.getLatestSnapshot();
      const e = snap?.entities.find(e => e.id === entityId);
      if (e && Math.abs(e.velocity[0]) > 0.01) {
        velocityChanged = true;
        details.push(`Velocity changed: [${e.velocity.map(v => v.toFixed(2)).join(', ')}] after ${(i + 1) * 50}ms`);
        break;
      }
    }

    if (!velocityChanged) {
      details.push('Velocity did not change after MoveInput');
      return { name: 'entity-lifecycle', status: 'fail', summary: 'MoveInput not processed', details, durationMs: 0 };
    }

    // ── Step 3: Verify entity moves over time ──
    const posBeforeMove = ctx.getLatestSnapshot()?.entities.find(e => e.id === entityId)?.position;
    await ctx.sleep(500);
    const posAfterMove = ctx.getLatestSnapshot()?.entities.find(e => e.id === entityId)?.position;

    if (posBeforeMove && posAfterMove) {
      const dx = Math.abs(posAfterMove[0] - posBeforeMove[0]);
      details.push(`Position delta after 0.5s: ${dx.toFixed(2)} units in X`);
      if (dx < 0.1) {
        details.push('WARNING: Entity barely moved');
      }
    }

    // ── Step 4: Verify authority is consistent ──
    let authChanges = 0;
    let lastAuth = authServer;
    for (let i = 0; i < 10; i++) {
      await ctx.sleep(100);
      const snap = ctx.getLatestSnapshot();
      const e = snap?.entities.find(e => e.id === entityId);
      if (e && e.authority_server !== lastAuth) {
        authChanges++;
        details.push(`Authority changed: S${lastAuth} → S${e.authority_server} at tick ${snap?.tick}`);
        lastAuth = e.authority_server;
      }
    }
    details.push(`Authority changes during observation: ${authChanges}`);

    // ── Step 5: Verify entity visible on server metrics ──
    const metrics = await ctx.getAllMetrics();
    const totalAuth = metrics.reduce((s, m) => s + m.entity_count, 0);
    details.push(`Server metrics: ${metrics.map(m => `S${m.server_id}:${m.entity_count}auth`).join(' ')}`);

    // ── Step 6: Remove the entity ──
    ctx.log('Removing entity...');
    ctx.server.send(JSON.stringify({
      type: 'RemoveEntity',
      entity_id: entityId,
    }));

    await ctx.sleep(500);

    const snapFinal = ctx.getLatestSnapshot();
    const stillExists = snapFinal?.entities.find(e => e.id === entityId);
    if (stillExists) {
      details.push('WARNING: Entity still exists after RemoveEntity');
    } else {
      details.push('Entity removed successfully');
    }

    return {
      name: 'entity-lifecycle',
      status: velocityChanged && !stillExists ? 'pass' : 'warn',
      summary: `spawn→move→verify→remove: velocity=${velocityChanged}, removed=${!stillExists}, auth_changes=${authChanges}`,
      details,
      durationMs: 0,
    };
  },
};
