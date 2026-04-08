import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';

async function main() {
  const worldView = new WorldView();
  const network = new NetworkClient('ws://localhost:4322', worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView);

  await renderer.init();

  setInterval(() => {
    debug.setConnected(network.isConnected());
    debug.setFollowMode(renderer.followPlayer);
    debug.update();
  }, 100);
}

main().catch(console.error);
