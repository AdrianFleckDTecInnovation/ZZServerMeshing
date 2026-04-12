import type { Scenario, ScenarioResult, TestContext } from '../types.js';

export const inputSeqEcho: Scenario = {
  name: 'input-seq-echo',
  description: 'Verify client prediction protocol: send MoveInput with seq numbers, confirm last_input_seq echoed back',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    // ── Step 1: Spawn a player entity ──
    ctx.log('Spawning test player...');
    ctx.server.send(JSON.stringify({
      type: 'SpawnEntity',
      position: [50, 0, 50],
      entity_type: 'player',
    }));

    await ctx.sleep(1000);

    // Find player entities in the snapshot
    const snap = ctx.getLatestSnapshot();
    if (!snap) {
      return {
        name: 'input-seq-echo',
        status: 'fail',
        summary: 'No snapshot received',
        details: ['getLatestSnapshot() returned null'],
        durationMs: 0,
      };
    }

    const players = snap.entities.filter(e => e.entity_type === 'Player');
    if (players.length === 0) {
      return {
        name: 'input-seq-echo',
        status: 'fail',
        summary: 'No Player entities found after spawn',
        details: [`Snapshot has ${snap.entities.length} entities but none are Player type`],
        durationMs: 0,
      };
    }

    // Use the last spawned player (most likely ours)
    const player = players[players.length - 1];
    details.push(`Test player: ${player.id.substring(0, 8)}... on S${player.authority_server}`);
    details.push(`Initial last_input_seq: ${player.last_input_seq}`);
    ctx.log(`Player: ${player.id.substring(0, 8)}... seq=${player.last_input_seq}`);

    // Record initial velocity for later comparison
    const initialVelocity = [...player.velocity];

    // ── Step 2: Send seq=1 and measure roundtrip time ──
    ctx.log('Sending MoveInput with seq=1...');
    const seq1SendTime = Date.now();

    ctx.server.send(JSON.stringify({
      type: 'MoveInput',
      entity_id: player.id,
      direction: [1, 0, 0],
      seq: 1,
    }));

    // Poll until last_input_seq >= 1
    let seq1AckTime: number | null = null;
    for (let i = 0; i < 40; i++) { // up to 2 seconds
      await ctx.sleep(50);
      const s = ctx.getLatestSnapshot();
      if (!s) continue;

      const p = s.entities.find(e => e.id === player.id);
      if (!p) continue;

      ctx.log(`Poll ${i}: last_input_seq=${p.last_input_seq} vel=[${p.velocity.map(v => v.toFixed(2)).join(',')}]`);

      if (p.last_input_seq >= 1) {
        seq1AckTime = Date.now();
        details.push(`seq=1 acknowledged in ${seq1AckTime - seq1SendTime}ms`);
        ctx.log(`seq=1 acked in ${seq1AckTime - seq1SendTime}ms`);
        break;
      }
    }

    if (seq1AckTime === null) {
      details.push('seq=1 was NOT acknowledged within 2s');
      return {
        name: 'input-seq-echo',
        status: 'fail',
        summary: 'seq=1 not echoed within 2 seconds',
        details,
        durationMs: 0,
      };
    }

    const seq1Roundtrip = seq1AckTime - seq1SendTime;

    // ── Step 3: Send seq=2,3,4,5 rapidly ──
    ctx.log('Sending seq=2,3,4,5 in rapid burst...');
    const burstSendTime = Date.now();

    for (let seq = 2; seq <= 5; seq++) {
      // Alternate directions so movement is visible
      const dir = seq % 2 === 0 ? [0, 0, 1] : [0, 0, -1];
      ctx.server.send(JSON.stringify({
        type: 'MoveInput',
        entity_id: player.id,
        direction: dir,
        seq,
      }));
    }

    // Poll until last_input_seq >= 5
    let allAcked = false;
    let finalSeq = 0;
    for (let i = 0; i < 40; i++) { // up to 2 seconds
      await ctx.sleep(50);
      const s = ctx.getLatestSnapshot();
      if (!s) continue;

      const p = s.entities.find(e => e.id === player.id);
      if (!p) continue;

      finalSeq = p.last_input_seq;
      ctx.log(`Burst poll ${i}: last_input_seq=${finalSeq}`);

      if (finalSeq >= 5) {
        allAcked = true;
        const burstTime = Date.now() - burstSendTime;
        details.push(`seq=2..5 all acknowledged in ${burstTime}ms`);
        ctx.log(`Burst acked in ${burstTime}ms`);
        break;
      }
    }

    if (!allAcked) {
      details.push(`Burst NOT fully acknowledged: last_input_seq=${finalSeq} (expected >= 5)`);
      return {
        name: 'input-seq-echo',
        status: 'fail',
        summary: `Seq burst incomplete: last_input_seq=${finalSeq}, expected >= 5`,
        details,
        durationMs: 0,
      };
    }

    // ── Step 4: Verify velocity changed (input was actually processed) ──
    const finalSnap = ctx.getLatestSnapshot();
    const finalPlayer = finalSnap?.entities.find(e => e.id === player.id);

    let velocityChanged = false;
    if (finalPlayer) {
      const dv =
        Math.abs(finalPlayer.velocity[0] - initialVelocity[0]) +
        Math.abs(finalPlayer.velocity[1] - initialVelocity[1]) +
        Math.abs(finalPlayer.velocity[2] - initialVelocity[2]);
      velocityChanged = dv > 0.01;
      details.push(`Velocity change: initial=[${initialVelocity.map(v => v.toFixed(2)).join(',')}] final=[${finalPlayer.velocity.map(v => v.toFixed(2)).join(',')}] changed=${velocityChanged}`);
    } else {
      details.push('WARN: could not find player in final snapshot for velocity check');
    }

    if (!velocityChanged) {
      details.push('WARN: velocity did not change — input may not have been fully processed');
    }

    // ── Summary ──
    details.push(`seq=1 roundtrip: ${seq1Roundtrip}ms`);
    details.push(`All 5 seqs echoed: yes`);
    details.push(`Velocity confirmed changed: ${velocityChanged}`);

    let status: ScenarioResult['status'] = 'pass';
    if (!velocityChanged) status = 'warn';
    if (seq1Roundtrip > 500) status = 'warn';

    return {
      name: 'input-seq-echo',
      status,
      summary: `All 5 seqs echoed, roundtrip=${seq1Roundtrip}ms, velocity_changed=${velocityChanged}`,
      details,
      durationMs: 0,
    };
  },
};
