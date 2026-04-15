import type { Scenario, ScenarioResult, TestContext, EntityState } from '../types.js';

export const forwardedInput: Scenario = {
  name: 'forwarded-input',
  description: 'Verify MoveInput for shadow entities is forwarded to the authority peer and processed',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];

    if (ctx.serverCount < 2) {
      return {
        name: 'forwarded-input',
        status: 'fail',
        summary: 'Need at least 2 servers for input forwarding test',
        details: ['serverCount < 2'],
        durationMs: 0,
      };
    }

    // ── Step 1: Get current snapshot and identify our connected server ──
    ctx.log('Getting initial snapshot...');
    let snap = ctx.getLatestSnapshot();
    if (!snap) {
      await ctx.sleep(500);
      snap = ctx.getLatestSnapshot();
    }

    if (!snap) {
      return {
        name: 'forwarded-input',
        status: 'fail',
        summary: 'No snapshot received from server',
        details: ['getLatestSnapshot() returned null'],
        durationMs: 0,
      };
    }

    ctx.log(`Snapshot: tick=${snap.tick} entities=${snap.entities.length}`);

    // Figure out which server we're connected to by looking at the server URL port
    // ws://localhost:7878 = server 1, 7879 = server 2, etc.
    const urlMatch = ctx.serverUrl.match(/:(\d+)/);
    const wsPort = urlMatch ? parseInt(urlMatch[1]) : 7878;
    const connectedServer = wsPort - 7877; // 7878 -> 1, 7879 -> 2, etc.
    details.push(`Connected to S${connectedServer} (port ${wsPort})`);
    ctx.log(`Connected to S${connectedServer}`);

    // ── Step 2: Find a shadow entity (authority != our server) ──
    // We need entities from other servers visible in our snapshot
    let shadowEntities = snap.entities.filter(e => e.authority_server !== connectedServer);
    ctx.log(`Shadow entities visible: ${shadowEntities.length}`);

    if (shadowEntities.length === 0) {
      // Spawn some entities and wait for them to be distributed and replicated
      ctx.log('No shadow entities visible — spawning NPCs near origin and waiting for distribution...');
      await ctx.spawnCluster(30, [0, 0, 0], 200);
      await ctx.sleep(4000); // let coordinator rebalance + replication propagate

      snap = ctx.getLatestSnapshot();
      if (snap) {
        shadowEntities = snap.entities.filter(e => e.authority_server !== connectedServer);
        ctx.log(`After spawn+wait: ${shadowEntities.length} shadow entities visible`);
      }
    }

    if (shadowEntities.length === 0) {
      // Still no shadows — try spawning more and waiting longer
      ctx.log('Still no shadows. Spawning more near origin and waiting longer...');
      await ctx.spawnCluster(20, [0, 0, 0], 200);
      await ctx.sleep(5000);

      snap = ctx.getLatestSnapshot();
      if (snap) {
        shadowEntities = snap.entities.filter(e => e.authority_server !== connectedServer);
      }

      if (shadowEntities.length === 0) {
        details.push('No shadow entities visible even after spawning and waiting');
        details.push('This means all entities are on our connected server, or replication is broken');

        // Log what we see for debugging
        if (snap) {
          const serverCounts: Record<number, number> = {};
          for (const e of snap.entities) {
            serverCounts[e.authority_server] = (serverCounts[e.authority_server] || 0) + 1;
          }
          details.push(`Authority distribution: ${JSON.stringify(serverCounts)}`);
        }

        return {
          name: 'forwarded-input',
          status: 'fail',
          summary: 'No shadow entities visible — cannot test input forwarding',
          details,
          durationMs: 0,
        };
      }
    }

    details.push(`Shadow entities available: ${shadowEntities.length}`);

    // Pick a shadow entity to target — prefer NPCs (they won't have competing player input)
    const npcs = shadowEntities.filter(e => e.entity_type === 'Npc');
    const target = npcs.length > 0 ? npcs[0] : shadowEntities[0];

    details.push(`Target entity: ${target.id.substring(0, 8)}... type=${target.entity_type} authority=S${target.authority_server}`);
    ctx.log(`Targeting ${target.id.substring(0, 8)}... on S${target.authority_server} (we're on S${connectedServer})`);

    // Record initial state
    const initialVelocity = [...target.velocity];
    const initialPosition = [...target.position];
    details.push(`Initial position: [${initialPosition.map(v => v.toFixed(2)).join(',')}]`);
    details.push(`Initial velocity: [${initialVelocity.map(v => v.toFixed(2)).join(',')}]`);

    // ── Step 3: Send MoveInput targeting the shadow entity ──
    // Our server should forward this to the authority peer
    ctx.log('Sending MoveInput for shadow entity (should be forwarded to authority)...');
    const sendTime = Date.now();

    ctx.server.send(JSON.stringify({
      type: 'MoveInput',
      entity_id: target.id,
      direction: [1, 0, 1], // diagonal movement
    }));

    // ── Step 4: Poll for velocity/position change ──
    ctx.log('Polling for velocity change...');
    let forwarded = false;
    let pollCount = 0;

    for (let i = 0; i < 40; i++) { // up to 2 seconds
      await ctx.sleep(50);
      pollCount++;

      const s = ctx.getLatestSnapshot();
      if (!s) continue;

      const e = s.entities.find(ent => ent.id === target.id);
      if (!e) {
        ctx.log(`Poll ${i}: entity not found in snapshot (may have been transferred)`);
        continue;
      }

      const dVelX = Math.abs(e.velocity[0] - initialVelocity[0]);
      const dVelZ = Math.abs(e.velocity[2] - initialVelocity[2]);
      const dPosX = Math.abs(e.position[0] - initialPosition[0]);
      const dPosZ = Math.abs(e.position[2] - initialPosition[2]);

      const velocityChanged = dVelX > 0.01 || dVelZ > 0.01;
      const positionChanged = dPosX > 0.5 || dPosZ > 0.5;

      if (i % 5 === 0) {
        ctx.log(`Poll ${i}: vel=[${e.velocity.map(v => v.toFixed(2)).join(',')}] pos=[${e.position.map(v => v.toFixed(2)).join(',')}] dVel=${(dVelX + dVelZ).toFixed(3)} dPos=${(dPosX + dPosZ).toFixed(3)}`);
      }

      if (velocityChanged || positionChanged) {
        const latency = Date.now() - sendTime;
        forwarded = true;
        details.push(`Input forwarded and processed in ${latency}ms (${pollCount} polls)`);
        details.push(`Final velocity: [${e.velocity.map(v => v.toFixed(2)).join(',')}]`);
        details.push(`Final position: [${e.position.map(v => v.toFixed(2)).join(',')}]`);
        ctx.log(`Forwarding confirmed in ${latency}ms`);
        break;
      }
    }

    if (!forwarded) {
      // Check if the entity was at least still present
      const finalSnap = ctx.getLatestSnapshot();
      const finalEntity = finalSnap?.entities.find(e => e.id === target.id);

      if (!finalEntity) {
        details.push('Target entity disappeared from snapshots during polling');
        details.push('Entity may have been removed or authority transferred during test');
      } else {
        details.push(`Entity still present but velocity unchanged after 2s`);
        details.push(`Final velocity: [${finalEntity.velocity.map(v => v.toFixed(2)).join(',')}]`);
        details.push(`This may indicate input forwarding is not implemented or broken`);
      }

      // Check metrics to see if peers are connected
      try {
        const m = await ctx.getMetrics(connectedServer);
        details.push(`S${connectedServer} metrics: peers=${m.peer_count} auth=${m.entity_count} shadows=${m.shadow_count}`);
      } catch {
        details.push(`Could not fetch metrics for S${connectedServer}`);
      }

      return {
        name: 'forwarded-input',
        status: 'fail',
        summary: `Input forwarding failed: no velocity change on S${target.authority_server} entity within 2s`,
        details,
        durationMs: 0,
      };
    }

    // ── Step 5: Send a second input to confirm it's not a one-off ──
    ctx.log('Sending second MoveInput to confirm consistent forwarding...');
    const snap2Before = ctx.getLatestSnapshot();
    const entity2Before = snap2Before?.entities.find(e => e.id === target.id);
    const vel2Before = entity2Before ? [...entity2Before.velocity] : [0, 0, 0];

    ctx.server.send(JSON.stringify({
      type: 'MoveInput',
      entity_id: target.id,
      direction: [-1, 0, 0], // opposite direction
    }));

    let secondConfirmed = false;
    for (let i = 0; i < 20; i++) {
      await ctx.sleep(50);
      const s = ctx.getLatestSnapshot();
      if (!s) continue;

      const e = s.entities.find(ent => ent.id === target.id);
      if (!e) continue;

      const dVelX = Math.abs(e.velocity[0] - vel2Before[0]);
      if (dVelX > 0.01) {
        secondConfirmed = true;
        details.push(`Second input confirmed: velocity changed again`);
        break;
      }
    }

    if (!secondConfirmed) {
      details.push('WARN: second input did not produce a visible velocity change');
    }

    let status: ScenarioResult['status'] = 'pass';
    if (!secondConfirmed) status = 'warn';

    return {
      name: 'forwarded-input',
      status,
      summary: `Input forwarding S${connectedServer}->S${target.authority_server} confirmed, second input ${secondConfirmed ? 'also confirmed' : 'not confirmed'}`,
      details,
      durationMs: 0,
    };
  },
};
