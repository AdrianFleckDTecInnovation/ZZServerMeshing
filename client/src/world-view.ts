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
  private static readonly TICK_DURATION_MS = 50; // 20Hz

  applySnapshot(snapshot: WorldSnapshot): void {
    this.currentTick = snapshot.tick;
    this.lastSnapshotTime = performance.now();

    if (snapshot._metrics) {
      this.metrics = snapshot._metrics;
    }

    const incomingIds = new Set<string>();
    this.serverEntityCounts.clear();

    for (const raw of snapshot.entities) {
      const id = raw.id;
      const serverId = raw.authority_server;

      incomingIds.add(id);
      this.serverEntityCounts.set(
        serverId,
        (this.serverEntityCounts.get(serverId) || 0) + 1
      );

      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);

      const existing = this.entities.get(id);
      if (existing) {
        // Shift current -> previous, set new current
        existing.previousPosition.copy(existing.currentPosition);
        existing.currentPosition.copy(pos);
        existing.velocity.copy(vel);
        existing.entityType = raw.entity_type;
        existing.authorityServer = serverId;
        existing.hasSecondSnapshot = true;
      } else {
        // First time seeing this entity — snap to position
        this.entities.set(id, {
          id,
          position: pos.clone(),
          previousPosition: pos.clone(),
          currentPosition: pos.clone(),
          velocity: vel,
          entityType: raw.entity_type,
          authorityServer: serverId,
          hasSecondSnapshot: false,
        });
      }
    }

    // Remove entities no longer in snapshot
    for (const id of this.entities.keys()) {
      if (!incomingIds.has(id)) {
        this.entities.delete(id);
      }
    }
  }

  interpolate(_dt: number): void {
    const now = performance.now();
    const timeSinceSnapshot = now - this.lastSnapshotTime;
    // alpha: 0 at snapshot arrival, 1.0 one tick later, cap at 1.2 for slight extrapolation
    const alpha = Math.min(timeSinceSnapshot / WorldView.TICK_DURATION_MS, 1.2);

    for (const entity of this.entities.values()) {
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
