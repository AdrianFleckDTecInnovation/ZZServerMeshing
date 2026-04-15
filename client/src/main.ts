import { WorldView } from './world-view';
import { NetworkClient } from './network';
import { Renderer } from './renderer';
import { DebugOverlay } from './debug-overlay';

async function getServerUrl(): Promise<string> {
  const coordinatorUrl = import.meta.env.VITE_COORDINATOR_URL || '';
  const assignUrl = coordinatorUrl ? `${coordinatorUrl}/assign` : '/assign';
  const resp = await fetch(assignUrl);
  const data = await resp.json();
  console.log(`Assigned to server: ${data.ws_url}`);
  return data.ws_url;
}

async function main() {
  const worldView = new WorldView();
  const coordinatorUrl = import.meta.env.VITE_COORDINATOR_URL || '';
  const serverUrl = await getServerUrl();
  const network = new NetworkClient(serverUrl, worldView);
  const renderer = new Renderer(worldView, network);
  const debug = new DebugOverlay(worldView, coordinatorUrl);

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
