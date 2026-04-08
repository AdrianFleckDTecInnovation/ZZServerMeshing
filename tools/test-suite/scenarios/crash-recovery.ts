import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const crashRecovery: Scenario = {
  name: 'crash-recovery',
  description: 'Kill a sim-server and verify entities are recovered by survivors within 10 seconds',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    // Need at least 2 servers
    if (ctx.serverCount < 2) {
      return {
        name: 'crash-recovery',
        status: 'fail',
        summary: 'Need at least 2 servers for crash recovery test',
        details: [],
        durationMs: 0,
      };
    }

    // Ensure there are entities to recover — spawn some if needed
    const baseline = await ctx.getAllMetrics();
    const totalBaseline = baseline.reduce((sum, m) => sum + m.entity_count, 0);
    if (totalBaseline < 30) {
      ctx.log('Spawning entities for crash recovery test...');
      await ctx.spawn(60);
      await ctx.sleep(3000); // let them distribute
    }

    // Snapshot before crash
    const preCrash = await ctx.getAllMetrics();
    const totalPreCrash = preCrash.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Pre-crash: ${totalPreCrash} entities across ${preCrash.length} servers`);
    for (const m of preCrash) {
      details.push(`  S${m.server_id}: ${m.entity_count} entities`);
    }

    // Pick the server with the most entities to crash (maximize impact)
    const target = preCrash.reduce((a, b) => a.entity_count > b.entity_count ? a : b);
    const targetId = target.server_id;
    const targetEntities = target.entity_count;
    details.push(`Killing S${targetId} (${targetEntities} entities)...`);

    ctx.log(`Killing sim-server ${targetId}...`);
    const { execSync } = await import('child_process');

    // Try Docker first, then local process kill
    let killed = false;
    try {
      // Docker: stop the container
      execSync(`docker compose -f docker-compose.test.yml stop sim-server-${targetId} 2>/dev/null`, {
        timeout: 5000,
        cwd: process.cwd().replace(/\/tools$/, ''),
      });
      killed = true;
      details.push(`Killed via docker compose stop`);
    } catch {}

    if (!killed) {
      try {
        // Local: kill processes by port
        const wsPort = 7877 + targetId;
        execSync(`lsof -ti tcp:${wsPort} | xargs kill -9 2>/dev/null || true`);
        killed = true;
        details.push(`Killed via local process signal`);
      } catch {
        details.push('Warning: Could not kill server process');
      }
    }

    // Wait for coordinator to detect the crash (5s heartbeat timeout) + recovery
    const recoveryStart = Date.now();
    let recovered = false;
    let recoveredEntities = 0;
    const survivorCount = ctx.serverCount - 1;

    for (let i = 0; i < 20; i++) { // up to 20 seconds
      await ctx.sleep(1000);

      // Fetch metrics from survivors only
      const survivors: Awaited<ReturnType<typeof ctx.getAllMetrics>> = [];
      for (let s = 1; s <= ctx.serverCount; s++) {
        if (s === targetId) continue;
        try {
          const m = await ctx.getMetrics(s);
          survivors.push(m);
        } catch {}
      }

      const survivorEntities = survivors.reduce((sum, m) => sum + m.entity_count, 0);
      ctx.log(`${((Date.now() - recoveryStart) / 1000).toFixed(0)}s: survivors have ${survivorEntities} entities (need ~${totalPreCrash})`);

      // Recovery is successful if survivors have at least 90% of pre-crash entities
      if (survivorEntities >= totalPreCrash * 0.9) {
        recovered = true;
        recoveredEntities = survivorEntities;
        break;
      }
    }

    const recoveryTime = (Date.now() - recoveryStart) / 1000;

    if (recovered) {
      details.push(`Recovery: ${recoveryTime.toFixed(1)}s — ${recoveredEntities} entities on ${survivorCount} survivors`);

      // Show final distribution
      for (let s = 1; s <= ctx.serverCount; s++) {
        if (s === targetId) continue;
        try {
          const m = await ctx.getMetrics(s);
          details.push(`  S${m.server_id}: ${m.entity_count} entities (${m.tick_budget_pct.toFixed(0)}% budget)`);
        } catch {}
      }

      return {
        name: 'crash-recovery',
        status: recoveryTime > 10 ? 'warn' : 'pass',
        summary: `S${targetId} crash recovered in ${recoveryTime.toFixed(1)}s (${recoveredEntities}/${totalPreCrash} entities)`,
        details,
        durationMs: 0,
      };
    }

    return {
      name: 'crash-recovery',
      status: 'fail',
      summary: `Recovery failed — entities not adopted within 20s after S${targetId} crash`,
      details,
      durationMs: 0,
    };
  },
};
