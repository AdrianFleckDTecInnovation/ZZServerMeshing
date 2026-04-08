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

/**
 * Aggregates entity state from all sim-servers into a single unified view.
 * Each sim-server sends only its authoritative entities.
 * The aggregator merges them — no overlap expected.
 */
export class Aggregator {
  private serverSnapshots = new Map<number, ServerSnapshot>();
  private lastTick = 0;

  onServerSnapshot(serverId: number, snapshot: WorldSnapshot) {
    const entities = new Map<string, EntityState>();
    for (const e of snapshot.entities) {
      entities.set(e.id, e);
    }
    this.serverSnapshots.set(serverId, {
      tick: snapshot.tick,
      entities,
    });
    if (snapshot.tick > this.lastTick) {
      this.lastTick = snapshot.tick;
    }
  }

  removeServer(serverId: number) {
    this.serverSnapshots.delete(serverId);
  }

  /** Build a merged snapshot from all servers. */
  getSnapshot(): WorldSnapshot | null {
    if (this.serverSnapshots.size === 0) return null;

    const merged: EntityState[] = [];

    for (const [_serverId, snap] of this.serverSnapshots) {
      for (const entity of snap.entities.values()) {
        merged.push(entity);
      }
    }

    return {
      type: 'WorldSnapshot',
      tick: this.lastTick,
      entities: merged,
    };
  }

  /** Look up which server is authoritative for a given entity. */
  getAuthorityServer(entityId: string): number | null {
    for (const [_serverId, snap] of this.serverSnapshots) {
      if (snap.entities.has(entityId)) {
        return snap.entities.get(entityId)!.authority_server;
      }
    }
    return null;
  }

  /** Return the server ID with the fewest entities. */
  getLeastLoadedServer(serverIds: number[]): number {
    let bestId = serverIds[0];
    let bestCount = Infinity;
    for (const sid of serverIds) {
      const snap = this.serverSnapshots.get(sid);
      const count = snap ? snap.entities.size : 0;
      if (count < bestCount) {
        bestCount = count;
        bestId = sid;
      }
    }
    return bestId;
  }
}
