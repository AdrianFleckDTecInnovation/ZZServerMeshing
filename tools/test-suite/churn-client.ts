// Simulated client that maintains a connection and re-assigns on disconnect.
// Reports migration events. Used to validate Batch 2's /assign refetch.
import WebSocket from 'ws';

const coordUrl = 'https://192.168.0.208:7871';

type Events = { time: number; msg: string }[];
const events: Events = [];
function log(msg: string) {
  const t = Date.now();
  events.push({ time: t, msg });
  console.log(`[${((t - T0) / 1000).toFixed(1)}s] ${msg}`);
}
const T0 = Date.now();

async function assign(): Promise<string> {
  const resp = await fetch(`${coordUrl}/assign`);
  const data = await resp.json() as any;
  return data.ws_url;
}

async function connect(url: string): Promise<WebSocket> {
  return new Promise((res, rej) => {
    const ws = new WebSocket(url, { rejectUnauthorized: false });
    ws.on('open', () => res(ws));
    ws.on('error', rej);
    setTimeout(() => rej(new Error('connect timeout')), 5000);
  });
}

async function main() {
  let attempts = 0;
  let currentUrl: string | null = null;
  let ws: WebSocket | null = null;

  while (attempts < 50) {
    try {
      const url = await assign();
      if (url !== currentUrl) {
        log(`assigned ${url}`);
        currentUrl = url;
      }
      ws = await connect(url);
      log(`connected to ${url}`);

      await new Promise<void>((res) => {
        ws!.on('close', () => { log(`disconnect from ${url}`); res(); });
        ws!.on('error', (e) => { log(`error: ${e.message}`); res(); });
      });

      ws = null;
    } catch (e: any) {
      log(`connect failed: ${e.message}`);
      await new Promise(r => setTimeout(r, 500));
    }
    attempts++;
  }

  log('exiting');
}

main().catch(e => { console.error(e); process.exit(1); });
