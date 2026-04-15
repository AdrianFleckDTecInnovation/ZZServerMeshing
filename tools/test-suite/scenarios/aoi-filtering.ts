import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const aoiFiltering: Scenario = {
  name: 'aoi-filtering',
  description: 'Verify AoI filtering: near entities visible, far entities invisible, player always visible',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    // ── Step 1: Wait for initial snapshot and record player ──
    ctx.log('Waiting for initial snapshot...');
    await ctx.sleep(500);
    let snap = ctx.getLatestSnapshot();
    if (!snap) {
      await ctx.sleep(1000);
      snap = ctx.getLatestSnapshot();
    }

    if (!snap) {
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: 'No snapshot received from server',
        details: ['getLatestSnapshot() returned null'],
        durationMs: 0,
      };
    }

    // Find the player entity (type = Player). The test client gets a player on connect.
    const players = snap.entities.filter(e => e.entity_type === 'Player');
    if (players.length === 0) {
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: 'No player entity in snapshot',
        details: ['Expected at least one Player entity after connect'],
        durationMs: 0,
      };
    }

    const player = players[0];
    const playerId = player.id;
    details.push(`Player: ${playerId.substring(0, 8)}... at (${player.position.map(v => v.toFixed(0)).join(',')})`);
    ctx.log(`Player at (${player.position.map(v => v.toFixed(0)).join(',')})`);

    // Record baseline entity count (before our spawns)
    const baselineCount = snap.entities.length;
    details.push(`Baseline entities: ${baselineCount}`);

    // ── Step 2: Spawn 10 entities near origin (within AoI) ──
    ctx.log('Spawning 10 NPCs near origin (within AoI)...');
    await ctx.spawnCluster(10, [0, 0, 0], 100);
    await ctx.sleep(2000);

    snap = ctx.getLatestSnapshot();
    if (!snap) {
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: 'No snapshot after near spawn',
        details,
        durationMs: 0,
      };
    }

    const afterNearSpawn = snap.entities.length;
    details.push(`After near spawn: ${afterNearSpawn} entities visible`);
    ctx.log(`After near spawn: ${afterNearSpawn} entities`);

    // Should have gained ~10 entities (some may still be propagating)
    const nearGain = afterNearSpawn - baselineCount;
    if (nearGain < 5) {
      details.push(`WARN: only gained ${nearGain} entities from 10 near spawns`);
    }

    // ── Step 3: Spawn 10 entities far away (outside AoI) ──
    ctx.log('Spawning 10 NPCs far from origin (outside AoI at ~3000,0,3000)...');
    await ctx.spawnCluster(10, [3000, 0, 3000], 100);
    await ctx.sleep(2000);

    snap = ctx.getLatestSnapshot();
    if (!snap) {
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: 'No snapshot after far spawn',
        details,
        durationMs: 0,
      };
    }

    const afterFarSpawn = snap.entities.length;
    details.push(`After far spawn: ${afterFarSpawn} entities visible`);
    ctx.log(`After far spawn: ${afterFarSpawn} entities`);

    // The far entities should NOT be visible (outside AoI).
    // Entity count should be roughly the same as after near spawn.
    const farGain = afterFarSpawn - afterNearSpawn;
    details.push(`Far spawn gain: ${farGain} (should be ~0)`);

    // ── Step 4: Verify the player entity is always present ──
    const playerPresent = snap.entities.some(e => e.id === playerId);
    details.push(`Player visible: ${playerPresent}`);

    // ── Step 5: Check for far entity IDs in snapshot ──
    // We can verify through metrics that the far entities exist on the servers
    // but are filtered from the client snapshot.
    const metrics = await ctx.getAllMetrics();
    const totalAuth = metrics.reduce((sum, m) => sum + m.entity_count, 0);
    details.push(`Server total auth entities: ${totalAuth}`);
    details.push(`Client visible entities: ${afterFarSpawn}`);

    // The server should have more entities than the client sees
    const filterActive = totalAuth > afterFarSpawn;
    details.push(`AoI filter active: ${filterActive} (server has ${totalAuth - afterFarSpawn} more than client sees)`);

    for (const m of metrics) {
      details.push(`  S${m.server_id}: ${m.entity_count} auth, ${m.shadow_count} shadows`);
    }

    // ── Verdict ──
    let status: ScenarioResult['status'] = 'pass';
    const warnings: string[] = [];

    if (!playerPresent) {
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: 'Player entity disappeared from snapshot',
        details,
        durationMs: 0,
      };
    }

    if (farGain > 3) {
      // If more than 3 far entities leaked through, AoI is not working
      return {
        name: 'aoi-filtering',
        status: 'fail',
        summary: `AoI not filtering: ${farGain} far entities appeared in snapshot (expected ~0)`,
        details,
        durationMs: 0,
      };
    }

    if (nearGain < 5) {
      warnings.push(`only ${nearGain}/10 near entities visible`);
    }

    if (!filterActive) {
      warnings.push('server entity count not higher than client count');
    }

    if (warnings.length > 0) status = 'warn';

    const summary = status === 'pass'
      ? `AoI OK: ${afterFarSpawn} visible, ${totalAuth} on servers, player always visible, far entities filtered`
      : `AoI issues: ${warnings.join(', ')}`;

    return {
      name: 'aoi-filtering',
      status,
      summary,
      details,
      durationMs: 0,
    };
  },
};
