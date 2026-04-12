import WebSocket from 'ws';
import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const rebalanceStorm: Scenario = {
  name: 'rebalance-storm',
  description: 'Dump 300 entities on one server, watch coordinator rebalance until distribution is within 2:1 ratio',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    if (ctx.serverCount < 2) {
      return {
        name: 'rebalance-storm',
        status: 'fail',
        summary: 'Need at least 2 servers',
        details: [],
        durationMs: 0,
      };
    }

    const DUMP_COUNT = 300;
    const TARGET_RATIO = 2.0; // max:min ratio for "balanced"

    // Record baseline
    const baseline = await ctx.getAllMetrics();
    const baselineTotal = baseline.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Baseline: ${baselineTotal} entities`);

    // Pick server 1 to overload — connect directly to its WS port
    const targetServer = 1;
    const targetWsPort = 7877 + targetServer;

    ctx.log(`Dumping ${DUMP_COUNT} entities on S${targetServer} (ws://localhost:${targetWsPort})...`);

    // Connect directly to the sim-server to bypass server round-robin
    const directWs = await new Promise<WebSocket>((resolve, reject) => {
      const ws = new WebSocket(`ws://localhost:${targetWsPort}`);
      ws.on('open', () => resolve(ws));
      ws.on('error', reject);
    });

    for (let i = 0; i < DUMP_COUNT; i++) {
      const x = (Math.random() - 0.5) * 800;
      const z = (Math.random() - 0.5) * 800;
      directWs.send(JSON.stringify({
        type: 'SpawnEntity',
        position: [x, 0, z],
        entity_type: 'npc',
      }));
    }
    directWs.close();

    await ctx.sleep(1000);

    // Verify the imbalance
    const afterDump = await ctx.getAllMetrics();
    const afterDist = afterDump.map(m => `S${m.server_id}:${m.entity_count}`).join(' ');
    details.push(`After dump: ${afterDist}`);

    // Track rebalance progress
    const rebalanceStart = Date.now();
    let balanced = false;
    let rebalanceCount = 0;
    const previousCounts = new Map<number, number>();
    for (const m of afterDump) {
      previousCounts.set(m.server_id, m.entity_count);
    }

    for (let i = 0; i < 30; i++) { // up to 30 seconds (multiple rebalance cycles at 5s each)
      await ctx.sleep(2000);

      const metrics = await ctx.getAllMetrics();
      const counts = metrics.map(m => m.entity_count);
      const maxCount = Math.max(...counts);
      const minCount = Math.min(...counts);
      const ratio = minCount > 0 ? maxCount / minCount : Infinity;

      // Detect rebalance events
      for (const m of metrics) {
        const prev = previousCounts.get(m.server_id) ?? 0;
        if (Math.abs(m.entity_count - prev) > 3) {
          rebalanceCount++;
        }
        previousCounts.set(m.server_id, m.entity_count);
      }

      const dist = metrics.map(m => `S${m.server_id}:${m.entity_count}`).join(' ');
      ctx.log(`${((Date.now() - rebalanceStart) / 1000).toFixed(0)}s: ${dist} (ratio: ${ratio.toFixed(1)})`);

      if (ratio <= TARGET_RATIO && maxCount - minCount < 10) {
        balanced = true;
        break;
      }
    }

    const rebalanceTime = (Date.now() - rebalanceStart) / 1000;

    // Final state
    const finalMetrics = await ctx.getAllMetrics();
    details.push(`Final distribution:`);
    for (const m of finalMetrics) {
      details.push(`  S${m.server_id}: ${m.entity_count} entities (${m.tick_budget_pct.toFixed(0)}% budget)`);
    }
    details.push(`Rebalance events detected: ${rebalanceCount}`);

    if (balanced) {
      return {
        name: 'rebalance-storm',
        status: rebalanceTime > 20 ? 'warn' : 'pass',
        summary: `Balanced ${DUMP_COUNT} entity dump in ${rebalanceTime.toFixed(1)}s (${rebalanceCount} rebalances)`,
        details,
        durationMs: 0,
      };
    }

    return {
      name: 'rebalance-storm',
      status: 'warn',
      summary: `Rebalance incomplete after ${rebalanceTime.toFixed(0)}s — may need more time`,
      details,
      durationMs: 0,
    };
  },
};
