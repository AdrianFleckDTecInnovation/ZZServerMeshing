/**
 * WI-193 scenario 2 verification — drain under load.
 *
 * Steps:
 *  1. Connect to S2 directly via WSS.
 *  2. Spawn 100 NPCs (JSON SpawnEntity messages).
 *  3. Wait for entities to be counted on S2's /metrics.
 *  4. SIGTERM S2 via ssh unoq2.
 *  5. Wait 5s, check S1+S3 have the 100 entities (plus originals).
 *  6. Verify S2's sim2.log shows "Graceful drain complete".
 *
 * Run with: npx tsx tools/verify-wi193.ts
 */

import WebSocket from 'ws';
import { execSync } from 'child_process';
import https from 'https';

const S2_URL = 'wss://192.168.0.173:7879';

function sh(cmd: string): string {
  try {
    return execSync(cmd, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  } catch (e: any) {
    return `ERR ${e.status}: ${e.stderr?.toString() ?? ''}`;
  }
}

async function wait(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function getServers(): Promise<{ servers: Array<{ server_id: number; entity_count: number; host: string }> }> {
  return new Promise((resolve, reject) => {
    https
      .get('https://192.168.0.208:7871/servers', { rejectUnauthorized: false }, (res) => {
        let data = '';
        res.on('data', (c) => (data += c));
        res.on('end', () => {
          try {
            resolve(JSON.parse(data));
          } catch (e) {
            reject(e);
          }
        });
      })
      .on('error', reject);
  });
}

async function main() {
  console.log('=== WI-193 Scenario 2: Drain under load ===');

  // Baseline
  const before = await getServers();
  const beforeMap: Record<number, number> = {};
  for (const s of before.servers) beforeMap[s.server_id] = s.entity_count;
  console.log(`Baseline: ${before.servers.map((s) => `S${s.server_id}:${s.entity_count}`).join(' ')}`);
  const beforeTotal = before.servers.reduce((a, b) => a + b.entity_count, 0);

  // Connect to S2
  console.log(`Connecting to ${S2_URL}...`);
  const ws = new WebSocket(S2_URL, { rejectUnauthorized: false });
  await new Promise<void>((resolve, reject) => {
    ws.once('open', resolve);
    ws.once('error', reject);
  });
  console.log('Connected');

  // Absorb PlayerAssigned / initial frames
  ws.on('message', () => {});
  await wait(500);

  // Spawn 100 NPCs at random positions near origin
  const SPAWN_COUNT = 100;
  for (let i = 0; i < SPAWN_COUNT; i++) {
    const x = (Math.random() - 0.5) * 400;
    const z = (Math.random() - 0.5) * 400;
    ws.send(
      JSON.stringify({
        type: 'SpawnEntity',
        position: [x, 0, z],
        entity_type: 'Npc',
      })
    );
    if ((i + 1) % 20 === 0) await wait(100); // tiny throttle
  }
  console.log(`Sent ${SPAWN_COUNT} SpawnEntity messages`);
  await wait(2000);

  // Check S2's count went up
  const mid = await getServers();
  const s2Mid = mid.servers.find((s) => s.server_id === 2)?.entity_count ?? 0;
  console.log(`S2 entity count after spawn: ${s2Mid} (was ${beforeMap[2]})`);
  console.log(`All servers mid: ${mid.servers.map((s) => `S${s.server_id}:${s.entity_count}`).join(' ')}`);

  // Record tick on S2 before kill, for sim2.log grep scoping
  const killAtTime = new Date().toISOString();
  console.log(`--- Killing S2 with SIGTERM at ${killAtTime} ---`);
  sh(`ssh unoq2 'pkill -TERM -f /home/arduino/mesh/sim-server'`);

  // Wait for drain to complete and coordinator to reflect
  await wait(6000);

  const after = await getServers();
  console.log(`After: ${after.servers.map((s) => `S${s.server_id}:${s.entity_count}`).join(' ')}`);
  const afterTotal = after.servers.reduce((a, b) => a + b.entity_count, 0);
  console.log(`Total entities: before=${beforeTotal + SPAWN_COUNT} after=${afterTotal}`);

  // Verify S2 is gone, entities migrated to S1/S3
  const s2After = after.servers.find((s) => s.server_id === 2);
  const s1After = after.servers.find((s) => s.server_id === 1)?.entity_count ?? 0;
  const s3After = after.servers.find((s) => s.server_id === 3)?.entity_count ?? 0;

  console.log('');
  console.log('=== Results ===');
  if (s2After) {
    console.log(`  FAIL: S2 still in /servers (entity_count=${s2After.entity_count})`);
  } else {
    console.log(`  PASS: S2 removed from /servers`);
  }

  const expectedS1S3 = (beforeMap[1] ?? 0) + (beforeMap[3] ?? 0) + s2Mid;
  const actualS1S3 = s1After + s3After;
  console.log(`  S1+S3 now=${actualS1S3} expected≈${expectedS1S3} (no recovery-race doubling)`);

  // Pull log from unoq2 scoped to our kill
  console.log('');
  console.log('--- sim2.log relevant lines ---');
  const log = sh(
    `ssh unoq2 'tail -60 ~/mesh/sim2.log | grep -E "(Received shutdown|drain|Drain|Graceful|Game loop exited)"'`
  );
  console.log(log);

  ws.close();
}

main().catch((e) => {
  console.error('FATAL:', e);
  process.exit(1);
});
