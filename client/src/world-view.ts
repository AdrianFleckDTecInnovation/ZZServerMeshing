import * as THREE from 'three/webgpu';

export interface EntityData {
  id: string;
  position: THREE.Vector3;
  velocity: THREE.Vector3;
  targetPosition: THREE.Vector3;
  entityType: 'Player' | 'Npc';
  authorityServer: number;
  lastUpdate: number;
}

export interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: RawEntity[];
}

// Raw entity format from server JSON -- handles both array [x,y,z] and object {x,y,z} for Vec3
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
  /** Our local player entity ID, set by the gateway. */
  localPlayerId: string | null = null;

  applySnapshot(snapshot: WorldSnapshot): void {
    this.currentTick = snapshot.tick;

    const incomingIds = new Set<string>();
    this.serverEntityCounts.clear();

    for (const raw of snapshot.entities) {
      const id = raw.id;
      const serverId = raw.authority_server;

      incomingIds.add(id);

      // Track per-server counts
      this.serverEntityCounts.set(
        serverId,
        (this.serverEntityCounts.get(serverId) || 0) + 1
      );

      const pos = toVec3(raw.position);
      const vel = toVec3(raw.velocity);

      const existing = this.entities.get(id);
      if (existing) {
        existing.velocity.copy(vel);
        existing.targetPosition.copy(pos);
        existing.entityType = raw.entity_type;
        existing.authorityServer = serverId;
        existing.lastUpdate = performance.now();
      } else {
        this.entities.set(id, {
          id,
          position: pos.clone(),
          velocity: vel,
          targetPosition: pos,
          entityType: raw.entity_type,
          authorityServer: serverId,
          lastUpdate: performance.now(),
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

  interpolate(dt: number): void {
    const lerpFactor = Math.min(1.0, dt * 15); // Smooth interpolation
    for (const entity of this.entities.values()) {
      entity.position.lerp(entity.targetPosition, lerpFactor);
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
