/**
 * Playwright browser test — opens the actual client in a real browser,
 * waits for frames, then reads window.__diag to detect flicker + warping.
 *
 * Usage:
 *   npm --prefix tools run browser -- http://127.0.0.1:4321
 *   A ws:// URL can also select one sim-server for the local client.
 */
import { chromium, type Browser } from 'playwright';

const target = new URL(process.argv[2] || 'http://127.0.0.1:4321');
const direct = ['ws:', 'wss:'].includes(target.protocol);
const CLIENT_URL = direct ? 'http://127.0.0.1:4321' : target.href;
const TEST_DURATION_MS = 15000;

async function run(browser: Browser) {
  const page = await browser.newPage();
  if (direct) {
    await page.route('**/assign', route => route.fulfill({ json: { ws_url: target.href } }));
  }

  // Capture console logs
  const consoleLogs: string[] = [];
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  page.on('console', (msg) => {
    consoleLogs.push(`[${msg.type()}] ${msg.text()} ${msg.location().url}`);
  });

  console.log(`Opening ${CLIENT_URL}...`);
  await page.goto(CLIENT_URL, { waitUntil: 'domcontentloaded', timeout: 10000 });

  // Wait for WebSocket connection + some frames
  // No manual warmup skip — client handles it internally now
  console.log(`Collecting for ${TEST_DURATION_MS / 1000}s...`);
  await page.waitForTimeout(TEST_DURATION_MS);

  // Read diagnostics from the browser
  const diag = await page.evaluate(() => {
    const d = (window as any).__diag;
    if (!d) return null;
    const overlay = document.querySelector('#debug-overlay')?.textContent ?? '';
    return {
      renderFps: Number(overlay.match(/Render (\d+) FPS/)?.[1] ?? 0),
      frames: d.frames,
      applied: d.applied,
      dropped: d.dropped,
      dupes: d.dupes,
      textFrames: d.textFrames,
      entityFlickerIn: d.entityFlickerIn,
      entityFlickerOut: d.entityFlickerOut,
      positionJumps: d.positionJumps,
      positionChecks: d.positionChecks,
      worstJump: d.worstJump,
      errors: d.errors.slice(0, 20),
      perFrame: d.perFrame.slice(-50),
    };
  });

  console.log(`\n${'='.repeat(60)}`);
  console.log(`  BROWSER TEST RESULTS`);
  console.log(`${'='.repeat(60)}\n`);

  if (!diag) {
    console.log('FAIL: window.__diag not found — client did not initialize');
    console.log('\nConsole logs:');
    for (const log of consoleLogs.slice(-20)) console.log(`  ${log}`);
    process.exitCode = 1;
    return;
  }

  console.log(`Frames received:  ${diag.frames}`);
  console.log(`Frames applied:   ${diag.applied}`);
  console.log(`Render rate:      ${diag.renderFps} FPS`);
  console.log(`Frames dropped:   ${diag.dropped}`);
  console.log(`Duplicate ticks:  ${diag.dupes}`);
  console.log(`Text frames:      ${diag.textFrames}`);
  console.log('');
  console.log(`Flicker-in:       ${diag.entityFlickerIn}`);
  console.log(`Flicker-out:      ${diag.entityFlickerOut}`);
  console.log(`Position jumps:   ${diag.positionJumps}/${diag.positionChecks} (${diag.positionChecks > 0 ? (diag.positionJumps/diag.positionChecks*100).toFixed(2) : 0}%)`);
  console.log(`Worst jump:       ${diag.worstJump.toFixed(1)} units`);

  if (diag.errors.length > 0) {
    console.log(`\nErrors (${diag.errors.length}):`);
    for (const e of diag.errors) console.log(`  ${e}`);
  }

  // Show per-frame details for frames with issues
  const badFrames = diag.perFrame.filter((f: any) => f.jumps > 0 || f.flickerIn > 0 || f.flickerOut > 0);
  if (badFrames.length > 0) {
    console.log(`\nBad frames (${badFrames.length}):`);
    for (const f of badFrames.slice(0, 20)) {
      console.log(`  tick=${f.tick} entities=${f.entities} jumps=${f.jumps} flicker_in=${f.flickerIn} flicker_out=${f.flickerOut} servers=${JSON.stringify(f.servers)}`);
    }
  }

  // Show some console logs for context
  const netLogs = consoleLogs.filter(l => l.includes('[NET]') || l.includes('error') || l.includes('warn'));
  if (netLogs.length > 0) {
    console.log(`\nRelevant console logs:`);
    for (const l of netLogs.slice(-10)) console.log(`  ${l}`);
  }

  console.log(`\n${'='.repeat(60)}`);
  const problems: string[] = [...pageErrors];
  if (diag.renderFps <= 0) problems.push('render loop did not update the live FPS counter');
  if (diag.errors.length) problems.push(`${diag.errors.length} client errors`);
  if (diag.entityFlickerIn > 5) problems.push(`${diag.entityFlickerIn} flicker-in events`);
  if (diag.entityFlickerOut > 5) problems.push(`${diag.entityFlickerOut} flicker-out events`);
  if (diag.positionJumps > 0) problems.push(`${diag.positionJumps} position jumps (worst: ${diag.worstJump.toFixed(1)})`);
  if (diag.dropped > 0) problems.push(`${diag.dropped} dropped frames`);
  if (diag.applied < 20) problems.push(`only ${diag.applied} frames applied in ${TEST_DURATION_MS/1000}s`);

  if (problems.length === 0) {
    console.log('PASS — browser rendered the mesh and met the snapshot checks');
  } else {
    console.log('FAIL:');
    for (const p of problems) console.log(`  - ${p}`);
    process.exitCode = 1;
  }
}

async function main() {
  const browser = await chromium.launch({ headless: true, channel: process.env.PLAYWRIGHT_CHANNEL });
  try { await run(browser); }
  finally { await browser.close(); }
}

main().catch((e) => {
  console.error('Fatal:', e);
  process.exit(1);
});
