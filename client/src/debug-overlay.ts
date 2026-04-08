import { WorldView, SERVER_COLORS } from './world-view';

export class DebugOverlay {
  private el: HTMLElement;
  private worldView: WorldView;
  private fps = 0;
  private frameCount = 0;
  private lastFpsTime = performance.now();
  private connected = false;

  constructor(worldView: WorldView) {
    this.el = document.getElementById('debug-overlay')!;
    this.worldView = worldView;
  }

  setConnected(connected: boolean): void {
    this.connected = connected;
  }

  update(): void {
    // FPS calculation
    this.frameCount++;
    const now = performance.now();
    if (now - this.lastFpsTime >= 1000) {
      this.fps = this.frameCount;
      this.frameCount = 0;
      this.lastFpsTime = now;
    }

    const wv = this.worldView;
    const totalEntities = wv.entities.size;
    const connStatus = this.connected ? '🟢 Connected' : '🔴 Disconnected';

    let serverLines = '';
    const sortedServers = [...wv.serverEntityCounts.entries()].sort((a, b) => a[0] - b[0]);
    for (const [serverId, count] of sortedServers) {
      const colorIdx = (serverId - 1) % SERVER_COLORS.length;
      const c = SERVER_COLORS[colorIdx >= 0 ? colorIdx : 0];
      const hex = '#' + c.getHexString();
      serverLines += `  <span style="color:${hex}">Server ${serverId}: ${count} entities</span>\n`;
    }

    this.el.innerHTML = `${connStatus}
Tick: ${wv.currentTick} | FPS: ${this.fps}
Entities: ${totalEntities}
${serverLines ? 'Servers:\n' + serverLines : ''}`;
  }
}
