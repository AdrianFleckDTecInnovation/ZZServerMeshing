import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const steadyScale: Scenario = {
  name: 'steady-scale',
  description: 'Add 10 entities/sec until a server hits 80% tick budget — find the entity ceiling',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const SPAWN_RATE = 100; // per second
    const BUDGET_CEILING = 80; // percent
    const MAX_ENTITIES = 10000; // safety cap
    const CHECK_INTERVAL_MS = 1000;

    // Record baseline
    const baselineMetrics = await ctx.getAllMetrics();
    const baselineEntities = baselineMetrics.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Baseline: ${baselineEntities} entities`);

    let totalSpawned = 0;
    let ceilingHit = false;
    let ceilingEntityCount = 0;
    let ceilingMetrics: Awaited<ReturnType<typeof ctx.getAllMetrics>> | null = null;

    ctx.log(`Scaling at ${SPAWN_RATE} entities/sec...`);

    while (totalSpawned < MAX_ENTITIES) {
      // Spawn a batch
      await ctx.spawn(SPAWN_RATE);
      totalSpawned += SPAWN_RATE;

      await ctx.sleep(CHECK_INTERVAL_MS);

      // Check metrics
      const metrics = await ctx.getAllMetrics();
      const maxBudget = Math.max(...metrics.map(m => m.tick_budget_pct));
      const totalEntities = metrics.reduce((sum, m) => sum + m.entity_count, 0);

      if (totalSpawned % 50 === 0) {
        ctx.log(`${totalEntities} entities, max tick budget: ${maxBudget.toFixed(0)}%`);
      }

      if (maxBudget >= BUDGET_CEILING) {
        ceilingHit = true;
        ceilingEntityCount = totalEntities;
        ceilingMetrics = metrics;
        break;
      }
    }

    if (!ceilingHit) {
      details.push(`Reached ${MAX_ENTITIES} entities without hitting ${BUDGET_CEILING}% budget`);
      const finalMetrics = await ctx.getAllMetrics();
      const maxBudget = Math.max(...finalMetrics.map(m => m.tick_budget_pct));
      details.push(`Final tick budget: ${maxBudget.toFixed(0)}%`);

      return {
        name: 'steady-scale',
        status: 'pass',
        summary: `Cluster handled ${MAX_ENTITIES}+ entities without hitting budget ceiling`,
        details,
        durationMs: 0,
      };
    }

    details.push(`Entity ceiling: ${ceilingEntityCount} entities`);

    // Analyze bottleneck at ceiling
    const worstServer = ceilingMetrics!.reduce((a, b) =>
      a.tick_budget_pct > b.tick_budget_pct ? a : b
    );

    const totalP95 = worstServer.phases.total_ms.p95;
    const phases = [
      { name: 'simulation', ms: worstServer.phases.simulation_ms.p95 },
      { name: 'replication', ms: worstServer.phases.replication_ms.p95 },
      { name: 'transfers', ms: worstServer.phases.transfers_ms.p95 },
      { name: 'input_drain', ms: worstServer.phases.input_drain_ms.p95 },
      { name: 'shadow_cleanup', ms: worstServer.phases.shadow_cleanup_ms.p95 },
    ].sort((a, b) => b.ms - a.ms);

    details.push(`Bottleneck server: S${worstServer.server_id} at ${worstServer.tick_budget_pct.toFixed(0)}% budget`);
    details.push(`Phase breakdown at ceiling (p95):`);
    for (const phase of phases) {
      const pct = totalP95 > 0 ? (phase.ms / totalP95 * 100).toFixed(0) : '0';
      const bar = '█'.repeat(Math.round(phase.ms / totalP95 * 20));
      details.push(`  ${phase.name.padEnd(16)} ${phase.ms.toFixed(1)}ms (${pct}%) ${bar}`);
    }

    // Server distribution at ceiling
    const dist = ceilingMetrics!.map(m =>
      `S${m.server_id}: ${m.entity_count} entities (${m.tick_budget_pct.toFixed(0)}% budget)`
    );
    details.push(`Server distribution:`);
    for (const d of dist) {
      details.push(`  ${d}`);
    }

    return {
      name: 'steady-scale',
      status: 'warn',
      summary: `Ceiling at ${ceilingEntityCount} entities — bottleneck: ${phases[0].name}`,
      details,
      durationMs: 0,
    };
  },
};
