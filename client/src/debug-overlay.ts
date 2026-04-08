import { WorldView, SERVER_COLORS } from './world-view';

export class DebugOverlay {
  private el: HTMLElement;
  private worldView: WorldView;
  private fps = 0;
  private frameCount = 0;
  private lastFpsTime = performance.now();
  private connected = false;
  private followMode = true;

  constructor(worldView: WorldView) {
    this.el = document.getElementById('debug-overlay')!;
    this.worldView = worldView;
  }

  setConnected(connected: boolean): void {
    this.connected = connected;
  }

  setFollowMode(follow: boolean): void {
    this.followMode = follow;
  }

  update(): void {
    this.frameCount++;
    const now = performance.now();
    if (now - this.lastFpsTime >= 1000) {
      this.fps = this.frameCount;
      this.frameCount = 0;
      this.lastFpsTime = now;
    }

    const wv = this.worldView;
    const totalEntities = wv.entities.size;
    const connStatus = this.connected ? 'Connected' : 'Disconnected';
    const playerId = wv.localPlayerId;

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
    if (playerId) {
      const player = wv.entities.get(playerId);
      if (player) {
        playerInfo = `Player: S${player.authorityServer} (${Math.round(player.position.x)}, ${Math.round(player.position.z)})`;
      } else {
        playerInfo = 'Player: spawning...';
      }
    }

    this.el.innerHTML =
`${connStatus} | Tick ${wv.currentTick} | FPS ${this.fps}
Entities: ${totalEntities} | Camera: ${this.followMode ? 'follow' : 'free'} [F]
${playerInfo}
${serverLines}`;
  }
}
