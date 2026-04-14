import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const chunkedReplication: Scenario = {
  name: 'chunked-replication',
  description: 'Verify UDP replication chunking works for >100 entities (breaks old 60KB limit)',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const start = Date.now();

    if (ctx.serverCount < 2) {
      return {
        name: 'chunked-replication',
        status: 'fail',
        summary: 'Need at least 2 servers for chunked replication test',
        details: ['serverCount < 2'],
        durationMs: 0,
      };
    }

    // Step 1: Record baseline
    let metrics = await ctx.getAllMetrics();
    const baselineAuth = metrics.map(m => ({ id: m.server_id, auth: m.entity_count, shadow: m.shadow_count }));
    details.push(`Baseline: ${JSON.stringify(baselineAuth)}`);

    // Step 2: Spawn 200 entities (forces >1 chunk at 100 per chunk)
    ctx.log('Spawning 200 entities for chunk test...');
    await ctx.spawn(200);
    await ctx.sleep(3000); // let replication propagate

    metrics = await ctx.getAllMetrics();
    const totalAuth = metrics.reduce((sum, m) => sum + m.entity_count, 0);
    const maxShadow = Math.max(...metrics.map(m => m.shadow_count));
    details.push(`After 200 spawn: total_auth=${totalAuth}, max_shadow=${maxShadow}`);
    ctx.log(`Total auth: ${totalAuth}, max shadow on any server: ${maxShadow}`);

    // Verify shadows replicated (at least 150 to account for distribution)
    const minExpectedShadows = 150;
    if (maxShadow < minExpectedShadows) {
      details.push(`FAIL: max shadow count ${maxShadow} < ${minExpectedShadows} expected`);
      return {
        name: 'chunked-replication',
        status: 'fail',
        summary: `Chunked replication failed: only ${maxShadow} shadows visible (expected >= ${minExpectedShadows})`,
        details,
        durationMs: Date.now() - start,
      };
    }

    details.push(`Step 1 passed: ${maxShadow} shadows visible (>= ${minExpectedShadows})`);

    // Step 3: Spawn more to push well past old 60KB limit (total >1000)
    ctx.log('Spawning 900 more entities to exceed old 60KB UDP limit...');
    for (let i = 0; i < 9; i++) {
      await ctx.spawn(100);
      await ctx.sleep(200);
    }
    await ctx.sleep(3000); // let replication propagate

    metrics = await ctx.getAllMetrics();
    const totalAuth2 = metrics.reduce((sum, m) => sum + m.entity_count, 0);
    const maxShadow2 = Math.max(...metrics.map(m => m.shadow_count));
    details.push(`After 1100 spawn: total_auth=${totalAuth2}, max_shadow=${maxShadow2}`);
    ctx.log(`Total auth: ${totalAuth2}, max shadow on any server: ${maxShadow2}`);

    // Verify shadows replicated at scale (at least 800)
    const minExpectedShadows2 = 800;
    if (maxShadow2 < minExpectedShadows2) {
      details.push(`FAIL: max shadow count ${maxShadow2} < ${minExpectedShadows2} at scale`);
      return {
        name: 'chunked-replication',
        status: 'fail',
        summary: `Chunked replication at scale failed: only ${maxShadow2} shadows (expected >= ${minExpectedShadows2})`,
        details,
        durationMs: Date.now() - start,
      };
    }

    details.push(`Step 2 passed: ${maxShadow2} shadows visible at scale (>= ${minExpectedShadows2})`);

    return {
      name: 'chunked-replication',
      status: 'pass',
      summary: `Chunked replication works: ${maxShadow} shadows at 200 entities, ${maxShadow2} shadows at ${totalAuth2} entities`,
      details,
      durationMs: Date.now() - start,
    };
  },
};
