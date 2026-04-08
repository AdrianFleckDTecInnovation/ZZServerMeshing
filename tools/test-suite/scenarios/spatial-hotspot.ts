import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const spatialHotspot: Scenario = {
  name: 'spatial-hotspot',
  description: 'Cluster 200 entities in a 50x50 area to stress UDP replication packet sizes',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const ENTITY_COUNT = 200;
    const HOTSPOT_SIZE = 50;
    const HOTSPOT_CENTER = { x: 100, z: 100 };

    // Record baseline
    const baseline = await ctx.getAllMetrics();
    const baselineTotal = baseline.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Baseline: ${baselineTotal} entities`);

    // Spawn all entities in a tight cluster
    ctx.log(`Spawning ${ENTITY_COUNT} entities in a ${HOTSPOT_SIZE}x${HOTSPOT_SIZE} area...`);

    for (let i = 0; i < ENTITY_COUNT; i++) {
      const x = HOTSPOT_CENTER.x + (Math.random() - 0.5) * HOTSPOT_SIZE;
      const z = HOTSPOT_CENTER.z + (Math.random() - 0.5) * HOTSPOT_SIZE;
      ctx.gateway.send(JSON.stringify({
        type: 'SpawnEntity',
        position: [x, 0, z],
        entity_type: 'npc',
      }));
    }

    // Wait for entities to settle
    await ctx.sleep(3000);

    // Measure impact
    const metrics = await ctx.getAllMetrics();
    const totalEntities = metrics.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Total entities: ${totalEntities} (${ENTITY_COUNT} in hotspot)`);

    // Check replication phase — this is where packet size issues surface
    let worstReplication = 0;
    let worstServer = 0;
    for (const m of metrics) {
      if (m.phases.replication_ms.p95 > worstReplication) {
        worstReplication = m.phases.replication_ms.p95;
        worstServer = m.server_id;
      }
    }

    details.push(`Replication bottleneck: S${worstServer} at ${worstReplication.toFixed(1)}ms p95`);

    // Full phase breakdown for worst server
    const worst = metrics.find(m => m.server_id === worstServer)!;
    const totalP95 = worst.phases.total_ms.p95;
    details.push(`S${worstServer} phase breakdown (p95):`);
    details.push(`  simulation:    ${worst.phases.simulation_ms.p95.toFixed(1)}ms`);
    details.push(`  replication:   ${worst.phases.replication_ms.p95.toFixed(1)}ms`);
    details.push(`  transfers:     ${worst.phases.transfers_ms.p95.toFixed(1)}ms`);
    details.push(`  input_drain:   ${worst.phases.input_drain_ms.p95.toFixed(1)}ms`);
    details.push(`  shadow_clean:  ${worst.phases.shadow_cleanup_ms.p95.toFixed(1)}ms`);
    details.push(`  total:         ${totalP95.toFixed(1)}ms (${worst.tick_budget_pct.toFixed(0)}% budget)`);

    // Check if any server is struggling (>60% budget)
    const maxBudget = Math.max(...metrics.map(m => m.tick_budget_pct));

    // Distribution — with a hotspot, one server may end up with most via round-robin,
    // or rebalancing may have spread them
    details.push(`Server distribution:`);
    for (const m of metrics) {
      details.push(`  S${m.server_id}: ${m.entity_count} auth + ${m.shadow_count} shadow`);
    }

    // Check shadow counts — every server should see ~ENTITY_COUNT shadows from peers
    const maxShadow = Math.max(...metrics.map(m => m.shadow_count));
    if (maxShadow > 0) {
      details.push(`Max shadow entities: ${maxShadow} (indicates UDP replication is working)`);
    }

    let status: ScenarioResult['status'] = 'pass';
    let summary: string;

    if (maxBudget > 80) {
      status = 'warn';
      summary = `Hotspot caused ${maxBudget.toFixed(0)}% tick budget — replication: ${worstReplication.toFixed(1)}ms p95`;
    } else {
      summary = `${ENTITY_COUNT} clustered entities handled, peak budget: ${maxBudget.toFixed(0)}%`;
    }

    return { name: 'spatial-hotspot', status, summary, details, durationMs: 0 };
  },
};
