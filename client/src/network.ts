import { WorldView, WorldSnapshot } from './world-view';
import { decodeBinaryFrame } from './binary-protocol';
import { PredictionManager } from './prediction';
import { NETCODE, PROTOCOL, DIAGNOSTICS } from './config';

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
  private url: string = '';
  private coordinatorUrl: string;
  private worldView: WorldView;
  private reconnectDelay: number = NETCODE.RECONNECT_DELAY_MS;
  private connected = false;
  /** If set, the next scheduleReconnect uses this URL and skips /assign.
   *  Populated by the server-directed `Reconnect { url }` handler; consumed
   *  exactly once by scheduleReconnect. Ensures Batch 4 player migrations
   *  override the coordinator's /assign choice for one reconnect cycle. */
  private serverDirectedUrl: string | null = null;
  /** WI-204: if set, sent as the first message on the next successful
   *  WebSocket open. Populated by the Reconnect handler alongside
   *  serverDirectedUrl so the target sim-server can adopt the pre-existing
   *  player entity instead of spawning a new one. Cleared on send. */
  private pendingClaim: string | null = null;
  playerId: string | null = null;
  prediction: PredictionManager;

  constructor(coordinatorUrl: string, worldView: WorldView) {
    this.coordinatorUrl = coordinatorUrl;
    this.worldView = worldView;
    this.prediction = new PredictionManager();
    void this.init();
  }

  /** Initial bootstrap: fetch /assign, then connect. If /assign fails,
   *  connect() is still called with an empty URL; the resulting WebSocket
   *  error bounces through scheduleReconnect() which retries /assign on
   *  exponential backoff. Don't hard-fail the page load on a blip. */
  private async init(): Promise<void> {
    const fresh = await this.fetchAssignment();
    if (fresh) {
      console.log(`Assigned to server: ${fresh}`);
      this.url = fresh;
    } else {
      console.warn('Initial /assign failed; will retry via reconnect backoff');
    }
    this.connect();
  }

  /** Fetch a fresh server assignment from the coordinator. Returns null on
   *  any failure (network error, non-2xx, missing field). Caller decides
   *  whether to fall back to the last-known URL. */
  private async fetchAssignment(): Promise<string | null> {
    const assignUrl = this.coordinatorUrl
      ? `${this.coordinatorUrl}/assign`
      : '/assign';
    try {
      const resp = await fetch(assignUrl);
      if (!resp.ok) {
        console.warn(`/assign returned HTTP ${resp.status}`);
        return null;
      }
      const data = await resp.json();
      return data.ws_url ?? null;
    } catch (e) {
      console.warn(`/assign fetch failed: ${e}`);
      return null;
    }
  }

  private connect(): void {
    // Guard: if /assign failed on bootstrap and we have no URL, don't try
    // to construct an empty-URL WebSocket (throws SyntaxError). Just
    // schedule another reconnect to retry /assign on backoff.
    if (!this.url) {
      console.warn('No server URL available; scheduling reconnect to retry /assign');
      this.scheduleReconnect();
      return;
    }
    console.log(`Connecting to ${this.url}...`);
    this.ws = new WebSocket(this.url);
    const diag = initDiag();
    let lastTick = 0;

    installDiagReport(diag);

    this.ws.onopen = () => {
      console.log('Connected to server');
      this.connected = true;
      this.reconnectDelay = NETCODE.RECONNECT_DELAY_MS;
      // WI-204: if this connection is the target-side of a server-directed
      // Reconnect, send ClaimPlayer as the FIRST message so the server
      // adopts our existing entity by UUID instead of spawning a new one.
      // Server's PredictionManager state (input seq counter, pending
      // inputs) is preserved across migration — the target server's
      // AuthorityTransfer.full_state carries over last_input_seq so
      // reconcile keeps working without reset.
      if (this.pendingClaim) {
        const pid = this.pendingClaim;
        console.log(`Claiming player ${pid.substring(0, 8)}...`);
        this.send({ type: 'ClaimPlayer', player_id: pid });
      }
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
            this.worldView.applyDelta(frame.tick, frame.entities, frame.removals);
          }
          diag.applied++;

          // Send tick ACK back to server for delta baseline tracking
          if (this.ws && this.connected) {
            const ackBuf = new ArrayBuffer(9);
            const ackView = new DataView(ackBuf);
            ackView.setUint8(0, PROTOCOL.FRAME_ACK);
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
              if (dist > DIAGNOSTICS.JUMP_THRESHOLD) {
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
          if (diag.perFrame.length > DIAGNOSTICS.MAX_FRAME_HISTORY) diag.perFrame.shift();

          // Periodic summary
          if (frame.tick % DIAGNOSTICS.SUMMARY_TICK_INTERVAL === 0) {
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
            this.pendingClaim = null;
            this.playerId = msg.player_id;
            this.worldView.localPlayerId = msg.player_id;
            console.log('Assigned player:', msg.player_id);
          } else if (msg.type === 'Reconnect') {
            console.log('Server requesting reconnection to:', msg.url);
            // Mark this as a server-directed URL so scheduleReconnect will
            // honour it instead of refetching /assign (which would steer
            // the client away from the rebalance target — Batch 4).
            this.serverDirectedUrl = msg.url;
            // WI-204: snapshot the current player_id BEFORE close (the
            // onclose handler nulls it out). Sent as ClaimPlayer on the
            // next WS open so the target server adopts this exact entity
            // rather than spawning a new one.
            if (this.playerId) {
              this.pendingClaim = this.playerId;
            }
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
    setTimeout(async () => {
      this.reconnectDelay = Math.min(this.reconnectDelay * 2, NETCODE.RECONNECT_MAX_DELAY_MS);

      if (this.pendingClaim && !this.serverDirectedUrl) {
        this.connect();
        return;
      }
      if (this.serverDirectedUrl) {
        // Server-directed URL takes precedence over /assign for exactly one
        // reconnect cycle. Used by Batch 4 player migration.
        const directed = this.serverDirectedUrl;
        this.serverDirectedUrl = null;
        this.url = directed;
        console.log(`Reconnecting to server-directed URL: ${directed}`);
      } else {
        const fresh = await this.fetchAssignment();
        if (fresh && fresh !== this.url) {
          console.log(`/assign returned new URL: ${fresh} (was ${this.url})`);
          this.url = fresh;
        } else if (fresh) {
          // /assign returned the same URL as last time — coordinator thinks
          // this server is still healthy. Retry it.
          console.log(`/assign returned same URL: ${fresh}`);
        } else {
          console.warn(`/assign unavailable, retrying last-known URL ${this.url}`);
        }
      }
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
