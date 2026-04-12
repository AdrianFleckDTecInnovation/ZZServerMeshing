import * as THREE from 'three/webgpu';

export interface EntityData {
  id: string;
  position: THREE.Vector3;         // interpolated (rendered) position
  previousPosition: THREE.Vector3; // server position at tick N-1
  currentPosition: THREE.Vector3;  // server position at tick N (latest)
  velocity: THREE.Vector3;
  entityType: 'Player' | 'Npc';
  authorityServer: number;
  hasSecondSnapshot: boolean;      // false until we have two positions to lerp between
  lastInputSeq: number;            // last acknowledged input sequence (players only)
}

export interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: RawEntity[];
  _metrics?: GatewayMetrics;
}

export interface GatewayMetrics {
  snapshotsPerSec: number;
  bandwidthKBps: number;
  rebalanceEvents: number;
  serverCount: number;
}

type Vec3Like = [number, number, number] | { x: number; y: number; z: number };

export interface RawEntity {
  id: string;
  position: Vec3Like;
  velocity: Vec3Like;
  entity_type: 'Player' | 'Npc';
  authority_server: number;
  last_input_seq: number;
}

function toVec3(v: Vec3Like): THREE.Vector3 {
  if (Array.isArray(v)) return new THREE.Vector3(v[0], v[1], v[2]);
  return new THREE.Vector3(v.x, v.y, v.z);
}

// Colors for up to 8 servers
export const SERVER_COLORS = [
  new THREE.Color(0xff4444), // Server 1 - Red
  new THREE.Color(0x4488ff), // Server 2 - Blue
  new THREE.Color(0x44ff44), // Server 3 - Green
  new THREE.Color(0xffaa00), // Server 4 - Orange
  new THREE.Color(0xff44ff), // Server 5 - Magenta
  new THREE.Color(0x00ffff), // Server 6 - Cyan
  new THREE.Color(0xffff44), // Server 7 - Yellow
  new THREE.Color(0xffffff), // Server 8 - White
];

export const PLAYER_COLOR = new THREE.Color(0xffd700); // Gold for players

export class WorldView {
  entities: Map<string, EntityData> = new Map();
  currentTick: number = 0;
  serverEntityCounts: Map<number, number> = new Map();
  localPlayerId: string | null = null;
  metrics: GatewayMetrics | null = null;

  /** Timestamp of most recent snapshot arrival */
  private lastSnapshotTime: number = performance.now();
  /** Measured interval between snapshots (adapts to actual network rate) */
  private measuredIntervalMs: number = 100;
  private prevSnapshotTime: number = 0;
  /** Raw frame counter for Hz display */
  private frameCount: number = 0;
  private lastHzTime: number = performance.now();
  private displayHz: number = 0;
  /** Measured network snapshot rate for debug overlay */
  get measuredHz(): number { return this.displayHz; }
  /** Number of snapshots applied — skip interpolation during warmup */
  private snapshotCount: number = 0;
  private static readonly WARMUP_SNAPSHOTS = 3;

  /** Call for every frame received (before duplicate filtering) to track true network rate */
  countFrame(): void {
    this.frameCount++;
    const now = performance.now();
    const elapsed = now - this.lastHzTime;
    if (elapsed >= 1000) {
      this.displayHz = Math.round(this.frameCount / (elapsed / 1000));
      this.frameCount = 0;
      this.lastHzTime = now;
    }
  }

  applySnapshot(snapshot: WorldSnapshot): void {
    // Skip duplicate ticks — gateway may send the same tick twice
    if (snapshot.tick === this.currentTick) return;

    this.currentTick = snapshot.tick;
    const now = performance.now();
    if (this.prevSnapshotTime > 0) {
      const interval = now - this.prevSnapshotTime;
      if (interval > 10 && interval < 500) {
        this.measuredIntervalMs = this.measuredIntervalMs * 0.8 + interval * 0.2;
      }
    }
    this.prevSnapshotTime = now;

    this.snapshotCount++;

    // During warmup, don't interpolate — snap to server positions
    if (this.snapshotCount <= WorldView.WARMUP_SNAPSHOTS) {
      this.lastSnapshotTime = now;
      // Will snap all entities to currentPosition below (hasSecondSnapshot stays false)
    } else {
      // Run interpolation one last time before applying new data
      // so entity.position reflects what was actually rendered
      this.interpolate(0);
      this.lastSnapshotTime = now;
    }

    if (snapshot._metrics) {
      this.metrics = snapshot._metrics;
    }

    const incomingIds = new Set<string>();

    for (const raw of snapshot.entities) {
      const id = raw.id;
      const serverId = raw.authority_server;

      incomingIds.add(id);

      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);

      const existing = this.entities.get(id);
      const isLocalPlayer = id === this.localPlayerId;
      if (existing) {
        existing.lastInputSeq = raw.last_input_seq ?? 0;
        if (isLocalPlayer) {
          // Local player: update server state but don't touch rendered position
          // (prediction manager handles position via reconciliation)
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
        } else if (this.snapshotCount <= WorldView.WARMUP_SNAPSHOTS) {
          // During warmup: snap everything, no interpolation
          existing.position.copy(pos);
          existing.previousPosition.copy(pos);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = false;
        } else {
          // Normal: interpolate from rendered position to new server position
          existing.previousPosition.copy(existing.position);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = true;
        }
      } else {
        this.entities.set(id, {
          id,
          position: pos.clone(),
          previousPosition: pos.clone(),
          currentPosition: pos.clone(),
          velocity: vel,
          entityType: raw.entity_type,
          authorityServer: serverId,
          hasSecondSnapshot: false,
          lastInputSeq: raw.last_input_seq ?? 0,
        });
      }
    }

    // Remove entities no longer in snapshot
    for (const id of this.entities.keys()) {
      if (!incomingIds.has(id)) {
        this.entities.delete(id);
      }
    }

    // Rebuild server counts from the deduplicated entity map
    this.serverEntityCounts.clear();
    for (const entity of this.entities.values()) {
      this.serverEntityCounts.set(
        entity.authorityServer,
        (this.serverEntityCounts.get(entity.authorityServer) || 0) + 1
      );
    }
  }

  /** Apply a delta frame: update changed entities, remove deleted ones.
   *  Does NOT reset lastSnapshotTime — interpolation timer only resets on full snapshots.
   *  This prevents unchanged entities from warping back to previousPosition. */
  applyDelta(tick: number, updates: RawEntity[], removals: string[], metrics?: GatewayMetrics): void {
    if (tick === this.currentTick) return;
    this.currentTick = tick;

    if (metrics) {
      this.metrics = metrics;
    }

    // Apply updates (new or changed entities)
    for (const raw of updates) {
      const id = raw.id;
      const serverId = raw.authority_server;
      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);
      const isLocalPlayer = id === this.localPlayerId;

      const existing = this.entities.get(id);
      if (existing) {
        existing.lastInputSeq = raw.last_input_seq ?? 0;
        if (isLocalPlayer) {
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
        } else {
          // Snap previousPosition to current rendered position for smooth transition
          existing.previousPosition.copy(existing.position);
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
          existing.hasSecondSnapshot = true;
        }
      } else {
        this.entities.set(id, {
          id,
          position: pos.clone(),
          previousPosition: pos.clone(),
          currentPosition: pos.clone(),
          velocity: vel,
          entityType: raw.entity_type,
          authorityServer: serverId,
          hasSecondSnapshot: false,
          lastInputSeq: raw.last_input_seq ?? 0,
        });
      }
    }

    // Apply removals
    for (const id of removals) {
      this.entities.delete(id);
    }

    // Recompute server entity counts
    this.serverEntityCounts.clear();
    for (const entity of this.entities.values()) {
      this.serverEntityCounts.set(
        entity.authorityServer,
        (this.serverEntityCounts.get(entity.authorityServer) || 0) + 1
      );
    }
  }

  interpolate(_dt: number): void {
    const now = performance.now();
    const timeSinceSnapshot = now - this.lastSnapshotTime;
    // Use measured interval instead of hardcoded 50ms — adapts to actual network rate
    const alpha = Math.min(timeSinceSnapshot / this.measuredIntervalMs, 1.2);

    for (const entity of this.entities.values()) {
      // Local player position is managed by the prediction system
      if (entity.id === this.localPlayerId) continue;
      if (!entity.hasSecondSnapshot) {
        entity.position.copy(entity.currentPosition);
        continue;
      }
      entity.position.lerpVectors(entity.previousPosition, entity.currentPosition, alpha);
    }
  }

  getColorForEntity(entity: EntityData): THREE.Color {
    if (entity.entityType === 'Player') {
      return PLAYER_COLOR;
    }
    const idx = (entity.authorityServer - 1) % SERVER_COLORS.length;
    return SERVER_COLORS[idx >= 0 ? idx : 0];
  }
}
