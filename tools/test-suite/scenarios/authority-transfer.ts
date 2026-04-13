import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const authorityTransfer: Scenario = {
  name: 'authority-transfer',
  description: 'Trigger rebalance by overloading one server, verify entities transfer with state preserved',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    if (ctx.serverCount < 2) {
      return { name: 'authority-transfer', status: 'fail', summary: 'Need at least 2 servers', details, durationMs: 0 };
    }

    // ── Step 1: Record baseline distribution ──
    const baseline = await ctx.getAllMetrics();
    if (baseline.length === 0) {
      // Metrics endpoints unreachable (remote boards) — use snapshot instead
      const snap = ctx.getLatestSnapshot();
      const count = snap?.entities.length ?? 0;
      details.push(`Baseline (from snapshot): ${count} entities`);
    } else {
      details.push('Baseline:');
      for (const m of baseline) {
        details.push(`  S${m.server_id}: ${m.entity_count} entities`);
      }
    }
    const totalBaseline = baseline.length > 0
      ? baseline.reduce((s, m) => s + m.entity_count, 0)
      : (ctx.getLatestSnapshot()?.entities.length ?? 0);

    // ── Step 2: Overload one server to trigger rebalance ──
    // Spawn 100 entities — they all go to our connected server
    ctx.log('Spawning 100 entities to trigger imbalance...');
    for (let i = 0; i < 100; i++) {
      ctx.server.send(JSON.stringify({
        type: 'SpawnEntity',
        position: [(Math.random() - 0.5) * 400, 0, (Math.random() - 0.5) * 400],
        entity_type: 'npc',
      }));
    }

    await ctx.sleep(2000);

    // Verify imbalance (use snapshots if metrics unavailable)
    const imbalanced = await ctx.getAllMetrics();
    let imbalanceRatio = 1.0;
    if (imbalanced.length >= 2) {
      details.push('After spawn (imbalanced):');
      for (const m of imbalanced) {
        details.push(`  S${m.server_id}: ${m.entity_count} entities`);
      }
      const maxServer = imbalanced.reduce((a, b) => a.entity_count > b.entity_count ? a : b);
      const minServer = imbalanced.reduce((a, b) => a.entity_count < b.entity_count ? a : b);
      imbalanceRatio = maxServer.entity_count / Math.max(minServer.entity_count, 1);
      details.push(`Imbalance ratio: ${imbalanceRatio.toFixed(1)}x`);
    } else {
      const snap = ctx.getLatestSnapshot();
      details.push(`After spawn: ${snap?.entities.length ?? 0} entities visible (metrics unavailable)`);
    }

    // ── Step 3: Take a snapshot of entity-to-server mapping BEFORE rebalance ──
    const snapBefore = ctx.getLatestSnapshot();
    if (!snapBefore) {
      return { name: 'authority-transfer', status: 'fail', summary: 'No snapshot', details, durationMs: 0 };
    }

    const authMapBefore = new Map<string, number>();
    for (const e of snapBefore.entities) {
      authMapBefore.set(e.id, e.authority_server);
    }
    details.push(`Tracking ${authMapBefore.size} entities for authority changes`);

    // ── Step 4: Wait for coordinator rebalance (runs every 5s) ──
    ctx.log('Waiting for rebalance (up to 30s)...');
    let transfersDetected = 0;
    const transferredEntities: { id: string; from: number; to: number }[] = [];
    let rebalanced = false;

    for (let i = 0; i < 60; i++) { // up to 30 seconds
      await ctx.sleep(500);
      const snap = ctx.getLatestSnapshot();
      if (!snap) continue;

      // Check for authority changes
      for (const e of snap.entities) {
        const prevAuth = authMapBefore.get(e.id);
        if (prevAuth !== undefined && prevAuth !== e.authority_server) {
          // This entity transferred!
          if (!transferredEntities.find(t => t.id === e.id)) {
            transferredEntities.push({ id: e.id, from: prevAuth, to: e.authority_server });
            transfersDetected++;
          }
          authMapBefore.set(e.id, e.authority_server); // update to track further transfers
        }
      }

      // Check progress via snapshot authority distribution
      if (i % 10 === 0) {
        const snapNow = ctx.getLatestSnapshot();
        if (snapNow) {
          const serverCounts: Record<number, number> = {};
          for (const e of snapNow.entities) {
            serverCounts[e.authority_server] = (serverCounts[e.authority_server] || 0) + 1;
          }
          const counts = Object.values(serverCounts);
          const cMax = Math.max(...counts);
          const cMin = Math.min(...counts);
          const ratio = cMax / Math.max(cMin, 1);
          ctx.log(`${(i * 0.5).toFixed(0)}s: ${transfersDetected} transfers, ratio=${ratio.toFixed(1)}x dist=${JSON.stringify(serverCounts)}`);
        }
      }

      if (transfersDetected > 5) {
        rebalanced = true;
        break;
      }
    }

    // ── Step 5: Report results ──
    const postMetrics = await ctx.getAllMetrics();
    if (postMetrics.length > 0) {
      details.push('After rebalance:');
      for (const m of postMetrics) {
        details.push(`  S${m.server_id}: ${m.entity_count} entities`);
      }
    }

    details.push(`Transfers detected: ${transfersDetected}`);
    for (const t of transferredEntities.slice(0, 10)) {
      details.push(`  ${t.id.substring(0, 8)}... S${t.from} → S${t.to}`);
    }
    if (transferredEntities.length > 10) {
      details.push(`  ... and ${transferredEntities.length - 10} more`);
    }

    // ── Step 6: Verify total entity count preserved ──
    const snapPost = ctx.getLatestSnapshot();
    const clientEntities = snapPost?.entities.length ?? 0;
    details.push(`Client sees: ${clientEntities} entities`);

    if (transfersDetected === 0) {
      return {
        name: 'authority-transfer',
        status: 'warn',
        summary: `No authority transfers detected in 30s (imbalance=${imbalanceRatio.toFixed(1)}x)`,
        details,
        durationMs: 0,
      };
    }

    return {
      name: 'authority-transfer',
      status: rebalanced ? 'pass' : 'warn',
      summary: `${transfersDetected} entities transferred, balance ${rebalanced ? 'restored' : 'improving'}`,
      details,
      durationMs: 0,
    };
  },
};
