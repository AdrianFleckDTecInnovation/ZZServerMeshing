import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';

async function main() {
  const worldView = new WorldView();
  const gatewayUrl = import.meta.env.VITE_GATEWAY_URL || 'ws://localhost:4322';
  const network = new NetworkClient(gatewayUrl, worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView);

  renderer.onRenderFrame = () => debug.tick();
  await renderer.init();

  setInterval(() => {
    debug.setConnected(network.isConnected());
    debug.setFollowMode(renderer.followPlayer);
    debug.update();
  }, 100);
}

main().catch(console.error);
