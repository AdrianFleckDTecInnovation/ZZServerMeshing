import { WorldView, SERVER_COLORS } from './world-view';

export class DebugOverlay {
  private el: HTMLElement;
  private worldView: WorldView;
  private renderFps = 0;
  private renderFrameCount = 0;
  private lastFpsTime = performance.now();
  private connected = false;
  private followMode = true;
  private serverUrl = '';

  constructor(worldView: WorldView) {
    this.el = document.getElementById('debug-overlay')!;
    this.worldView = worldView;
  }

  setConnected(connected: boolean): void { this.connected = connected; }
  setFollowMode(follow: boolean): void { this.followMode = follow; }
  setServerUrl(url: string): void { this.serverUrl = url; }

  /** Call from the render loop (requestAnimationFrame) to count actual render FPS */
  tick(): void {
    this.renderFrameCount++;
  }

  update(): void {
    const now = performance.now();
    if (now - this.lastFpsTime >= 1000) {
      this.renderFps = this.renderFrameCount;
      this.renderFrameCount = 0;
      this.lastFpsTime = now;
    }

    const wv = this.worldView;
    const totalEntities = wv.entities.size;
    const connStatus = this.connected ? `Connected to ${this.serverUrl}` : 'Disconnected';

    // Per-server bars
    let serverLines = '';
    const sortedServers = [...wv.serverEntityCounts.entries()].sort((a, b) => a[0] - b[0]);
    const maxCount = Math.max(1, ...sortedServers.map(([, c]) => c));
    for (const [serverId, count] of sortedServers) {
      const colorIdx = (serverId - 1) % SERVER_COLORS.length;
      const c = SERVER_COLORS[colorIdx >= 0 ? colorIdx : 0];
      const hex = '#' + c.getHexString();
      const barWidth = Math.round((count / maxCount) * 20);
      const bar = '\u2588'.repeat(barWidth) + '\u2591'.repeat(20 - barWidth);
      serverLines += `  <span style="color:${hex}">S${serverId} ${bar} ${count}</span>\n`;
    }

    // Player info
    let playerInfo = '';
    if (wv.localPlayerId) {
      const player = wv.entities.get(wv.localPlayerId);
      if (player) {
        playerInfo = `Player: S${player.authorityServer} (${Math.round(player.position.x)}, ${Math.round(player.position.z)})`;
      } else {
        playerInfo = 'Player: spawning...';
      }
    }

    // Network metrics
    let metricsLine = '';
    if (wv.metrics) {
      const m = wv.metrics;
      metricsLine = `Net: ${m.snapshotsPerSec} snap/s | ${m.bandwidthKBps} KB | ${m.rebalanceEvents} rebalances`;
    }

    // Network snapshot rate (measured from actual arrivals)
    const netHz = wv.measuredHz > 0 ? `${wv.measuredHz.toFixed(0)}` : '?';

    this.el.innerHTML =
`${connStatus} | Tick ${wv.currentTick} | Render ${this.renderFps} FPS | Net ${netHz} Hz
Entities: ${totalEntities} | Camera: ${this.followMode ? 'follow' : 'free'} [F]
${playerInfo}
${metricsLine}
${serverLines}`;
  }
}
