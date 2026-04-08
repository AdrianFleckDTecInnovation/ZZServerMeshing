interface EntityState {
  id: string;
  position: number[];
  velocity: number[];
  entity_type: string;
  authority_server: number;
}

interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: EntityState[];
}

interface ServerSnapshot {
  tick: number;
  entities: Map<string, EntityState>;
}

export class Aggregator {
  private serverSnapshots = new Map<number, ServerSnapshot>();
  private lastTick = 0;

  // Metrics
  snapshotsPerSecond = 0;
  totalBandwidthBytes = 0;
  rebalanceEventsObserved = 0;
  private snapshotCount = 0;
  private lastMetricsReset = Date.now();
  private previousAuthorities = new Map<string, number>();

  onServerSnapshot(serverId: number, snapshot: WorldSnapshot, rawSizeBytes: number) {
    this.snapshotCount++;
    this.totalBandwidthBytes += rawSizeBytes;

    const entities = new Map<string, EntityState>();
    for (const e of snapshot.entities) {
      entities.set(e.id, e);

      // Detect authority changes (rebalance events)
      const prev = this.previousAuthorities.get(e.id);
      if (prev !== undefined && prev !== e.authority_server) {
        this.rebalanceEventsObserved++;
      }
      this.previousAuthorities.set(e.id, e.authority_server);
    }
    this.serverSnapshots.set(serverId, { tick: snapshot.tick, entities });
    if (snapshot.tick > this.lastTick) this.lastTick = snapshot.tick;
  }

  removeServer(serverId: number) {
    this.serverSnapshots.delete(serverId);
  }

  getSnapshot(): WorldSnapshot | null {
    if (this.serverSnapshots.size === 0) return null;
    const merged: EntityState[] = [];
    for (const snap of this.serverSnapshots.values()) {
      for (const entity of snap.entities.values()) {
        merged.push(entity);
      }
    }
    return { type: 'WorldSnapshot', tick: this.lastTick, entities: merged };
  }

  getAuthorityServer(entityId: string): number | null {
    for (const snap of this.serverSnapshots.values()) {
      if (snap.entities.has(entityId)) {
        return snap.entities.get(entityId)!.authority_server;
      }
    }
    return null;
  }

  getLeastLoadedServer(serverIds: number[]): number {
    let bestId = serverIds[0];
    let bestCount = Infinity;
    for (const sid of serverIds) {
      const snap = this.serverSnapshots.get(sid);
      const count = snap ? snap.entities.size : 0;
      if (count < bestCount) { bestCount = count; bestId = sid; }
    }
    return bestId;
  }

  updateMetrics(): { snapshotsPerSec: number; bandwidthKBps: number; rebalanceEvents: number; serverCount: number } {
    const now = Date.now();
    const elapsed = (now - this.lastMetricsReset) / 1000;
    if (elapsed >= 1) {
      this.snapshotsPerSecond = Math.round(this.snapshotCount / elapsed);
      this.snapshotCount = 0;
      this.lastMetricsReset = now;
    }
    return {
      snapshotsPerSec: this.snapshotsPerSecond,
      bandwidthKBps: Math.round(this.totalBandwidthBytes / 1024),
      rebalanceEvents: this.rebalanceEventsObserved,
      serverCount: this.serverSnapshots.size,
    };
  }
}
