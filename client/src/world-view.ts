import * as THREE from 'three/webgpu';
import { NETCODE, COLORS } from './config';

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
  lastUpdateTime: number;          // performance.now() when this entity was last updated by the server
}

export interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: RawEntity[];
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
export const SERVER_COLORS = COLORS.SERVERS.map(c => new THREE.Color(c));

export const PLAYER_COLOR = new THREE.Color(COLORS.PLAYER);

export class WorldView {
  entities: Map<string, EntityData> = new Map();
  currentTick: number = 0;
  serverEntityCounts: Map<number, number> = new Map();
  localPlayerId: string | null = null;
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
      if (interval > NETCODE.INTERVAL_MIN_MS && interval < NETCODE.INTERVAL_MAX_MS) {
        this.measuredIntervalMs = this.measuredIntervalMs * (1 - NETCODE.INTERVAL_EMA_NEW) + interval * NETCODE.INTERVAL_EMA_NEW;
      }
    }
    this.prevSnapshotTime = now;

    this.snapshotCount++;

    // During warmup, don't interpolate — snap to server positions
    if (this.snapshotCount > NETCODE.WARMUP_SNAPSHOTS) {
      // Run interpolation one last time before applying new data
      // so entity.position reflects what was actually rendered
      this.interpolate(0);
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
        existing.lastUpdateTime = now;
        if (isLocalPlayer) {
          // Local player: update server state but don't touch rendered position
          // (prediction manager handles position via reconciliation)
          existing.currentPosition.copy(pos);
          existing.velocity.copy(vel);
          existing.entityType = raw.entity_type;
          existing.authorityServer = serverId;
        } else if (this.snapshotCount <= NETCODE.WARMUP_SNAPSHOTS) {
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
          lastUpdateTime: now,
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
   *  Only resets lastUpdateTime for entities present in the delta's update list.
   *  Unchanged entities continue interpolating with their existing timer. */
  applyDelta(tick: number, updates: RawEntity[], removals: string[]): void {
    if (tick === this.currentTick) return;
    this.currentTick = tick;
    const now = performance.now();

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
        existing.lastUpdateTime = now;
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
          lastUpdateTime: now,
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

    for (const [id, entity] of this.entities) {
      // Local player position is managed by the prediction system
      if (entity.id === this.localPlayerId) continue;

      // Safety net: remove entities not updated by the server for too long.
      // This catches ghost entities from baseline drift or missed delta removals.
      // Full snapshots arrive every 5s, so any legitimate entity will be refreshed well within this window.
      if (now - entity.lastUpdateTime > NETCODE.STALE_ENTITY_MS) {
        this.entities.delete(id);
        continue;
      }

      if (!entity.hasSecondSnapshot) {
        entity.position.copy(entity.currentPosition);
        continue;
      }
      // Per-entity interpolation: each entity tracks its own last update time
      const timeSinceUpdate = now - entity.lastUpdateTime;
      const alpha = Math.min(timeSinceUpdate / this.measuredIntervalMs, NETCODE.MAX_INTERPOLATION_ALPHA);
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
