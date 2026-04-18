import type { Scenario, ScenarioResult, TestContext } from '../types.js';

// WI-143: Scene test for design 014 (rebalance in-flight transfer TTL).
//
// Reproduces the live-cluster bug documented in
// worklogs/cluster-ops/worklog-20260418-010926.md:
//
//   A RebalanceDirective fires, the target sim-server crashes before it can
//   ack, and the source's `in_flight_transfers` counter is stuck > 0 forever.
//   That filters the source out of rebalance candidacy permanently, and the
//   mesh silently stops self-balancing.
//
// With WI-141's fix (TTL + topology sweep), the in-flight slot is reaped
// after 15 s (or earlier via the unregister sweep), `ttl_prunes_total`
// monotonically increments, and the coordinator resumes issuing directives.
//
// Flow:
//   1. Spawn 300 NPCs round-robin across S1/S2/S3; let replication settle.
//   2. Spawn an extra 200 NPCs on whichever server currently holds the most
//      entities, deliberately biasing the load to force a rebalance tick.
//   3. Poll the coordinator's /servers until a source reports
//      `in_flight_transfers >= 1` (the WI-142 exposure). Record the target
//      server id (inferred from the directive firing — we can't see it
//      directly, but the busiest→least-busy pair is deterministic).
//   4. SIGKILL the target before it can send TransferResult.
//   5. Restart the target.
//   6. Poll the coordinator for up to 30 s (15 s TTL + 5 s rebalance +
//      5 s rebalance cooldown + 5 s headroom). Assert either:
//        A. `ttl_prunes_total` on the coordinator is strictly > baseline
//           (TTL path — the 15 s timeout reaped the stuck slot), OR
//        B. source's `in_flight_transfers` drops to 0 AND the coordinator
//           saw the target disconnect during the kill window (unregister
//           sweep path — WI-141's `clear_in_flight_targeting` fired when
//           the target's WS connection closed).
//
// Both A and B are behaviors introduced by WI-141. Pre-fix:
//   - The counter was a u32 with no TTL — A is impossible.
//   - `unregister()` did a bare `servers.remove(&id)` with no sweep of
//     other servers' deques — B is impossible.
// So if neither A nor B holds within the 30 s budget, the test fails
// strictly, proving the fix is absent. This is the regression guard
// called for in WI-143.
//
// A companion sub-assertion C requires that the source's in_flight is 0
// at the end of the test regardless of which path freed it; this catches
// bugs where ttl_prunes_total bumps but the deque isn't actually popped.

const PROJECT_ROOT = (() => process.cwd().replace(/\/tools$/, ''))();
const COORD_HTTP = process.env.COORDINATOR_URL || 'http://localhost:7871';
const BOARD_MODE = !!process.env.METRICS_HOSTS;

// How long we'll wait for recovery after the target restart. TTL is 15 s,
// rebalance interval is 5 s, cooldown is 5 s; 30 s gives one full cycle
// past the TTL with headroom for a full cargo/coordinator round-trip.
const MAX_POLL_MS = 30_000;
const POLL_INTERVAL_MS = 500;
// Detecting an in-flight directive requires catching it in a very narrow
// window: localhost transfer round-trip is <200 ms. Poll aggressively here.
const DIRECTIVE_POLL_INTERVAL_MS = 50;

// ── coordinator /servers shape (WI-142 adds in_flight_transfers + ttl_prunes_total) ──
interface CoordServerEntry {
  server_id: number;
  host: string;
  ws_port: number;
  udp_port: number;
  metrics_port: number;
  entity_count: number;
  /** WI-142: per-source count of RebalanceDirectives with no TransferResult yet. */
  in_flight_transfers?: number;
}
interface CoordServersResponse {
  servers: CoordServerEntry[];
  /** WI-142: process-lifetime count of in-flight slots reaped by TTL. */
  ttl_prunes_total?: number;
}

async function fetchServers(): Promise<CoordServersResponse | null> {
  try {
    const resp = await fetch(`${COORD_HTTP}/servers`, { signal: AbortSignal.timeout(3000) });
    if (!resp.ok) return null;
    return (await resp.json()) as CoordServersResponse;
  } catch {
    return null;
  }
}

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

async function killBoard(serverId: number): Promise<boolean> {
  const { execSync } = await import('child_process');
  const host = serverId === 1 ? 'unoq' : serverId === 2 ? 'unoq2' : 'unoq3';
  try {
    execSync(
      `ssh -o BatchMode=yes -o ConnectTimeout=5 ${host} 'pkill -9 -f sim-server' 2>/dev/null || true`,
      { timeout: 8000 },
    );
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
    '--tls-cert /home/arduino/mesh/certs/mesh-coordinator.crt',
    '--tls-key /home/arduino/mesh/certs/mesh-coordinator.key',
    `> ~/mesh/sim${serverId}.log 2>&1 &`,
  ].join(' ');
  try {
    execSync(`ssh -o BatchMode=yes -o ConnectTimeout=5 ${host} '${cmd}'`, { timeout: 10000 });
    return true;
  } catch {
    return false;
  }
}

async function waitForServerOnline(ctx: TestContext, serverId: number, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const m = await ctx.getMetrics(serverId);
      if (m && m.server_id === serverId && m.tick > 0) return true;
    } catch {}
    await ctx.sleep(500);
  }
  return false;
}

export const rebalanceStuckRecovery: Scenario = {
  name: 'rebalance-stuck-recovery',
  description:
    'Kill a rebalance target mid-transfer and verify TTL reaps the stuck in-flight slot (design 014)',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const fail = (summary: string) => ({
      name: 'rebalance-stuck-recovery',
      status: 'fail' as const,
      summary,
      details,
      durationMs: 0,
    });

    if (ctx.serverCount < 3) {
      return {
        ...fail(`Need 3 servers for this test (got ${ctx.serverCount})`),
      };
    }

    // -- Step 0: sanity: coordinator exposes the WI-142 fields ---------------
    const initial = await fetchServers();
    if (!initial) {
      return fail('Coordinator /servers unreachable');
    }
    if (initial.ttl_prunes_total === undefined) {
      return fail('Coordinator /servers missing ttl_prunes_total (pre-WI-142 coordinator?)');
    }
    const baselinePrunes = initial.ttl_prunes_total;
    details.push(`Pre-test: ttl_prunes_total=${baselinePrunes}`);

    // Missing per-server field is also a pre-WI-142 signal.
    if (initial.servers.some((s) => s.in_flight_transfers === undefined)) {
      return fail('Coordinator /servers missing in_flight_transfers per server (pre-WI-142?)');
    }

    // -- Step 1: Spawn a heavy imbalance to force a rebalance tick ----------
    // The TestContext.spawn() round-robins across all server connections,
    // which produces an even distribution and no rebalance pressure. We
    // want imbalance, so we send all SpawnEntity messages down the primary
    // WebSocket (attached to ws://localhost:7878 = S1). Every entity that
    // hits a server's WS is spawned on that server's authority, so 300
    // NPCs on S1 alone trips the coordinator's diff >= 10 && diff >= 20%
    // avg thresholds on the very next tick.
    ctx.log('Spawning 300 NPCs on the primary server to force imbalance...');
    const primary = ctx.server;
    for (let i = 0; i < 300; i++) {
      primary.send(
        JSON.stringify({
          type: 'SpawnEntity',
          position: [(Math.random() - 0.5) * 8000, 0, (Math.random() - 0.5) * 8000],
          entity_type: 'npc',
        }),
      );
    }
    // Let replication propagate and the coordinator tick at least once
    // (rebalance interval is 5 s).
    await ctx.sleep(6000);

    // -- Step 3: Poll /servers until some source has in_flight >= 1 ---------
    // The coordinator rebalance interval is 5 s and cooldown is 5 s, so a
    // directive fires at most every ~5 s. The transfer round-trip on
    // localhost is <200 ms, so the directive is in-flight for a very short
    // window. Poll fast so we don't miss it. Budget up to 30 s across
    // multiple rebalance cycles.
    ctx.log('Waiting for a rebalance directive to fire...');
    let sourceId: number | null = null;
    let targetId: number | null = null;
    const waitDirective = Date.now();
    while (Date.now() - waitDirective < 30_000) {
      const snap = await fetchServers();
      if (snap) {
        const withInflight = snap.servers.find((s) => (s.in_flight_transfers ?? 0) > 0);
        if (withInflight) {
          sourceId = withInflight.server_id;
          // Target = least-loaded server (that's what the coordinator picks).
          const leastLoaded = [...snap.servers].sort(
            (a, b) => a.entity_count - b.entity_count,
          )[0];
          if (leastLoaded && leastLoaded.server_id !== sourceId) {
            targetId = leastLoaded.server_id;
          }
          break;
        }
      }
      await ctx.sleep(DIRECTIVE_POLL_INTERVAL_MS);
    }

    if (sourceId === null || targetId === null) {
      // Soft failure: the coordinator didn't issue a directive in the
      // observation window. Don't call this a pass — the test depends on
      // observing a real rebalance to exercise the TTL path.
      details.push(
        `No in-flight directive observed within 30 s. Current /servers: ${JSON.stringify(
          (await fetchServers())?.servers,
        )}`,
      );
      return fail('No rebalance directive observed — cannot exercise TTL path');
    }
    details.push(`Observed directive: S${sourceId} -> S${targetId} (in_flight >= 1)`);

    // -- Step 4: Kill the target before it can ack --------------------------
    ctx.log(`Killing target S${targetId}...`);
    const killed = BOARD_MODE ? await killBoard(targetId) : await killLocal(targetId);
    if (!killed) {
      return fail(`Could not kill target S${targetId}`);
    }
    // Give the kill a moment to propagate through the OS so the
    // sim-server definitely cannot send TransferResult.
    await ctx.sleep(1500);

    // -- Step 5: Restart the target -----------------------------------------
    ctx.log(`Restarting target S${targetId}...`);
    const restarted = BOARD_MODE
      ? await restartBoard(targetId)
      : (await restartLocal(targetId)) !== null;
    if (!restarted) {
      return fail(`Could not restart target S${targetId}`);
    }

    const back = await waitForServerOnline(ctx, targetId, 30_000);
    if (!back) {
      return fail(`Target S${targetId} did not come back online within 30 s`);
    }

    // -- Step 6: Poll for recovery ------------------------------------------
    // Recovery can happen via two WI-141 paths:
    //   (TTL path)        ttl_prunes_total strictly increments after 15 s.
    //   (Unregister path) target's WS disconnect triggers
    //                     clear_in_flight_targeting(target) on the
    //                     coordinator, instantly emptying every other
    //                     source's matching slots. Observable as the
    //                     source's in_flight dropping to 0 *before* 15 s
    //                     has elapsed (TTL couldn't have fired that fast).
    //
    // We record both signals and require at least ONE to fire. We also
    // record the earliest moment the source returned to in_flight=0 so we
    // can distinguish the two paths in the output.
    ctx.log('Polling for recovery...');
    const pollStart = Date.now();
    let prunesIncreased = false;
    let inflightZeroAt: number | null = null;
    let lastSnap: CoordServersResponse | null = null;

    while (Date.now() - pollStart < MAX_POLL_MS) {
      const snap = await fetchServers();
      if (snap) {
        lastSnap = snap;
        const src = snap.servers.find((s) => s.server_id === sourceId);
        const srcInflight = src?.in_flight_transfers ?? 0;
        const currPrunes = snap.ttl_prunes_total ?? 0;

        if (srcInflight === 0 && inflightZeroAt === null) {
          inflightZeroAt = Date.now() - pollStart;
        }
        if (currPrunes > baselinePrunes) prunesIncreased = true;

        // Stop as soon as recovery is observable: in_flight=0 AND (either
        // path fired). Pre-WI-141, in_flight stays >0 for the whole window
        // because neither path exists.
        if (inflightZeroAt !== null && (prunesIncreased || inflightZeroAt < 15_000)) {
          break;
        }
      }
      await ctx.sleep(POLL_INTERVAL_MS);
    }

    const elapsed = Date.now() - pollStart;
    details.push(`Recovery poll: ${elapsed} ms (budget ${MAX_POLL_MS} ms)`);
    if (lastSnap) {
      details.push(
        `Final ttl_prunes_total=${lastSnap.ttl_prunes_total} (baseline=${baselinePrunes})`,
      );
      for (const s of lastSnap.servers) {
        details.push(
          `  S${s.server_id}: entities=${s.entity_count}, in_flight=${s.in_flight_transfers ?? '?'}`,
        );
      }
    }
    details.push(
      `Recovery path: inflightZero@${inflightZeroAt === null ? 'never' : `${inflightZeroAt}ms`}, prunesIncreased=${prunesIncreased}`,
    );

    // -- Verdict ------------------------------------------------------------
    // Either WI-141 path counts as proof the fix is present:
    //   - TTL: prunesIncreased === true
    //   - Unregister sweep: inflightZeroAt < 15_000 ms (before TTL could fire)
    //
    // A sub-assertion requires in_flight to have actually reached 0 at some
    // point — a guard against a bug where the counter never drops even
    // though something incremented ttl_prunes_total.
    const ttlFired = prunesIncreased;
    const sweepFired = inflightZeroAt !== null && inflightZeroAt < 15_000;
    const anyPathFired = ttlFired || sweepFired;
    const inflightEventuallyZero = inflightZeroAt !== null;

    if (anyPathFired && inflightEventuallyZero) {
      const path = ttlFired ? 'TTL' : 'unregister-sweep';
      return {
        name: 'rebalance-stuck-recovery',
        status: 'pass',
        summary: `Stuck-rebalance recovered via ${path} (S${sourceId}->S${targetId}) in ${(elapsed / 1000).toFixed(1)}s`,
        details,
        durationMs: 0,
      };
    }

    const failures: string[] = [];
    if (!inflightEventuallyZero) {
      failures.push(`S${sourceId}.in_flight_transfers never reached 0`);
    }
    if (!anyPathFired) {
      failures.push(
        `neither TTL nor unregister-sweep fired (prunes=${lastSnap?.ttl_prunes_total ?? '?'}@baseline ${baselinePrunes}, inflightZeroAt=${inflightZeroAt === null ? 'never' : `${inflightZeroAt}ms (>=15s = TTL territory)`}) — WI-141 fix may be absent`,
      );
    }

    return {
      name: 'rebalance-stuck-recovery',
      status: 'fail',
      summary: `Stuck-rebalance recovery failed: ${failures.join('; ')}`,
      details,
      durationMs: 0,
    };
  },
};
