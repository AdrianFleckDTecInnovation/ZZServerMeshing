import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';

async function main() {
  const worldView = new WorldView();
  const network = new NetworkClient('ws://localhost:7878', worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView);

  await renderer.init();

  // Update debug overlay at a lower rate
  setInterval(() => {
    debug.setConnected(network.isConnected());
    debug.update();
  }, 100);
}

main().catch(console.error);
