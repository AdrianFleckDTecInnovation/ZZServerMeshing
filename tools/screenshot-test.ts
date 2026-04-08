/**
 * Take rapid screenshots to visually verify rendering.
 * No assertions — just capture what the browser actually shows.
 */
import { chromium } from 'playwright';
import { execSync, spawn } from 'child_process';
import { mkdirSync } from 'fs';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const CLIENT_URL = 'http://localhost:4321';
const SCREENSHOT_DIR = '/tmp/mesh-screenshots';
const NUM_SCREENSHOTS = 10;
const INTERVAL_MS = 500;

async function main() {
  mkdirSync(SCREENSHOT_DIR, { recursive: true });

  // Kill old client
  try { execSync('lsof -ti tcp:4321 | xargs kill -9 2>/dev/null'); } catch {}
  await new Promise(r => setTimeout(r, 500));

  // Start Vite
  const vite = spawn('npx', ['vite', '--port', '4321'], {
    cwd: process.cwd().replace(/\/tools$/, '/client'),
    env: { ...process.env, VITE_GATEWAY_URL: GW_URL },
    stdio: 'pipe',
  });
  await new Promise<void>((resolve) => {
    vite.stdout?.on('data', (d: Buffer) => {
      if (d.toString().includes('ready') || d.toString().includes('Local:')) resolve();
    });
    setTimeout(resolve, 8000);
  });

  const browser = await chromium.launch({ headless: false });
  const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });

  // Collect console output
  page.on('console', msg => {
    if (msg.text().includes('[NET]') || msg.text().includes('error')) {
      console.log(`  BROWSER: ${msg.text()}`);
    }
  });

  console.log(`Opening ${CLIENT_URL}...`);
  await page.goto(CLIENT_URL, { timeout: 10000 });

  // Wait for connection
  await page.waitForTimeout(3000);

  // Take screenshots
  console.log(`Taking ${NUM_SCREENSHOTS} screenshots every ${INTERVAL_MS}ms...`);
  for (let i = 0; i < NUM_SCREENSHOTS; i++) {
    const path = `${SCREENSHOT_DIR}/frame-${i.toString().padStart(3, '0')}.png`;
    await page.screenshot({ path });
    console.log(`  ${path}`);
    await page.waitForTimeout(INTERVAL_MS);
  }

  // Get diag data
  const diag = await page.evaluate(() => {
    const d = (window as any).__diag;
    if (!d) return null;
    return {
      frames: d.frames,
      applied: d.applied,
      dropped: d.dropped,
      dupes: d.dupes,
      entityFlickerIn: d.entityFlickerIn,
      entityFlickerOut: d.entityFlickerOut,
      positionJumps: d.positionJumps,
      positionChecks: d.positionChecks,
      worstJump: d.worstJump,
      errors: d.errors?.slice(0, 10) || [],
    };
  });

  console.log('\nDiagnostics:', JSON.stringify(diag, null, 2));

  await browser.close();
  vite.kill();

  console.log(`\nScreenshots saved to ${SCREENSHOT_DIR}/`);
}

main().catch(e => { console.error(e); process.exit(1); });
