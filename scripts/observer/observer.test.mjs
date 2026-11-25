import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { clusterEvents, receive, Telemetry } from './telemetry.mjs';

test('client counts a complete, correctly addressed frame only once', () => {
  const client = { id: 2, tick: 0, frames: 0, bytes: 0, own: null };
  const packet = Buffer.alloc(32);
  packet.write('MSO1');
  packet.writeBigUInt64LE(3n, 4);
  packet.writeUInt16LE(2, 12);
  packet.writeUInt16LE(1, 14);
  packet.writeUInt32LE(2, 16);
  packet.writeInt32LE(-3500, 20);
  assert.equal(receive(packet, client, 12), true);
  assert.deepEqual(client, { id: 2, tick: 3, frames: 1, bytes: 32, own: [-3.5, 0, 0], neighbors: [] });
  assert.equal(receive(packet, client, 12), false);
  packet.writeBigUInt64LE(7n, 4);
  assert.equal(receive(packet.subarray(0, 31), client, 12), false);
  packet.writeUInt16LE(1, 12);
  assert.equal(receive(packet, client, 12), false);
  assert.equal(client.frames, 1);
});

test('server tick rate and client receive rates have independent clocks', () => {
  const stats = new Telemetry(),
    players = [
      { id: 1, cluster: 1 },
      { id: 2, cluster: 2 },
    ];
  const at = (a, b) => [
    { frames: a, tick: a, own: null },
    { frames: b, tick: b, own: null },
  ];
  const frame = (tick, seconds) => ({
    players,
    tick,
    seconds,
    peerApplied: tick,
    workers: [
      { id: 1, tick, seconds },
      { id: 2, tick: tick * 2, seconds },
    ],
  });
  assert.equal(stats.sample(frame(20, 1), at(0, 0), 0).medianHz, null);
  const state = stats.sample(frame(60, 3), at(25, 0), 1250);
  assert.equal(state.serverHz, 20);
  assert.equal(state.workers[1].hz, 40);
  assert.equal(state.medianHz, 10);
  assert.equal(state.minHz, 0);
  const merged = players.map((p) => ({ ...p, cluster: 1 }));
  assert.deepEqual(clusterEvents(players, merged), [{ kind: 'merge', before: [1, 2], after: [1] }]);
  assert.deepEqual(clusterEvents(merged, players), [{ kind: 'split', before: [1], after: [1, 2] }]);
});

test(
  'four mTLS servers own, merge and separate spatial islands while clients receive local and remote players',
  { timeout: 120_000 },
  async (t) => {
    const child = spawn(
      process.execPath,
      [
        fileURLToPath(new URL('launch.mjs', import.meta.url)),
        '--port',
        '0',
        '--players',
        '24',
        '--cycle-seconds',
        '8',
        '--duration',
        '35',
      ],
      {
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
      },
    );
    const closed = once(child, 'close');
    let address,
      output = '';
    child.stdout.on('data', (b) => {
      output += b;
      address = output.match(/http:\/\/127\.0\.0\.1:\d+\//)?.[0];
    });
    child.stderr.on('data', (b) => {
      output += b;
    });
    const stop = () =>
      fetch(address + 'api/stop', {
        method: 'POST',
        headers: { 'X-Mesh-Observer': '1' },
        signal: AbortSignal.timeout(2000),
      });
    t.after(async () => {
      if (child.exitCode === null && child.signalCode === null) {
        if (address) await stop().catch(() => child.kill());
        else child.kill();
      }
      await closed;
    });
    const until = Date.now() + 100_000;
    while (!address && Date.now() < until && child.exitCode === null) await delay(100);
    assert.ok(address, output);
    const counts = new Set();
    let last,
      received = false,
      isolated = false,
      shared = false,
      fused = false,
      handed = false,
      returned = false,
      visited = false;
    const end = Date.now() + 25_000;
    while (Date.now() < end) {
      last = await (await fetch(address + 'api/state')).json();
      if (last.status === 'live') {
        assert.equal(last.players.length, 24);
        assert.equal(new Set(last.players.map((p) => p.id)).size, 24);
        counts.add(new Set(last.players.map((p) => p.cluster)).size);
        assert.equal(new Set(last.workers.map((w) => w.pid)).size, 4);
        const owners = new Map(last.players.map((p) => [p.id, p.worker]));
        const groups = Map.groupBy(last.players, (p) => p.cluster);
        const westernRoute = last.routes.some(([a, b]) => (a === 1 && b === 2) || (a === 2 && b === 1));
        if (
          last.workers.reduce((n, w) => n + w.owned, 0) === 24 &&
          groups.size === 6 &&
          !westernRoute &&
          groups.get(1)?.every((p) => p.worker === 1) &&
          groups.get(2)?.every((p) => p.worker === 2)
        )
          isolated = true;
        // C03 stays shared by W1 and W3 even before the other pairs approach.
        shared ||=
          groups.size === 6 &&
          groups.get(3)?.some((p) => p.neighbors.some((id) => owners.get(id) !== p.worker));
        fused ||=
          groups.size === 4 &&
          westernRoute &&
          new Set(groups.get(5)?.map((p) => p.worker)).size === 3 &&
          groups.get(1)?.some((p) => p.neighbors.some((id) => owners.get(id) !== p.worker)) &&
          last.workers.every((w) => w.applied > 0 && w.hz > 0);
        received ||= last.players.every((p) => p.hz > 0 && p.rxTick > 0 && p.rxPosition?.length === 3);
        const moved = last.players.find((p) => p.id === last.handoffPlayer);
        handed ||= moved.worker === 2 && moved.rxPosition?.length === 3 && moved.rxTick > 0;
        returned ||=
          handed &&
          moved.worker === 1 &&
          moved.rxTick > 0 &&
          last.events.some((e) => e.kind === 'handoff' && e.target === 1);
        visited ||= last.events.some(
          (e) =>
            e.kind === 'move' &&
            e.player === last.traveler &&
            e.before.includes(4) &&
            e.after.includes(5),
        );
        if (
          received &&
          counts.has(4) &&
          counts.has(6) &&
          last.events.some((e) => e.kind === 'split') &&
          last.events.some((e) => e.kind === 'merge') &&
          isolated &&
          shared &&
          fused &&
          handed &&
          returned &&
          visited
        )
          break;
      }
      await delay(150);
    }
    assert.ok(received, 'all clients must actually decode their positions');
    assert.ok(handed && returned, 'the client must resume on each new fenced authority');
    assert.ok(visited, 'the W4 player must enter the other physical island');
    assert.ok(
      isolated,
      'separate W1 and W2 islands must have no replication route between their servers',
    );
    assert.ok(shared, 'clients in the permanent two-server island must actually receive remote players');
    assert.ok(
      fused,
      'the western and eastern mergers must involve two and three real servers respectively',
    );
    assert.ok(counts.has(6) && counts.has(4));
    assert.ok(
      last.events.some((e) => e.kind === 'merge') && last.events.some((e) => e.kind === 'split'),
    );
    assert.ok(last.players.every((p) => !p.neighbors.includes(p.id)));
    const denied = await fetch(address + 'api/stop', { method: 'POST' });
    assert.equal(denied.status, 403);
    assert.ok((await stop()).ok);
    assert.equal((await closed)[0], 0);
  },
);
