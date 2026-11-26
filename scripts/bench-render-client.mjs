// Measure one rendered browser client against an already running WebSocket demo.
import { writeFile } from 'node:fs/promises';
import { setTimeout as sleep } from 'node:timers/promises';
import { parseArgs } from 'node:util';
import { chromium } from 'playwright';

const { values } = parseArgs({ options: {
  url: { type: 'string', default: 'http://127.0.0.1:4321' },
  seconds: { type: 'string', default: '10' },
  channel: { type: 'string', default: process.platform === 'win32' ? 'msedge' : 'chromium' },
  out: { type: 'string' },
} });
const seconds = Number(values.seconds);
if (!Number.isInteger(seconds) || seconds < 4 || seconds > 60) throw new Error('Use 4..60 seconds');
const viewport = { width: 1280, height: 720 };
const browser = await chromium.launch({ channel: values.channel, headless: true });
try {
  const page = await browser.newPage({ viewport, deviceScaleFactor: 1 });
  const protocol = await page.context().newCDPSession(page);
  await protocol.send('Network.enable');
  let snapshots = 0;
  let acknowledgements = 0;
  function frameType({ response }) {
    return response.opcode === 2 ? Buffer.from(response.payloadData.slice(0, 4), 'base64')[0] : 0;
  }
  protocol.on('Network.webSocketFrameReceived', event => {
    const type = frameType(event);
    if (type === 1 || type === 2) snapshots++;
  });
  protocol.on('Network.webSocketFrameSent', event => {
    if (frameType(event) === 3) acknowledgements++;
  });
  await page.goto(values.url, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => /Connected to .*Render [1-9]\d* FPS/.test(
    document.querySelector('#debug-overlay')?.textContent ?? ''), null, { timeout: 35000 });
  await sleep(1500);
  const startingSnapshots = snapshots;
  const startingAcks = acknowledgements;
  await page.evaluate(ms => {
    const overlay = document.querySelector('#debug-overlay');
    const result = { frame_intervals_ms: [], visible_entities: [], elapsed_ms: 0 };
    const readOverlay = () => {
      const count = /Entities: (\d+)/.exec(overlay.textContent);
      if (count) result.visible_entities.push(Number(count[1]));
    };
    readOverlay();
    const observer = new MutationObserver(readOverlay);
    observer.observe(overlay, { childList: true });
    let first, previous;
    function frame(now) {
      first ??= now;
      if (previous !== undefined) result.frame_intervals_ms.push(now - previous);
      previous = now;
      if (now - first < ms) {
        requestAnimationFrame(frame);
      } else {
        observer.disconnect();
        result.elapsed_ms = now - first;
        window.meshRenderSample = result;
      }
    }
    requestAnimationFrame(frame);
  }, seconds * 1000);
  await page.keyboard.down('w');
  await sleep(2000);
  await page.keyboard.up('w');
  const completed = await page.waitForFunction(() => window.meshRenderSample, null, { timeout: seconds * 1000 + 10000 });
  const snapshotCount = snapshots - startingSnapshots;
  const ackCount = acknowledgements - startingAcks;
  const sample = await completed.jsonValue();
  const intervals = sample.frame_intervals_ms.toSorted((a, b) => a - b);
  if (!intervals.length || !sample.visible_entities.length || !snapshotCount || !ackCount) {
    throw new Error('Missing rendered frames, visible entities, snapshots or acknowledgements');
  }
  const info = await browser.newBrowserCDPSession();
  const { gpu } = await info.send('SystemInfo.getInfo');
  const result = {
    browser: browser.version(), channel: values.channel, viewport, angle_renderer: gpu.auxAttributes?.glRenderer,
    seconds, snapshots: snapshotCount, sent_snapshot_acks: ackCount,
    average_frames_per_second: 1000 * intervals.length / sample.elapsed_ms,
    frame_interval_p50_ms: intervals[Math.ceil(0.50 * intervals.length) - 1],
    frame_interval_p95_ms: intervals[Math.ceil(0.95 * intervals.length) - 1],
    ...sample,
  };
  const output = JSON.stringify(result, null, 2) + '\n';
  if (values.out) await writeFile(values.out, output);
  else process.stdout.write(output);
} finally {
  await browser.close();
}
