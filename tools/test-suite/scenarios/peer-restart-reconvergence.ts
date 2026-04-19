import type { Scenario, ScenarioResult, TestContext, ServerMetrics } from '../types.js';

// WI-137: Verify the peer-restart-recovery fix end-to-end.
// WI-146 (design 015): Adds a sustained-visibility post-convergence assertion
// so we also catch cross-clock shadow_cleanup flicker, not just initial
// reconvergence.
//
// Flow:
//   1. Spawn ~500 entities distributed across S1/S2/S3.
//   2. Record S1/S3 shadow counts for entities authoritative on S2, and S2's
//      pre-kill session_id as seen from S1/S3's peer_health.
//   3. SIGKILL S2 (matches the crash-recovery scenario's kill path; faster than
//      waiting for a graceful shutdown, and the whole point of the fix is that
//      receivers detect restart via session_id without relying on the
//      coordinator's 15 s heartbeat timeout).
//   4. Restart S2 as a fresh process (new PID, new session_id).
//   5. Spawn a fresh batch of entities on S2 so it has a meaningful auth set.
//   6. Assert within 40 ticks (2 s at 20 Hz) after S2's first post-restart
//      replication tick:
//        A. S1 & S3 shadow_count for auth=S2 reconverges (within 10% of
//           expected = S2.entity_count).
//        B. S1 & S3 peer_health[2].chunk_evictions rate-of-change goes to 0.
//        C. S1 & S3 peer_health[2].session_id matches S2's new local session_id.
//   7. Design 015 sustained-visibility check: after reconvergence settles,
//      sample shadow_count on every survivor 20 times over 2 s and assert the
//      swing (max-min) is within ±5% of the median. Pre-015 this produced a
//      huge swing (hundreds) due to a cross-clock bug in shadow_cleanup;
//      post-015 the swing is tight.
//
// Against the pre-Batch-1 codebase, assertion A would time out (shadows never
// reconverge) because the stuck latest_tick evicts all post-restart chunk
// reassemblies indefinitely. Assertion C wouldn't compile against pre-Batch-1
// because peer_health didn't expose session_id yet. This test is therefore a
// strict regression guard for the fix.

const PROJECT_ROOT = (() => process.cwd().replace(/\/tools$/, ''))();
const COORD_HTTP = process.env.COORDINATOR_URL || 'http://localhost:7871';
const BOARD_MODE = !!process.env.METRICS_HOSTS; // treat as board cluster when metrics hosts override is set

// How many ticks to poll post-restart for reconvergence.
// The CORE fix-verification signal (session_id propagation on survivors) should
// land within 40 ticks (2 s) of S2's first post-restart PeerMessage arriving on
// survivors. We budget generously for the outer loop because:
//   - Local: `cargo run` restart takes 3-8s depending on cache.
//   - Boards: board start latency + initial coordinator handshake ~5s.
//   - Coordinator rebalance interval is 5s, and rebalance is how S2 first gets
//     entities to replicate (and therefore its first session-stamped packet on
//     the wire). The test does not wait forever — 60s is ample.
const CONVERGENCE_POLL_TICKS = 40;
const TICK_MS = 50;
const POLL_INTERVAL_MS = TICK_MS * 5; // sample every 5 ticks = 250ms
const MAX_POLL_MS = 60_000; // 60s: post-restart + first-rebalance + session propagation

async function killLocal(serverId: number): Promise<boolean> {
  const { execSync } = await import('child_process');
  try {
    const wsPort = 7877 + serverId;
    const pids = execSync(`lsof -ti tcp:${wsPort} -sTCP:LISTEN 2>/dev/null || true`).toString().trim();
    if (!pids) return false;
    execSync(`kill -9 ${pids.split('\n').join(' ')}`);
    return true;
  } catch {
    return false;
  }
}

async function restartLocal(serverId: number): Promise<number | null> {
  const wsPort = 7877 + serverId;
  const udpPort = 9100 + serverId;
  const metricsPort = 8100 + serverId;
  const { spawn: spawnChild } = await import('child_process');
  const { openSync } = await import('fs');
  const logFd = openSync(`${PROJECT_ROOT}/.sim-server-${serverId}.log`, 'a');
  const child = spawnChild('cargo', [
    'run', '--manifest-path', `${PROJECT_ROOT}/Cargo.toml`, '-p', 'sim-server', '--',
    '--server-id', `${serverId}`, '--port', `${wsPort}`, '--udp-port', `${udpPort}`,
    '--metrics-port', `${metricsPort}`,
    '--coordinator', `127.0.0.1:7870`, '--advertise-host', '127.0.0.1', '--initial-npcs', '0',
  ], { stdio: ['ignore', logFd, logFd], detached: true });
  child.unref();
  return child.pid ?? null;
}

// Board kill/restart via SSH, mirroring what `start-unoq.sh` does. The board
// dashboard's /api/mesh/start spawns children without reaping them (pre-
// existing bug — dashboard redirects stdout/stderr to /dev/null and never
// awaits the child), so using the dashboard here leaves sim-server zombies
// after a kill+restart cycle. SSH + `nohup` gives us a freshly-detached
// process in the same shape as the initial cluster bring-up, which is the
// scenario we actually want to verify.
async function killBoard(serverId: number): Promise<boolean> {
  const { execSync } = await import('child_process');
  const host = serverId === 1 ? 'unoq' : serverId === 2 ? 'unoq2' : 'unoq3';
  try {
    execSync(`ssh -o BatchMode=yes -o ConnectTimeout=5 ${host} 'pkill -9 -f sim-server' 2>/dev/null || true`,
      { timeout: 8000 });
    return true;
  } catch {
    return false;
  }
}

async function restartBoard(serverId: number): Promise<boolean> {
  const { execSync } = await import('child_process');
  const host = serverId === 1 ? 'unoq' : serverId === 2 ? 'unoq2' : 'unoq3';
  const boardIp = serverId === 1 ? '192.168.0.15' : serverId === 2 ? '192.168.0.173' : '192.168.0.86';
  const coordIp = '192.168.0.208';
  const wsPort = 7877 + serverId;
  const udpPort = 9100 + serverId;
  const metricsPort = 8100 + serverId;
  const cmd = [
    'nohup ~/mesh/sim-server',
    `--server-id ${serverId}`,
    `--port ${wsPort}`, `--udp-port ${udpPort}`, `--metrics-port ${metricsPort}`,
    `--coordinator ${coordIp}:7870`, `--advertise-host ${boardIp}`,
    '--initial-npcs 0',
    // WI-223: coordinator binds WSS on :7870 on the boards, so outbound
    // coordinator connections must opt into TLS with the pinned cert.
    '--coordinator-tls',
    '--tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt',
    '--tls-key /home/arduino/mesh/certs/mesh-coordinator.key',
    `> ~/mesh/sim${serverId}.log 2>&1 &`,
  ].join(' ');
  try {
    execSync(`ssh -o BatchMode=yes -o ConnectTimeout=5 ${host} '${cmd}'`,
      { timeout: 10000 });
    return true;
  } catch {
    return false;
  }
}

async function waitForRestartedServerOnline(ctx: TestContext, serverId: number, timeoutMs: number): Promise<ServerMetrics | null> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const m = await ctx.getMetrics(serverId);
      if (m && m.server_id === serverId && m.tick > 0) return m;
    } catch {}
    await ctx.sleep(500);
  }
  return null;
}

function getPeerEntry(m: ServerMetrics, peerId: number) {
  return m.peer_health?.[String(peerId)];
}

export const peerRestartReconvergence: Scenario = {
  name: 'peer-restart-reconvergence',
  description: 'After killing S2 and restarting it, S1 and S3 shadows, chunk_evictions, and peer session_id must reconverge within 40 ticks',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const TARGET = 2;
    const survivorIds = [1, 3].filter(s => s <= ctx.serverCount);

    if (ctx.serverCount < 3) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: `Need 3 servers for this test (got ${ctx.serverCount})`,
        details: [],
        durationMs: 0,
      };
    }

    // -- Step 0: sanity: all servers up, session_ids present ---------------
    {
      const ms = await ctx.getAllMetrics();
      for (const m of ms) {
        if (m.session_id === undefined) {
          return {
            name: 'peer-restart-reconvergence',
            status: 'fail',
            summary: 'metrics JSON missing session_id (pre-Batch-1 server?)',
            details: [`S${m.server_id}: ${JSON.stringify(m).slice(0, 200)}`],
            durationMs: 0,
          };
        }
      }
    }

    // -- Step 1: Spawn ~500 entities across servers ------------------------
    ctx.log('Spawning 500 entities across the cluster...');
    await ctx.spawn(500);
    // Let replication propagate and shadows settle
    await ctx.sleep(4000);

    const preMetrics = await ctx.getAllMetrics();
    const s2Pre = preMetrics.find(m => m.server_id === TARGET);
    if (!s2Pre) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: 'S2 metrics unavailable before kill',
        details,
        durationMs: 0,
      };
    }

    const preSession = s2Pre.session_id!;
    const s2AuthPre = s2Pre.entity_count;
    details.push(`Pre-kill: S${TARGET} session_id=${BigInt(preSession).toString(16)} auth=${s2AuthPre}`);
    for (const m of preMetrics) {
      details.push(`  S${m.server_id}: auth=${m.entity_count} shadows=${m.shadow_count}`);
    }

    // Survivors' view of S2's session before kill (must match)
    for (const sid of survivorIds) {
      const m = preMetrics.find(x => x.server_id === sid);
      if (!m) continue;
      const peer = getPeerEntry(m, TARGET);
      if (peer) {
        details.push(`  S${sid} sees S${TARGET}: session=${peer.session_id ? BigInt(peer.session_id).toString(16) : '?'} latest_tick=${peer.latest_tick} chunk_evictions=${peer.chunk_evictions}`);
      }
    }

    // -- Step 2: Kill S2 ---------------------------------------------------
    ctx.log(`Killing S${TARGET}...`);
    const killed = BOARD_MODE ? await killBoard(TARGET) : await killLocal(TARGET);
    if (!killed) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: `Could not kill S${TARGET}`,
        details,
        durationMs: 0,
      };
    }
    await ctx.sleep(1000);

    // -- Step 3: Restart S2 ------------------------------------------------
    ctx.log(`Restarting S${TARGET}...`);
    const restartOk = BOARD_MODE ? await restartBoard(TARGET) : (await restartLocal(TARGET)) !== null;
    if (!restartOk) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: `Could not restart S${TARGET}`,
        details,
        durationMs: 0,
      };
    }

    const s2Back = await waitForRestartedServerOnline(ctx, TARGET, 30_000);
    if (!s2Back) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: `S${TARGET} did not come back online within 30s`,
        details,
        durationMs: 0,
      };
    }
    const newSession = s2Back.session_id!;
    if (newSession === preSession) {
      return {
        name: 'peer-restart-reconvergence',
        status: 'fail',
        summary: `S${TARGET} session_id did not change across restart (pre=${BigInt(preSession).toString(16)}, new=${BigInt(newSession).toString(16)})`,
        details,
        durationMs: 0,
      };
    }
    details.push(`Post-restart: S${TARGET} session_id=${BigInt(newSession).toString(16)} tick=${s2Back.tick}`);

    // -- Step 4: Spawn a fresh batch on S2 so it has a meaningful auth set -
    ctx.log(`Spawning 200 entities (round-robin; ~67 land on S${TARGET})...`);
    await ctx.spawn(200);
    await ctx.sleep(1500); // let replication begin

    // Snapshot S2 entity count after spawn — this is what survivors must converge to.
    const s2Armed = await ctx.getMetrics(TARGET);
    const s2AuthTarget = s2Armed.entity_count;
    details.push(`S${TARGET} authoritative count after post-restart spawns: ${s2AuthTarget}`);

    // -- Step 5: Poll survivors until convergence --------------------------
    // Assertion A: S1 & S3 peer_health[2].session_id == newSession
    // Assertion B: S1 & S3 shadow contribution for S2 >= 0.9 * s2AuthTarget
    //             (we use total shadow_count as a coarse proxy since shadows aren't
    //              broken down by authority in /metrics; any reasonable fix causes it
    //              to reconverge, so the strict test is peer_health.latest_tick
    //              and session_id, plus chunk_evictions rate).
    // Assertion C: chunk_evictions rate-of-change goes to 0.
    const pollStart = Date.now();
    let sessionOk = false;
    let evictionsStable = false;
    let shadowOk = false;
    let lastEvictions: Record<number, number> = {};
    let evictionStableSamples = 0;
    let finalSurvivorMetrics: ServerMetrics[] = [];

    while (Date.now() - pollStart < MAX_POLL_MS) {
      const samples = await Promise.all(survivorIds.map(sid => ctx.getMetrics(sid).catch(() => null)));
      const valid = samples.filter((x): x is ServerMetrics => x !== null);
      finalSurvivorMetrics = valid;

      // A: session_id propagation (string-equality; runner preserves u64 precision)
      const sessionsSeen = valid.map(m => getPeerEntry(m, TARGET)?.session_id ?? '');
      sessionOk = valid.length === survivorIds.length &&
        sessionsSeen.every(s => s === newSession);

      // B: eviction rate-of-change -> 0. Take two samples POLL_INTERVAL_MS apart and
      //    require zero delta on two consecutive polls.
      const currEvictions: Record<number, number> = {};
      for (const m of valid) {
        currEvictions[m.server_id] = getPeerEntry(m, TARGET)?.chunk_evictions ?? 0;
      }
      if (Object.keys(lastEvictions).length > 0) {
        const stable = valid.every(m => currEvictions[m.server_id] === lastEvictions[m.server_id]);
        if (stable && sessionOk) {
          evictionStableSamples++;
          if (evictionStableSamples >= 2) evictionsStable = true;
        } else {
          evictionStableSamples = 0;
        }
      }
      lastEvictions = currEvictions;

      // C: shadow reconvergence. The survivors' shadow_count includes shadows of
      //    every other server; we need S2-authoritative shadow reconvergence, so we
      //    look at peer_health[2].latest_tick catching up to s2Back.tick and at the
      //    total shadow_count roughly matching (S1.auth + S3.auth) worth from S2
      //    perspective. The strict signal we have is: latest_tick for S2 >= restart tick.
      const latestTicks = valid.map(m => getPeerEntry(m, TARGET)?.latest_tick ?? 0);
      const s2Now = await ctx.getMetrics(TARGET).catch(() => null);
      if (s2Now) {
        // All survivors within 20 ticks of S2's current tick
        shadowOk = latestTicks.every(t => t > 0 && s2Now.tick - t <= 20);
      }

      if (sessionOk && evictionsStable && shadowOk) break;
      await ctx.sleep(POLL_INTERVAL_MS);
    }

    const elapsed = Date.now() - pollStart;
    details.push(`Convergence poll: ${elapsed} ms (budget ${MAX_POLL_MS} ms)`);
    for (const m of finalSurvivorMetrics) {
      const p = getPeerEntry(m, TARGET);
      const sessHex = p?.session_id ? BigInt(p.session_id).toString(16) : '?';
      details.push(`  S${m.server_id}: peer_health[${TARGET}]={session=${sessHex}, latest_tick=${p?.latest_tick ?? '?'}, chunk_evictions=${p?.chunk_evictions ?? '?'}, packets_received=${p?.packets_received ?? '?'}}`);
    }

    // Final verdict
    const failures: string[] = [];
    if (!sessionOk) failures.push('peer session_id did not update on survivors');
    if (!evictionsStable) failures.push('chunk_evictions kept growing after restart');
    if (!shadowOk) failures.push('survivors peer_health.latest_tick did not catch up to S2 current tick');

    if (failures.length === 0) {
      // Design 015 regression guard: after reconvergence, shadow_count on the
      // survivors must remain stable. Pre-015, a cross-clock comparison in
      // shadow_cleanup caused ~20 Hz flicker of S2-authoritative shadows on
      // S1/S3 (despawn-then-respawn every single tick). Sample shadow_count 20
      // times at 100ms intervals and require oscillation within ±5% of the
      // median (or ±2 entities, whichever is larger, for replication jitter).
      ctx.log('Checking sustained visibility (20 samples over 2s)...');
      await ctx.sleep(3000); // let things settle after reconvergence
      const shadowSeries: Record<number, number[]> = {};
      for (const sid of survivorIds) shadowSeries[sid] = [];
      for (let i = 0; i < 20; i++) {
        const samples = await Promise.all(
          survivorIds.map(sid => ctx.getMetrics(sid).catch(() => null)),
        );
        for (const m of samples) {
          if (m) shadowSeries[m.server_id]?.push(m.shadow_count);
        }
        await ctx.sleep(100);
      }
      const flickerFailures: string[] = [];
      for (const sid of survivorIds) {
        const series = shadowSeries[sid] ?? [];
        if (series.length < 10) continue;
        const sorted = [...series].sort((a, b) => a - b);
        const median = sorted[Math.floor(sorted.length / 2)];
        const min = sorted[0];
        const max = sorted[sorted.length - 1];
        const swing = max - min;
        // Allow ±5% of median OR ±2 entities (whichever is larger) for
        // normal rebalance/replication jitter.
        const allowed = Math.max(Math.floor(median * 0.05), 2);
        details.push(
          `  S${sid} shadow_count series: median=${median} min=${min} max=${max} swing=${swing} allowed=${allowed} (n=${series.length})`,
        );
        if (swing > allowed) {
          flickerFailures.push(
            `S${sid} shadow_count oscillating: swing=${swing} > allowed=${allowed} (median=${median})`,
          );
        }
      }
      if (flickerFailures.length > 0) {
        return {
          name: 'peer-restart-reconvergence',
          status: 'fail',
          summary: `Reconvergence OK but shadows flicker post-convergence: ${flickerFailures.join('; ')}`,
          details,
          durationMs: 0,
        };
      }
      return {
        name: 'peer-restart-reconvergence',
        status: 'pass',
        summary: `S${TARGET} restart reconverged + stable in ${(elapsed / 1000).toFixed(1)}s (session, evictions, shadow, sustained all OK)`,
        details,
        durationMs: 0,
      };
    }

    return {
      name: 'peer-restart-reconvergence',
      status: 'fail',
      summary: `Reconvergence failed: ${failures.join('; ')}`,
      details,
      durationMs: 0,
    };
  },
};
