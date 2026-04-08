/**
 * Rapid-fire screenshots at ~60fps to catch frame-level jitter/warping.
 */
import { chromium } from 'playwright';
import { execSync, spawn } from 'child_process';
import { mkdirSync } from 'fs';

const GW_URL = process.argv[2] || 'ws://localhost:4322';
const SCREENSHOT_DIR = '/tmp/mesh-rapid';
const NUM_SCREENSHOTS = 30;

async function main() {
  mkdirSync(SCREENSHOT_DIR, { recursive: true });
  try { execSync('lsof -ti tcp:4321 | xargs kill -9 2>/dev/null'); } catch {}
  await new Promise(r => setTimeout(r, 500));

  const vite = spawn('npx', ['vite', '--port', '4321'], {
    cwd: process.cwd().replace(/\/tools$/, '/client'),
    env: { ...process.env, VITE_GATEWAY_URL: GW_URL },
    stdio: 'pipe',
  });
  await new Promise<void>((resolve) => {
    vite.stdout?.on('data', (d: Buffer) => {
      if (d.toString().includes('Local:')) resolve();
    });
    setTimeout(resolve, 8000);
  });

  const browser = await chromium.launch({ headless: false });
  const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
  await page.goto('http://localhost:4321', { timeout: 10000 });
  await page.waitForTimeout(5000); // let it stabilize

  console.log(`Taking ${NUM_SCREENSHOTS} rapid screenshots...`);
  for (let i = 0; i < NUM_SCREENSHOTS; i++) {
    await page.screenshot({ path: `${SCREENSHOT_DIR}/f${i.toString().padStart(3, '0')}.png` });
  }

  // Also grab rendered entity positions directly from the WorldView
  const worldState = await page.evaluate(() => {
    const wv = (window as any).__worldView;
    // Try to access the worldView through the debug overlay or renderer
    const diag = (window as any).__diag;
    if (!diag) return { error: 'no diag' };

    // Read entity positions from the diag's prevPositions
    const entities: Array<{ id: string; cx: number; cz: number; px: number; pz: number; server: number }> = [];
    // We can't directly access worldView from here, but we can check perFrame
    return {
      lastFrames: diag.perFrame.slice(-5),
      positionJumps: diag.positionJumps,
      flickerIn: diag.entityFlickerIn,
      flickerOut: diag.entityFlickerOut,
      applied: diag.applied,
      errors: diag.errors,
    };
  });

  console.log('\nWorld state:', JSON.stringify(worldState, null, 2));

  await browser.close();
  vite.kill();
  console.log(`\nScreenshots: ${SCREENSHOT_DIR}/f*.png`);
}

main().catch(e => { console.error(e); process.exit(1); });
