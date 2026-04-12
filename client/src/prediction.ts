/**
 * Client-side prediction + server reconciliation.
 *
 * The client applies movement inputs locally for instant feedback, then
 * reconciles with the server's authoritative state each time a snapshot
 * arrives. Unacknowledged inputs are replayed on top of the server position
 * and the result is smoothly blended toward the current rendered position.
 */

import type { EntityData } from './world-view';

interface PendingInput {
  seq: number;
  direction: [number, number, number];
  dt: number; // actual frame dt when this input was applied
}

const PLAYER_SPEED = 5.0;
const MAX_PENDING = 128;
const CORRECTION_BLEND = 0.2; // exponential blend per reconciliation

export class PredictionManager {
  private seq = 0;
  private pending: PendingInput[] = [];

  /** Record and locally apply a movement input. Returns the new seq number. */
  pushInput(
    direction: [number, number, number],
    entity: EntityData,
    dt: number,
  ): number {
    this.seq++;

    this.pending.push({ seq: this.seq, direction, dt });
    if (this.pending.length > MAX_PENDING) {
      this.pending.shift();
    }

    // Apply prediction locally
    const len = Math.sqrt(
      direction[0] ** 2 + direction[1] ** 2 + direction[2] ** 2,
    );
    if (len > 0) {
      const nx = direction[0] / len;
      const nz = direction[2] / len;
      entity.position.x += nx * PLAYER_SPEED * dt;
      entity.position.z += nz * PLAYER_SPEED * dt;
    }

    return this.seq;
  }

  /**
   * Called when an authoritative snapshot arrives for the local player.
   * Discards acknowledged inputs, replays the rest on top of the server
   * position, and smoothly blends the rendered position toward the result.
   */
  reconcile(entity: EntityData, ackedSeq: number): void {
    // Drop inputs the server has already processed
    this.pending = this.pending.filter((p) => p.seq > ackedSeq);

    // Start from server's authoritative position
    let rx = entity.currentPosition.x;
    let rz = entity.currentPosition.z;

    // Replay unacknowledged inputs
    for (const input of this.pending) {
      const [dx, , dz] = input.direction;
      const len = Math.sqrt(dx * dx + dz * dz);
      if (len > 0) {
        rx += (dx / len) * PLAYER_SPEED * input.dt;
        rz += (dz / len) * PLAYER_SPEED * input.dt;
      }
    }

    // Smooth blend toward reconciled position
    entity.position.x += (rx - entity.position.x) * CORRECTION_BLEND;
    entity.position.z += (rz - entity.position.z) * CORRECTION_BLEND;
    // Y stays 0 (flat world)
    entity.position.y = 0;
  }

  get currentSeq(): number {
    return this.seq;
  }
}
