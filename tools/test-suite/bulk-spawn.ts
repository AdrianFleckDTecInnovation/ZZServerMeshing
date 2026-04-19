import WebSocket from 'ws';

const coordUrl = 'https://192.168.0.208:7871';
const TOTAL = parseInt(process.argv[2] || '3500');

async function main() {
  const resp = await fetch(`${coordUrl}/servers`);
  const data = await resp.json() as any;
  const urls = data.servers.map((s: any) => `wss://${s.host}:${s.ws_port}`);
  console.log(`Got ${urls.length} server URLs`);

  const conns: WebSocket[] = [];
  for (const u of urls) {
    const ws = await new Promise<WebSocket>((res, rej) => {
      const w = new WebSocket(u, { rejectUnauthorized: false });
      w.on('open', () => res(w));
      w.on('error', rej);
      setTimeout(() => rej(new Error('timeout')), 5000);
    });
    conns.push(ws);
  }
  console.log(`Connected to ${conns.length} servers`);

  const baseline = data.servers.reduce((s: number, x: any) => s + x.entity_count, 0);
  console.log(`Baseline entity count: ${baseline}`);
  const toSpawn = Math.max(0, TOTAL - baseline);
  console.log(`Spawning ${toSpawn} entities...`);

  let i = 0;
  let batchNo = 0;
  while (i < toSpawn) {
    const batchTarget = Math.min(i + 100, toSpawn);
    while (i < batchTarget) {
      for (const ws of conns) {
        const x = (Math.random() - 0.5) * 8000;
        const z = (Math.random() - 0.5) * 8000;
        ws.send(JSON.stringify({ type: 'SpawnEntity', position: [x, 0, z], entity_type: 'npc' }));
        i++;
        if (i >= batchTarget) break;
      }
    }
    batchNo++;
    if (batchNo % 5 === 0) console.log(`  spawned ${i}/${toSpawn}`);
    await new Promise(r => setTimeout(r, 150));
  }

  await new Promise(r => setTimeout(r, 3000));
  for (const ws of conns) ws.close();
  console.log(`Done spawning ${toSpawn} entities`);
}

main().catch(e => { console.error(e); process.exit(1); });
