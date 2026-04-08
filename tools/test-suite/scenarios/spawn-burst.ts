import type { Scenario, ScenarioResult, TestContext, ServerMetrics } from '../types.js';

export const spawnBurst: Scenario = {
  name: 'spawn-burst',
  description: 'Spawn 500 entities in 2 seconds, measure tick budget impact and recovery time',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const ENTITY_COUNT = 500;
    const BURST_DURATION_MS = 2000;
    const BATCH_SIZE = 50;
    const BATCH_INTERVAL = BURST_DURATION_MS / (ENTITY_COUNT / BATCH_SIZE); // ~200ms

    // Record baseline
    const baselineMetrics = await ctx.getAllMetrics();
    const baselineEntities = baselineMetrics.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Baseline: ${baselineEntities} entities across ${baselineMetrics.length} servers`);

    // Spawn in bursts
    const spawnStart = Date.now();
    let spawned = 0;
    while (spawned < ENTITY_COUNT) {
      const batch = Math.min(BATCH_SIZE, ENTITY_COUNT - spawned);
      await ctx.spawn(batch);
      spawned += batch;
      if (spawned < ENTITY_COUNT) {
        await ctx.sleep(BATCH_INTERVAL);
      }
    }
    const spawnDuration = Date.now() - spawnStart;
    details.push(`Spawned ${ENTITY_COUNT} entities in ${(spawnDuration / 1000).toFixed(1)}s`);

    // Measure peak tick budget right after burst
    await ctx.sleep(500);
    const peakMetrics = await ctx.getAllMetrics();
    const peakBudgets = peakMetrics.map(m => m.tick_budget_pct);
    const maxPeakBudget = Math.max(...peakBudgets);
    details.push(`Peak tick budget: ${maxPeakBudget.toFixed(1)}% (${peakBudgets.map((b, i) => `S${i + 1}:${b.toFixed(0)}%`).join(' ')})`);

    // Identify bottleneck phase at peak
    const worstServer = peakMetrics.reduce((a, b) => a.tick_budget_pct > b.tick_budget_pct ? a : b);
    const phases = worstServer.phases;
    const phaseBreakdown = [
      { name: 'simulation', p95: phases.simulation_ms.p95 },
      { name: 'replication', p95: phases.replication_ms.p95 },
      { name: 'transfers', p95: phases.transfers_ms.p95 },
      { name: 'input_drain', p95: phases.input_drain_ms.p95 },
      { name: 'shadow_cleanup', p95: phases.shadow_cleanup_ms.p95 },
    ].sort((a, b) => b.p95 - a.p95);

    details.push(`Bottleneck: ${phaseBreakdown[0].name} (p95=${phaseBreakdown[0].p95.toFixed(1)}ms on S${worstServer.server_id})`);

    // Wait for recovery (tick budget < 50%)
    const recoveryStart = Date.now();
    let recovered = false;
    for (let i = 0; i < 20; i++) { // up to 20 seconds
      await ctx.sleep(1000);
      const metrics = await ctx.getAllMetrics();
      const maxBudget = Math.max(...metrics.map(m => m.tick_budget_pct));
      if (maxBudget < 50) {
        recovered = true;
        break;
      }
    }
    const recoveryTime = (Date.now() - recoveryStart) / 1000;

    if (recovered) {
      details.push(`Recovery: ${recoveryTime.toFixed(1)}s to <50% tick budget`);
    } else {
      details.push(`Recovery: did NOT recover within 20s`);
    }

    // Check entity distribution
    const finalMetrics = await ctx.getAllMetrics();
    const dist = finalMetrics.map(m => `S${m.server_id}:${m.entity_count}`).join(' ');
    details.push(`Final distribution: ${dist}`);

    const totalEntities = finalMetrics.reduce((sum, m) => sum + m.entity_count, 0);
    const expectedTotal = baselineEntities + ENTITY_COUNT;
    if (totalEntities < expectedTotal * 0.9) {
      details.push(`Warning: expected ~${expectedTotal} entities but got ${totalEntities}`);
    }

    // Determine status
    let status: ScenarioResult['status'] = 'pass';
    let summary: string;

    if (!recovered) {
      status = 'fail';
      summary = `Cluster did not recover from ${ENTITY_COUNT} entity burst`;
    } else if (maxPeakBudget > 90) {
      status = 'warn';
      summary = `Burst handled but tick budget peaked at ${maxPeakBudget.toFixed(0)}%`;
    } else {
      summary = `${ENTITY_COUNT} entities absorbed, recovered in ${recoveryTime.toFixed(1)}s`;
    }

    return { name: 'spawn-burst', status, summary, details, durationMs: 0 };
  },
};
