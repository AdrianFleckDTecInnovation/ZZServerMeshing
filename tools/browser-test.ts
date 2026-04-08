/**
 * Playwright browser test — opens the actual client in a real browser,
 * waits for frames, then reads window.__diag to detect flicker + warping.
 *
 * Usage:
 *   npx tsx browser-test.ts [gateway-url]
 *   npx tsx browser-test.ts ws://20.29.160.152
 */
import { chromium } from 'playwright';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const CLIENT_URL = 'http://localhost:4321';
const TEST_DURATION_MS = 15000;

async function main() {
  console.log('Launching browser...');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();

  // Capture console logs
  const consoleLogs: string[] = [];
  page.on('console', (msg) => {
    consoleLogs.push(`[${msg.type()}] ${msg.text()}`);
  });

  // Start the Vite dev server
  const { execSync, spawn } = await import('child_process');

  // Kill anything on 4321
  try { execSync('lsof -ti tcp:4321 | xargs kill -9 2>/dev/null'); } catch {}
  await new Promise(r => setTimeout(r, 500));

  console.log('Starting Vite dev server...');
  const vite = spawn('npx', ['vite', '--port', '4321'], {
    cwd: process.cwd().replace(/\/tools$/, '/client'),
    env: { ...process.env, VITE_GATEWAY_URL: GW_URL },
    stdio: 'pipe',
  });

  // Wait for Vite to be ready
  await new Promise<void>((resolve) => {
    vite.stdout?.on('data', (data: Buffer) => {
      const text = data.toString();
      if (text.includes('ready') || text.includes('Local:')) resolve();
    });
    setTimeout(resolve, 8000); // fallback
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
    return {
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

  await browser.close();
  vite.kill();

  console.log(`\n${'='.repeat(60)}`);
  console.log(`  BROWSER TEST RESULTS`);
  console.log(`${'='.repeat(60)}\n`);

  if (!diag) {
    console.log('FAIL: window.__diag not found — client did not initialize');
    console.log('\nConsole logs:');
    for (const log of consoleLogs.slice(-20)) console.log(`  ${log}`);
    process.exit(1);
  }

  console.log(`Frames received:  ${diag.frames}`);
  console.log(`Frames applied:   ${diag.applied}`);
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
  const problems: string[] = [];
  if (diag.entityFlickerIn > 5) problems.push(`${diag.entityFlickerIn} flicker-in events`);
  if (diag.entityFlickerOut > 5) problems.push(`${diag.entityFlickerOut} flicker-out events`);
  if (diag.positionJumps > 0) problems.push(`${diag.positionJumps} position jumps (worst: ${diag.worstJump.toFixed(1)})`);
  if (diag.dropped > 0) problems.push(`${diag.dropped} dropped frames`);
  if (diag.applied < 20) problems.push(`only ${diag.applied} frames applied in ${TEST_DURATION_MS/1000}s`);

  if (problems.length === 0) {
    console.log('PASS — browser client shows no flicker or warping');
    process.exit(0);
  } else {
    console.log('FAIL:');
    for (const p of problems) console.log(`  - ${p}`);
    process.exit(1);
  }
}

main().catch((e) => {
  console.error('Fatal:', e);
  process.exit(1);
});
