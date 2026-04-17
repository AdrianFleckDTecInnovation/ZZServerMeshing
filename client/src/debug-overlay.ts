import { WorldView, SERVER_COLORS } from './world-view';
import { DIAGNOSTICS } from './config';

interface ServerInfo {
  server_id: number;
  entity_count: number;
}

export class DebugOverlay {
  private el: HTMLElement;
  private worldView: WorldView;
  private renderFps = 0;
  private renderFrameCount = 0;
  private lastFpsTime = performance.now();
  private connected = false;
  private followMode = true;
  private serverUrl = '';
  /** Total entity counts per server from coordinator (polled every 2s) */
  private serverTotals: Map<number, number> = new Map();
  private coordinatorUrl: string;
  private lastPollTime = 0;

  constructor(worldView: WorldView, coordinatorUrl: string) {
    this.el = document.getElementById('debug-overlay')!;
    this.worldView = worldView;
    this.coordinatorUrl = coordinatorUrl;
  }

  setConnected(connected: boolean): void { this.connected = connected; }
  setFollowMode(follow: boolean): void { this.followMode = follow; }
  setServerUrl(url: string): void { this.serverUrl = url; }

  /** Poll coordinator /servers for total entity counts */
  private async pollServerTotals(): Promise<void> {
    const now = performance.now();
    if (now - this.lastPollTime < DIAGNOSTICS.COORDINATOR_POLL_MS) return;
    this.lastPollTime = now;
    try {
      const url = this.coordinatorUrl ? `${this.coordinatorUrl}/servers` : '/servers';
      const resp = await fetch(url);
      const data = await resp.json();
      for (const s of data.servers as ServerInfo[]) {
        this.serverTotals.set(s.server_id, s.entity_count);
      }
    } catch {
      // Silently ignore — overlay will show stale or no totals
    }
  }

  /** Call from the render loop (requestAnimationFrame) to count actual render FPS */
  tick(): void {
    this.renderFrameCount++;
  }

  update(): void {
    this.pollServerTotals();

    const now = performance.now();
    if (now - this.lastFpsTime >= 1000) {
      this.renderFps = this.renderFrameCount;
      this.renderFrameCount = 0;
      this.lastFpsTime = now;
    }

    const wv = this.worldView;
    const totalEntities = wv.entities.size;
    const connStatus = this.connected ? `Connected to ${this.serverUrl}` : 'Disconnected';

    // Collect all server IDs (from both visible entities and coordinator totals)
    const allServerIds = new Set<number>();
    for (const sid of wv.serverEntityCounts.keys()) allServerIds.add(sid);
    for (const sid of this.serverTotals.keys()) allServerIds.add(sid);
    const sortedServers = [...allServerIds].sort((a, b) => a - b);

    // Compute totals for the summary line
    let totalVisible = 0;
    let totalOnServers = 0;
    for (const sid of sortedServers) {
      totalVisible += wv.serverEntityCounts.get(sid) ?? 0;
      totalOnServers += this.serverTotals.get(sid) ?? 0;
    }

    // Per-server bars showing visible/total
    let serverLines = '';
    const maxCount = Math.max(1, ...sortedServers.map(sid => this.serverTotals.get(sid) ?? wv.serverEntityCounts.get(sid) ?? 1));
    for (const serverId of sortedServers) {
      const visible = wv.serverEntityCounts.get(serverId) ?? 0;
      const total = this.serverTotals.get(serverId);
      const colorIdx = (serverId - 1) % SERVER_COLORS.length;
      const c = SERVER_COLORS[colorIdx >= 0 ? colorIdx : 0];
      const hex = '#' + c.getHexString();
      // Bar shows visible proportion of total
      const barMax = total ?? visible;
      const bw = DIAGNOSTICS.BAR_WIDTH;
      const barWidth = barMax > 0 ? Math.round((visible / maxCount) * bw) : 0;
      const totalBarWidth = barMax > 0 ? Math.round((barMax / maxCount) * bw) : 0;
      const visibleBar = '\u2588'.repeat(Math.min(barWidth, bw));
      const remainBar = '\u2591'.repeat(Math.max(0, totalBarWidth - barWidth));
      const emptyBar = '\u2591'.repeat(Math.max(0, bw - totalBarWidth));
      const label = total !== undefined ? `${visible}/${total}` : `${visible}`;
      serverLines += `  <span style="color:${hex}">S${serverId} ${visibleBar}${remainBar}${emptyBar} ${label}</span>\n`;
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

    // Network snapshot rate (measured from actual arrivals)
    const netHz = wv.measuredHz > 0 ? `${wv.measuredHz.toFixed(0)}` : '?';

    // AoI summary
    const aoiLine = totalOnServers > 0
      ? `AoI: ${totalVisible}/${totalOnServers} visible (${Math.round(totalVisible / totalOnServers * 100)}%)`
      : '';

    this.el.innerHTML =
`${connStatus} | Tick ${wv.currentTick} | Render ${this.renderFps} FPS | Net ${netHz} Hz
Entities: ${totalEntities} | Camera: ${this.followMode ? 'follow' : 'free'} [F]
${playerInfo}
${aoiLine}
${serverLines}`;
  }
}
