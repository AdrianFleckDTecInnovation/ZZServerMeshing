import { WorldView, WorldSnapshot } from './world-view';
import { decodeBinaryFrame } from './binary-protocol';
import { PredictionManager } from './prediction';

// Exposed to window for Playwright test access
interface FrameRecord {
  tick: number;
  type: 'full' | 'delta';
  entities: number;
  updates: number;
  removals: number;
  servers: Record<number, number>;
  jumps: number;
  flickerIn: number;
  flickerOut: number;
}

interface ClientDiag {
  frames: number;
  applied: number;
  dropped: number;
  dupes: number;
  textFrames: number;
  fullFrames: number;
  deltaFrames: number;
  totalUpdates: number;
  totalRemovals: number;
  entityCountSwings: number;
  entityFlickerIn: number;
  entityFlickerOut: number;
  positionJumps: number;
  positionChecks: number;
  worstJump: number;
  bytesReceived: number;
  fullFrameBytes: number;
  deltaFrameBytes: number;
  errors: string[];
  prevEntityIds: Set<string>;
  prevPositions: Map<string, [number, number, number]>;
  perFrame: FrameRecord[];
}

function initDiag(): ClientDiag {
  const diag: ClientDiag = {
    frames: 0, applied: 0, dropped: 0, dupes: 0, textFrames: 0,
    fullFrames: 0, deltaFrames: 0, totalUpdates: 0, totalRemovals: 0,
    entityCountSwings: 0, entityFlickerIn: 0, entityFlickerOut: 0,
    positionJumps: 0, positionChecks: 0, worstJump: 0,
    bytesReceived: 0, fullFrameBytes: 0, deltaFrameBytes: 0,
    errors: [],
    prevEntityIds: new Set(),
    prevPositions: new Map(),
    perFrame: [],
  };
  (window as any).__diag = diag;
  return diag;
}

/** Print a compact data flow summary to console. Call from browser: __diagReport() */
function installDiagReport(diag: ClientDiag): void {
  (window as any).__diagReport = () => {
    const recent = diag.perFrame.slice(-20);
    console.table(recent.map(f => ({
      tick: f.tick,
      type: f.type,
      entities: f.entities,
      upd: f.updates,
      rem: f.removals,
      flickIn: f.flickerIn,
      flickOut: f.flickerOut,
      jumps: f.jumps,
      servers: Object.entries(f.servers).map(([k,v]) => `S${k}:${v}`).join(' '),
    })));
    console.log(
      `Totals: ${diag.frames} frames (${diag.fullFrames} full, ${diag.deltaFrames} delta) | ` +
      `${diag.totalUpdates} updates, ${diag.totalRemovals} removals | ` +
      `flicker: +${diag.entityFlickerIn}/-${diag.entityFlickerOut} | ` +
      `jumps: ${diag.positionJumps}/${diag.positionChecks} (worst: ${diag.worstJump.toFixed(1)})`
    );
  };
}

export class NetworkClient {
  private ws: WebSocket | null = null;
  private url: string;
  private worldView: WorldView;
  private reconnectDelay = 1000;
  private maxReconnectDelay = 10000;
  private connected = false;
  playerId: string | null = null;
  prediction: PredictionManager;

  constructor(url: string, worldView: WorldView) {
    this.url = url;
    this.worldView = worldView;
    this.prediction = new PredictionManager();
    this.connect();
  }

  private connect(): void {
    console.log(`Connecting to ${this.url}...`);
    this.ws = new WebSocket(this.url);
    const diag = initDiag();
    let lastTick = 0;

    installDiagReport(diag);

    this.ws.onopen = () => {
      console.log('Connected to server');
      this.connected = true;
      this.reconnectDelay = 1000;
    };

    this.ws.binaryType = 'arraybuffer';

    this.ws.onmessage = (event) => {
      try {
        if (event.data instanceof ArrayBuffer) {
          diag.frames++;
          diag.bytesReceived += event.data.byteLength;
          this.worldView.countFrame();
          const frame = decodeBinaryFrame(event.data);
          if (!frame) {
            diag.dropped++;
            diag.errors.push(`frame ${diag.frames}: decode returned null, size=${event.data.byteLength}, byte0=0x${new Uint8Array(event.data)[0]?.toString(16)}`);
            return;
          }

          if (frame.tick === lastTick) {
            diag.dupes++;
          }
          lastTick = frame.tick;

          // Track frame type stats
          let frameUpdates = 0;
          let frameRemovals = 0;

          // Apply to world view
          if (frame.type === 'full') {
            diag.fullFrames++;
            diag.fullFrameBytes += event.data.byteLength;
            frameUpdates = frame.entities.length;
            this.worldView.applySnapshot({
              type: 'WorldSnapshot',
              tick: frame.tick,
              entities: frame.entities,
              _metrics: frame.metrics,
            });
          } else {
            diag.deltaFrames++;
            diag.deltaFrameBytes += event.data.byteLength;
            frameUpdates = frame.entities.length;
            frameRemovals = frame.removals.length;
            diag.totalUpdates += frameUpdates;
            diag.totalRemovals += frameRemovals;
            if (frameRemovals > 0) {
              console.warn(`[DELTA] tick=${frame.tick} +${frameUpdates} -${frameRemovals} removals: ${frame.removals.slice(0, 3).map(id => id.substring(0, 8)).join(', ')}${frameRemovals > 3 ? '...' : ''}`);
            }
            this.worldView.applyDelta(frame.tick, frame.entities, frame.removals, frame.metrics);
          }
          diag.applied++;

          // Send tick ACK (0x03) back to server for delta baseline tracking
          if (this.ws && this.connected) {
            const ackBuf = new ArrayBuffer(9);
            const ackView = new DataView(ackBuf);
            ackView.setUint8(0, 0x03);
            ackView.setUint32(1, frame.tick & 0xFFFFFFFF, true);
            ackView.setUint32(5, Math.floor(frame.tick / 0x100000000), true);
            this.ws.send(ackBuf);
          }

          // Reconcile local player prediction with server state
          if (this.playerId) {
            const player = this.worldView.entities.get(this.playerId);
            if (player) {
              this.prediction.reconcile(player, player.lastInputSeq);
            }
          }

          // --- Post-apply diagnostics: check what the WorldView actually contains ---
          const currentIds = new Set<string>();
          const serverCounts: Record<number, number> = {};
          let frameJumps = 0;
          let frameFlickerIn = 0;
          let frameFlickerOut = 0;

          for (const [id, entity] of this.worldView.entities) {
            currentIds.add(id);
            serverCounts[entity.authorityServer] = (serverCounts[entity.authorityServer] || 0) + 1;

            // Position jump detection from RENDERED positions (entity.position, not currentPosition)
            const prev = diag.prevPositions.get(id);
            if (prev && entity.hasSecondSnapshot) {
              diag.positionChecks++;
              const dx = entity.position.x - prev[0];
              const dz = entity.position.z - prev[2];
              const dist = Math.sqrt(dx * dx + dz * dz);
              if (dist > 5.0) {
                diag.positionJumps++;
                frameJumps++;
                if (dist > diag.worstJump) diag.worstJump = dist;
                if (diag.positionJumps <= 10) {
                  console.warn(`[JUMP] ${id.substring(0,8)} dist=${dist.toFixed(1)} S${entity.authorityServer} pos=(${entity.position.x.toFixed(1)},${entity.position.z.toFixed(1)}) prev=(${prev[0].toFixed(1)},${prev[2].toFixed(1)}) cur=(${entity.currentPosition.x.toFixed(1)},${entity.currentPosition.z.toFixed(1)}) prv=(${entity.previousPosition.x.toFixed(1)},${entity.previousPosition.z.toFixed(1)})`);
                }
              }
            }
          }

          // Flicker: entities that appeared or disappeared from WorldView
          if (diag.applied > 1) {
            for (const id of currentIds) {
              if (!diag.prevEntityIds.has(id)) { diag.entityFlickerIn++; frameFlickerIn++; }
            }
            for (const id of diag.prevEntityIds) {
              if (!currentIds.has(id)) { diag.entityFlickerOut++; frameFlickerOut++; }
            }
          }

          // Save state
          diag.prevEntityIds = currentIds;
          diag.prevPositions.clear();
          for (const [id, e] of this.worldView.entities) {
            diag.prevPositions.set(id, [e.position.x, e.position.y, e.position.z]);
          }

          // Keep last 200 frame records
          diag.perFrame.push({
            tick: frame.tick,
            type: frame.type,
            entities: this.worldView.entities.size,
            updates: frameUpdates,
            removals: frameRemovals,
            servers: serverCounts,
            jumps: frameJumps,
            flickerIn: frameFlickerIn,
            flickerOut: frameFlickerOut,
          });
          if (diag.perFrame.length > 200) diag.perFrame.shift();

          // Periodic summary every 5 seconds (100 ticks at 20Hz)
          if (frame.tick % 100 === 0) {
            const totalFrames = diag.fullFrames + diag.deltaFrames;
            const deltaRatio = totalFrames > 0
              ? ((diag.deltaFrames / totalFrames) * 100).toFixed(1)
              : '0.0';
            const avgFullSize = diag.fullFrames > 0
              ? Math.round(diag.fullFrameBytes / diag.fullFrames)
              : 0;
            const avgDeltaSize = diag.deltaFrames > 0
              ? Math.round(diag.deltaFrameBytes / diag.deltaFrames)
              : 0;
            console.log(
              `[NET] tick=${frame.tick} entities=${this.worldView.entities.size} ` +
              `frames=${diag.fullFrames}f+${diag.deltaFrames}d ` +
              `delta_ratio=${deltaRatio}% ` +
              `avg_full=${avgFullSize}B avg_delta=${avgDeltaSize}B ` +
              `total=${(diag.bytesReceived / 1024).toFixed(1)}KB ` +
              `flicker=+${diag.entityFlickerIn}/-${diag.entityFlickerOut} ` +
              `jumps=${diag.positionJumps}`
            );
          }

        } else {
          diag.textFrames++;
          const msg = JSON.parse(event.data as string);
          if (msg.type === 'WorldSnapshot') {
            this.worldView.applySnapshot(msg as WorldSnapshot);
          } else if (msg.type === 'PlayerAssigned') {
            this.playerId = msg.player_id;
            this.worldView.localPlayerId = msg.player_id;
            console.log('Assigned player:', msg.player_id);
          } else if (msg.type === 'Reconnect') {
            console.log('Server requesting reconnection to:', msg.url);
            this.url = msg.url;
            if (this.ws) {
              this.ws.close(); // triggers reconnect to new URL via onclose
            }
          }
        }
      } catch (e: any) {
        diag.errors.push(`frame ${diag.frames}: ${e.message}`);
        console.warn('Failed to parse server message:', e);
      }
    };

    this.ws.onclose = () => {
      console.log('Disconnected from server');
      this.connected = false;
      this.playerId = null;
      this.worldView.localPlayerId = null;
      this.scheduleReconnect();
    };

    this.ws.onerror = (err) => {
      console.error('WebSocket error:', err);
    };
  }

  private scheduleReconnect(): void {
    console.log(`Reconnecting in ${this.reconnectDelay}ms...`);
    setTimeout(() => {
      this.reconnectDelay = Math.min(this.reconnectDelay * 2, this.maxReconnectDelay);
      this.connect();
    }, this.reconnectDelay);
  }

  send(msg: object): void {
    if (this.ws && this.connected) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  spawnEntity(x: number, y: number, z: number, entityType: string): void {
    this.send({ type: 'SpawnEntity', position: [x, y, z], entity_type: entityType });
  }

  moveInput(entityId: string, dx: number, dy: number, dz: number, seq: number): void {
    this.send({ type: 'MoveInput', entity_id: entityId, direction: [dx, dy, dz], seq });
  }

  isConnected(): boolean {
    return this.connected;
  }

  getServerUrl(): string {
    return this.url;
  }
}
