import WebSocket from 'ws';

const URL = 'wss://192.168.0.15:7878';
const COUNT = 400; // extra entities onto S1
const CONNS = 10;

async function connect(): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(URL, { rejectUnauthorized: false });
    ws.binaryType = 'arraybuffer';
    ws.on('open', () => resolve(ws));
    ws.on('error', reject);
    ws.on('message', () => {}); // drain
    setTimeout(() => reject(new Error('WS timeout')), 5000);
  });
}

function sleep(ms: number) { return new Promise(r => setTimeout(r, ms)); }

async function main() {
  const wss: WebSocket[] = [];
  for (let i = 0; i < CONNS; i++) wss.push(await connect());
  console.log(`Connected ${wss.length} to S1`);

  const perConn = Math.ceil(COUNT / CONNS);
  for (let n = 0; n < perConn; n++) {
    for (const ws of wss) {
      const x = (Math.random() - 0.5) * 8000;
      const z = (Math.random() - 0.5) * 8000;
      const msg = JSON.stringify({ type: 'SpawnEntity', position: [x, 0, z], entity_type: 'Npc' });
      if (ws.readyState === WebSocket.OPEN) ws.send(msg);
    }
    await sleep(250); // ~40/sec total
  }

  await sleep(500);
  for (const ws of wss) ws.close();
  console.log(`Sent ~${perConn * CONNS} spawns to S1`);
  process.exit(0);
}
main().catch(e => { console.error(e); process.exit(1); });
