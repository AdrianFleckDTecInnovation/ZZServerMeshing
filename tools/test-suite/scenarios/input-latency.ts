import type { Scenario, ScenarioResult, TestContext, WorldSnapshot } from '../types.js';

export const inputLatency: Scenario = {
  name: 'input-latency',
  description: 'Measure round-trip latency: send MoveInput → observe position change in snapshot',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const SAMPLES = 30;
    const latencies: number[] = [];

    // Spawn a player entity to test with
    ctx.log('Spawning test player...');
    ctx.server.send(JSON.stringify({
      type: 'SpawnEntity',
      position: [0, 0, 0],
      entity_type: 'player',
    }));

    await ctx.sleep(1000);

    // Find our player entity
    const snapshot = ctx.getLatestSnapshot();
    if (!snapshot) {
      return {
        name: 'input-latency',
        status: 'fail',
        summary: 'No snapshot received',
        details: [],
        durationMs: 0,
      };
    }

    const players = snapshot.entities.filter(e => e.entity_type === 'Player');
    if (players.length === 0) {
      return {
        name: 'input-latency',
        status: 'fail',
        summary: 'No player entity found',
        details: [],
        durationMs: 0,
      };
    }

    // Use the last spawned player
    const player = players[players.length - 1];
    details.push(`Test player: ${player.id.substring(0, 8)}... on S${player.authority_server}`);

    // Measure latency by sending MoveInput and waiting for position change
    ctx.log(`Measuring ${SAMPLES} input round-trips...`);

    for (let i = 0; i < SAMPLES; i++) {
      const prevSnapshot = ctx.getLatestSnapshot();
      const prevPlayer = prevSnapshot?.entities.find(e => e.id === player.id);
      const prevPos = prevPlayer?.position ?? [0, 0, 0];

      // Alternate direction each sample
      const dir = i % 2 === 0 ? [1, 0, 0] : [-1, 0, 0];
      const sendTime = Date.now();

      ctx.server.send(JSON.stringify({
        type: 'MoveInput',
        entity_id: player.id,
        direction: dir,
      }));

      // Wait until we see the position change in a snapshot
      let observed = false;
      for (let j = 0; j < 20; j++) { // up to ~1s (20 ticks)
        await ctx.sleep(50);
        const snap = ctx.getLatestSnapshot();
        if (!snap) continue;

        const p = snap.entities.find(e => e.id === player.id);
        if (!p) continue;

        // Check if velocity changed (indicates input was processed)
        const velocityChanged =
          Math.abs(p.velocity[0] - (prevPlayer?.velocity[0] ?? 0)) > 0.01 ||
          Math.abs(p.velocity[2] - (prevPlayer?.velocity[2] ?? 0)) > 0.01;

        const posChanged =
          Math.abs(p.position[0] - prevPos[0]) > 0.01 ||
          Math.abs(p.position[2] - prevPos[2]) > 0.01;

        if (velocityChanged || posChanged) {
          const latency = Date.now() - sendTime;
          latencies.push(latency);
          observed = true;
          break;
        }
      }

      if (!observed) {
        details.push(`Sample ${i + 1}: timeout (no position change observed)`);
      }

      // Brief pause between samples
      await ctx.sleep(100);
    }

    if (latencies.length === 0) {
      return {
        name: 'input-latency',
        status: 'fail',
        summary: 'No successful latency measurements',
        details,
        durationMs: 0,
      };
    }

    // Compute stats
    latencies.sort((a, b) => a - b);
    const p50 = latencies[Math.floor(latencies.length * 0.5)];
    const p95 = latencies[Math.floor(latencies.length * 0.95)];
    const p99 = latencies[Math.floor(latencies.length * 0.99)];
    const avg = latencies.reduce((a, b) => a + b, 0) / latencies.length;
    const min = latencies[0];
    const max = latencies[latencies.length - 1];

    details.push(`Samples: ${latencies.length}/${SAMPLES} successful`);
    details.push(`Latency (ms):`);
    details.push(`  min:  ${min}ms`);
    details.push(`  p50:  ${p50}ms`);
    details.push(`  p95:  ${p95}ms`);
    details.push(`  p99:  ${p99}ms`);
    details.push(`  max:  ${max}ms`);
    details.push(`  avg:  ${avg.toFixed(0)}ms`);

    // Theoretical minimum: 1 tick (50ms) client→server + 1 tick server→client = ~100ms
    const theoreticalMin = 100;
    details.push(`Theoretical min: ~${theoreticalMin}ms (2 tick round-trip at 20Hz)`);

    let status: ScenarioResult['status'] = 'pass';
    if (p95 > 300) {
      status = 'warn';
    }
    if (p95 > 500) {
      status = 'fail';
    }

    return {
      name: 'input-latency',
      status,
      summary: `p50=${p50}ms p95=${p95}ms p99=${p99}ms (${latencies.length}/${SAMPLES} samples)`,
      details,
      durationMs: 0,
    };
  },
};
