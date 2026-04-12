import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';

async function getServerUrl(): Promise<string> {
  const coordinatorUrl = import.meta.env.VITE_COORDINATOR_URL || 'http://localhost:7871';
  const resp = await fetch(`${coordinatorUrl}/assign`);
  const data = await resp.json();
  console.log(`Assigned to server: ${data.ws_url}`);
  return data.ws_url;
}

async function main() {
  const worldView = new WorldView();
  const serverUrl = await getServerUrl();
  const network = new NetworkClient(serverUrl, worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView);

  renderer.onRenderFrame = () => debug.tick();
  await renderer.init();

  setInterval(() => {
    debug.setConnected(network.isConnected());
    debug.setFollowMode(renderer.followPlayer);
    debug.setServerUrl(network.getServerUrl());
    debug.update();
  }, 100);
}

main().catch(console.error);
