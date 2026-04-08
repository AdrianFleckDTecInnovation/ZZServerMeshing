import WebSocket from 'ws';

const GATEWAY_URL = process.argv[2] || 'ws://localhost:4322';
const ENTITY_COUNT = parseInt(process.argv[3] || '200');
const SPAWN_RATE = parseInt(process.argv[4] || '10'); // entities per second

interface EntityState {
  id: string;
  authority_server: number;
  entity_type: string;
}

interface Snapshot {
  type: string;
  tick: number;
  entities: EntityState[];
  _metrics?: { snapshotsPerSec: number; bandwidthKBps: number; rebalanceEvents: number; serverCount: number };
}

// Metrics
let entitiesSpawned = 0;
let snapshotsReceived = 0;
let lastEntityCount = 0;
let lastTick = 0;
const serverCounts = new Map<number, number>();
let rebalancesSeen = 0;
const previousAuth = new Map<string, number>();
const snapshotTimes: number[] = [];
let startTime = Date.now();

console.log(`\nServer Meshing Stress Test`);
console.log(`  Gateway:  ${GATEWAY_URL}`);
console.log(`  Entities: ${ENTITY_COUNT}`);
console.log(`  Rate:     ${SPAWN_RATE}/sec\n`);

const ws = new WebSocket(GATEWAY_URL);

ws.on('open', () => {
  console.log('Connected to gateway\n');
  startTime = Date.now();

  // Spawn entities at the configured rate
  const interval = setInterval(() => {
    if (entitiesSpawned >= ENTITY_COUNT) {
      clearInterval(interval);
      console.log(`\nAll ${ENTITY_COUNT} entities spawned. Monitoring for 15s...\n`);
      setTimeout(() => printSummary(), 15000);
      return;
    }

    const batch = Math.min(SPAWN_RATE, ENTITY_COUNT - entitiesSpawned);
    for (let i = 0; i < batch; i++) {
      const x = (Math.random() - 0.5) * 800;
      const z = (Math.random() - 0.5) * 800;
      ws.send(JSON.stringify({
        type: 'SpawnEntity',
        position: [x, 0, z],
        entity_type: 'npc',
      }));
      entitiesSpawned++;
    }
  }, 1000);
});

ws.on('message', (data) => {
  const raw = data.toString();
  const msg: Snapshot = JSON.parse(raw);

  if (msg.type === 'WorldSnapshot') {
    snapshotsReceived++;
    snapshotTimes.push(Date.now());
    lastEntityCount = msg.entities.length;
    lastTick = msg.tick;

    // Per-server counts + rebalance detection
    serverCounts.clear();
    for (const e of msg.entities) {
      serverCounts.set(e.authority_server, (serverCounts.get(e.authority_server) || 0) + 1);
      const prev = previousAuth.get(e.id);
      if (prev !== undefined && prev !== e.authority_server) {
        rebalancesSeen++;
      }
      previousAuth.set(e.id, e.authority_server);
    }
  }
});

ws.on('error', (err) => {
  console.error('WebSocket error:', err.message);
  process.exit(1);
});

// Progress every 5 seconds
const progressInterval = setInterval(() => {
  const elapsed = ((Date.now() - startTime) / 1000).toFixed(0);
  const dist = [...serverCounts.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([s, c]) => `S${s}:${c}`)
    .join(' ');
  const snapRate = getSnapshotRate();
  console.log(
    `[${elapsed}s] Spawned: ${entitiesSpawned} | Visible: ${lastEntityCount} | ` +
    `Tick: ${lastTick} | ${snapRate} snap/s | Rebalances: ${rebalancesSeen} | ${dist}`
  );
}, 5000);

function getSnapshotRate(): number {
  const now = Date.now();
  const recent = snapshotTimes.filter(t => now - t < 2000);
  return Math.round(recent.length / 2);
}

function printSummary() {
  clearInterval(progressInterval);
  const elapsed = (Date.now() - startTime) / 1000;

  // Snapshot interval stats
  const intervals: number[] = [];
  for (let i = 1; i < snapshotTimes.length; i++) {
    intervals.push(snapshotTimes[i] - snapshotTimes[i - 1]);
  }
  const avgInterval = intervals.length > 0
    ? (intervals.reduce((a, b) => a + b, 0) / intervals.length).toFixed(1)
    : 'N/A';
  const minInterval = intervals.length > 0 ? Math.min(...intervals) : 0;
  const maxInterval = intervals.length > 0 ? Math.max(...intervals) : 0;

  const dist = [...serverCounts.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([s, c]) => `  S${s}: ${c} entities`)
    .join('\n');

  console.log(`
============================================
  STRESS TEST SUMMARY
============================================
  Duration:       ${elapsed.toFixed(1)}s
  Entities:       ${entitiesSpawned} spawned, ${lastEntityCount} visible
  Snapshots:      ${snapshotsReceived} received
  Snapshot rate:  ${getSnapshotRate()}/sec
  Snap interval:  avg=${avgInterval}ms min=${minInterval}ms max=${maxInterval}ms
  Rebalances:     ${rebalancesSeen} authority changes observed
  Final tick:     ${lastTick}

  Server Distribution:
${dist}
============================================
`);

  ws.close();
  process.exit(0);
}
