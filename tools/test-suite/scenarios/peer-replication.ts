import type { Scenario, ScenarioResult, TestContext, ServerMetrics } from '../types.js';

export const peerReplication: Scenario = {
  name: 'peer-replication',
  description: 'Verify entities are replicated as shadows across all peers via metrics endpoints',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    if (ctx.serverCount < 2) {
      return {
        name: 'peer-replication',
        status: 'fail',
        summary: 'Need at least 2 servers for peer replication test',
        details: ['serverCount < 2'],
        durationMs: 0,
      };
    }

    // Ensure there are entities to replicate
    ctx.log('Checking baseline entity count...');
    let metrics = await ctx.getAllMetrics();
    const totalAuth = metrics.reduce((sum, m) => sum + m.entity_count, 0);

    if (totalAuth < 10) {
      ctx.log('Few entities in the world, spawning 30 NPCs...');
      await ctx.spawn(30);
      await ctx.sleep(3000); // let replication propagate
      metrics = await ctx.getAllMetrics();
    }

    if (metrics.length === 0) {
      return {
        name: 'peer-replication',
        status: 'fail',
        summary: 'Could not fetch metrics from any server',
        details: ['getAllMetrics() returned empty — are servers running?'],
        durationMs: 0,
      };
    }

    // ── Step 1: Log each server's metrics ──
    ctx.log(`Fetched metrics from ${metrics.length} servers`);
    for (const m of metrics) {
      const line = `S${m.server_id}: auth=${m.entity_count} shadows=${m.shadow_count} peers=${m.peer_count} budget=${m.tick_budget_pct.toFixed(0)}%`;
      details.push(line);
      ctx.log(line);
    }

    // ── Step 2: Verify peer_count == serverCount - 1 ──
    ctx.log('Checking peer counts...');
    const expectedPeers = ctx.serverCount - 1;
    let peerCountOk = true;
    for (const m of metrics) {
      if (m.peer_count !== expectedPeers) {
        details.push(`WARN: S${m.server_id} has peer_count=${m.peer_count}, expected ${expectedPeers}`);
        peerCountOk = false;
      }
    }

    if (peerCountOk) {
      details.push(`Peer count: all servers report ${expectedPeers} peers (correct)`);
    }

    // ── Step 3: Verify shadow_count > 0 on each server ──
    ctx.log('Checking shadow counts...');
    let allHaveShadows = true;
    const serversWithoutShadows: number[] = [];
    for (const m of metrics) {
      if (m.shadow_count === 0) {
        allHaveShadows = false;
        serversWithoutShadows.push(m.server_id);
      }
    }

    if (allHaveShadows) {
      details.push('Shadow replication: all servers have shadow_count > 0');
    } else {
      details.push(`WARN: servers with 0 shadows: [${serversWithoutShadows.join(', ')}]`);
      // Servers with 0 auth entities on peers would legitimately have 0 shadows,
      // so only warn if there are auth entities elsewhere
      const authOnOtherServers = metrics
        .filter(m => !serversWithoutShadows.includes(m.server_id))
        .reduce((sum, m) => sum + m.entity_count, 0);
      if (authOnOtherServers > 0) {
        details.push(`  Other servers have ${authOnOtherServers} auth entities that should be shadowed`);
      }
    }

    // ── Step 4: Verify total shadows approximately match (N-1) * avg_auth_count ──
    ctx.log('Checking shadow count consistency...');
    const totalAuthCount = metrics.reduce((sum, m) => sum + m.entity_count, 0);
    const totalShadowCount = metrics.reduce((sum, m) => sum + m.shadow_count, 0);
    const expectedTotalShadows = totalAuthCount * (ctx.serverCount - 1);

    details.push(`Total auth entities: ${totalAuthCount}`);
    details.push(`Total shadow copies: ${totalShadowCount}`);
    details.push(`Expected shadow copies: ~${expectedTotalShadows} (auth * (N-1))`);

    let shadowRatio = 0;
    if (expectedTotalShadows > 0) {
      shadowRatio = totalShadowCount / expectedTotalShadows;
      details.push(`Shadow ratio: ${(shadowRatio * 100).toFixed(0)}% of expected`);
    }

    // ── Step 5: Verify entity_count + shadow_count is roughly consistent ──
    ctx.log('Checking total-view consistency across servers...');
    const totalViews = metrics.map(m => m.entity_count + m.shadow_count);
    const avgView = totalViews.reduce((a, b) => a + b, 0) / totalViews.length;
    const maxDeviation = Math.max(...totalViews.map(v => Math.abs(v - avgView)));

    details.push(`World view (auth+shadow) per server: [${totalViews.join(', ')}]`);
    details.push(`Average world view: ${avgView.toFixed(0)}, max deviation: ${maxDeviation}`);

    // Each server should see roughly the same total (auth + shadows = full world)
    const viewConsistent = avgView > 0 ? (maxDeviation / avgView < 0.3) : true;
    if (!viewConsistent) {
      details.push('WARN: world view varies significantly across servers (>30% deviation)');
    }

    // ── Final verdict ──
    let status: ScenarioResult['status'] = 'pass';
    const warnings: string[] = [];

    if (!peerCountOk) warnings.push('peer count mismatch');
    if (!allHaveShadows) warnings.push('missing shadows');
    if (shadowRatio < 0.5 && expectedTotalShadows > 0) warnings.push(`low shadow ratio ${(shadowRatio * 100).toFixed(0)}%`);
    if (!viewConsistent) warnings.push('inconsistent world view');

    if (warnings.length > 0) status = 'warn';
    if (!peerCountOk && metrics.some(m => m.peer_count === 0)) status = 'fail';

    const summary = status === 'pass'
      ? `Replication OK: ${totalAuthCount} auth, ${totalShadowCount} shadows across ${metrics.length} servers`
      : `Replication issues: ${warnings.join(', ')}`;

    return {
      name: 'peer-replication',
      status,
      summary,
      details,
      durationMs: 0,
    };
  },
};
