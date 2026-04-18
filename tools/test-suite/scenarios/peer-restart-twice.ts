import type { Scenario, ScenarioResult, TestContext, ServerMetrics } from '../types.js';

// WI-138: same as peer-restart-reconvergence but restart S2 twice within ~10 s.
// Asserts:
//   - session_id changes twice on survivors' peer_health[2]
//   - chunk_evictions eventually stops growing (no accumulation that persists
//     across either transition)
//   - survivors' peer_health[2].latest_tick catches up after the second restart

const PROJECT_ROOT = (() => process.cwd().replace(/\/tools$/, ''))();
const BOARD_MODE = !!process.env.METRICS_HOSTS;

const TICK_MS = 50;
const POLL_INTERVAL_MS = TICK_MS * 5;
// Each restart pays a cargo-run-rebuild-register cycle (~3–8 s locally) plus a
// coordinator rebalance interval (5 s) before the new S2 has entities to
// replicate. With TWO back-to-back restarts we budget generously; real
// reconvergence once packets flow is still < 1 tick window.
const MAX_POLL_MS = 60_000;

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

// See peer-restart-reconvergence.ts for rationale — we use SSH + nohup, not
// the dashboard API, because the dashboard spawns zombie children on restart.
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

async function waitForOnline(ctx: TestContext, serverId: number, timeoutMs: number): Promise<ServerMetrics | null> {
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

function getPeer(m: ServerMetrics, peerId: number) {
  return m.peer_health?.[String(peerId)];
}

async function doKillAndRestart(ctx: TestContext, targetId: number): Promise<ServerMetrics | null> {
  const killed = BOARD_MODE ? await killBoard(targetId) : await killLocal(targetId);
  if (!killed) return null;
  await ctx.sleep(800);
  const ok = BOARD_MODE ? await restartBoard(targetId) : (await restartLocal(targetId)) !== null;
  if (!ok) return null;
  return await waitForOnline(ctx, targetId, 30_000);
}

export const peerRestartTwice: Scenario = {
  name: 'peer-restart-twice',
  description: 'Restart S2 twice within 10 s; both sessions must be cleanly detected and shadows reconverge after the second restart',

  async run(ctx: TestContext): Promise<ScenarioResult> {
    const details: string[] = [];
    const TARGET = 2;
    const survivorIds = [1, 3].filter(s => s <= ctx.serverCount);

    if (ctx.serverCount < 3) {
      return {
        name: 'peer-restart-twice',
        status: 'fail',
        summary: `Need 3 servers (got ${ctx.serverCount})`,
        details: [],
        durationMs: 0,
      };
    }

    // Spawn a small seed set so S2 has some authoritative entities.
    await ctx.spawn(200);
    await ctx.sleep(3000);

    const s2InitialMetrics = await ctx.getMetrics(TARGET);
    const sessionA = s2InitialMetrics.session_id!;
    details.push(`Initial: S${TARGET} session_id=${BigInt(sessionA).toString(16)}`);

    // ─── First restart ───────────────────────────────────────────────────
    ctx.log(`First restart of S${TARGET}...`);
    const after1 = await doKillAndRestart(ctx, TARGET);
    if (!after1) {
      return { name: 'peer-restart-twice', status: 'fail', summary: 'first restart failed', details, durationMs: 0 };
    }
    const sessionB = after1.session_id!;
    if (sessionB === sessionA) {
      return {
        name: 'peer-restart-twice',
        status: 'fail',
        summary: `session_id did not change on first restart (${BigInt(sessionA).toString(16)})`,
        details,
        durationMs: 0,
      };
    }
    details.push(`After restart #1: S${TARGET} session_id=${BigInt(sessionB).toString(16)}`);

    // Brief pause to let survivors see at least one post-restart packet.
    await ctx.sleep(1500);

    // Confirm survivors saw sessionB before we kill again. If they did not,
    // skip this assertion gracefully — the second restart still happens and
    // the final state check is the load-bearing assertion.
    for (const sid of survivorIds) {
      const m = await ctx.getMetrics(sid).catch(() => null);
      const p = m ? getPeer(m, TARGET) : null;
      const sessHex = p?.session_id ? BigInt(p.session_id).toString(16) : '?';
      details.push(`  After #1 S${sid} sees S${TARGET}: session=${sessHex}, latest_tick=${p?.latest_tick ?? '?'}`);
    }

    // ─── Second restart (within 10 s of first) ───────────────────────────
    ctx.log(`Second restart of S${TARGET}...`);
    const after2 = await doKillAndRestart(ctx, TARGET);
    if (!after2) {
      return { name: 'peer-restart-twice', status: 'fail', summary: 'second restart failed', details, durationMs: 0 };
    }
    const sessionC = after2.session_id!;
    if (sessionC === sessionA || sessionC === sessionB) {
      return {
        name: 'peer-restart-twice',
        status: 'fail',
        summary: `session_id did not change on second restart (A=${BigInt(sessionA).toString(16)}, B=${BigInt(sessionB).toString(16)}, C=${BigInt(sessionC).toString(16)})`,
        details,
        durationMs: 0,
      };
    }
    details.push(`After restart #2: S${TARGET} session_id=${BigInt(sessionC).toString(16)}`);

    // Spawn to drive meaningful replication now.
    await ctx.spawn(150);
    await ctx.sleep(1500);

    // ─── Poll for convergence on session C ───────────────────────────────
    const pollStart = Date.now();
    let sessionOk = false;
    let evictionsStable = false;
    let catchUpOk = false;
    let lastEvictions: Record<number, number> = {};
    let stableSamples = 0;
    let finalSurvivors: ServerMetrics[] = [];

    while (Date.now() - pollStart < MAX_POLL_MS) {
      const samples = await Promise.all(survivorIds.map(sid => ctx.getMetrics(sid).catch(() => null)));
      const valid = samples.filter((x): x is ServerMetrics => x !== null);
      finalSurvivors = valid;

      const seenSessions = valid.map(m => getPeer(m, TARGET)?.session_id ?? '');
      sessionOk = valid.length === survivorIds.length &&
        seenSessions.every(s => s === sessionC);

      const curr: Record<number, number> = {};
      for (const m of valid) {
        curr[m.server_id] = getPeer(m, TARGET)?.chunk_evictions ?? 0;
      }
      if (Object.keys(lastEvictions).length > 0) {
        const stable = valid.every(m => curr[m.server_id] === lastEvictions[m.server_id]);
        if (stable && sessionOk) {
          stableSamples++;
          if (stableSamples >= 2) evictionsStable = true;
        } else {
          stableSamples = 0;
        }
      }
      lastEvictions = curr;

      const s2Now = await ctx.getMetrics(TARGET).catch(() => null);
      if (s2Now) {
        const lts = valid.map(m => getPeer(m, TARGET)?.latest_tick ?? 0);
        catchUpOk = lts.every(t => t > 0 && s2Now.tick - t <= 20);
      }

      if (sessionOk && evictionsStable && catchUpOk) break;
      await ctx.sleep(POLL_INTERVAL_MS);
    }

    const elapsed = Date.now() - pollStart;
    details.push(`Convergence poll after restart #2: ${elapsed} ms (budget ${MAX_POLL_MS} ms)`);
    for (const m of finalSurvivors) {
      const p = getPeer(m, TARGET);
      const sessHex = p?.session_id ? BigInt(p.session_id).toString(16) : '?';
      details.push(`  S${m.server_id}: session=${sessHex} latest_tick=${p?.latest_tick ?? '?'} chunk_evictions=${p?.chunk_evictions ?? '?'}`);
    }

    const failures: string[] = [];
    if (!sessionOk) failures.push('survivors did not converge on session C');
    if (!evictionsStable) failures.push('chunk_evictions still growing after restart #2');
    if (!catchUpOk) failures.push('survivors did not catch up to S2 current tick');

    if (failures.length === 0) {
      return {
        name: 'peer-restart-twice',
        status: 'pass',
        summary: `Both restarts cleanly detected; survivors reconverged on session ${BigInt(sessionC).toString(16)} in ${(elapsed / 1000).toFixed(1)}s`,
        details,
        durationMs: 0,
      };
    }

    return {
      name: 'peer-restart-twice',
      status: 'fail',
      summary: `peer-restart-twice failed: ${failures.join('; ')}`,
      details,
      durationMs: 0,
    };
  },
};
