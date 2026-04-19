import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';
import { DIAGNOSTICS } from './config';

async function main() {
  const worldView = new WorldView();
  const coordinatorUrl = import.meta.env.VITE_COORDINATOR_URL || '';
  // NetworkClient now owns the /assign lifecycle: it fetches on construction
  // and re-fetches on every reconnect so the client migrates away from
  // crashed or drained servers automatically (design 016 Batch 2).
  const network = new NetworkClient(coordinatorUrl, worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView, coordinatorUrl);

  renderer.onRenderFrame = () => debug.tick();
  await renderer.init();

  setInterval(() => {
    debug.setConnected(network.isConnected());
    debug.setFollowMode(renderer.followPlayer);
    debug.setServerUrl(network.getServerUrl());
    debug.update();
  }, DIAGNOSTICS.OVERLAY_UPDATE_MS);
}

main().catch(console.error);
