import { WorldView, WorldSnapshot } from './world-view';

export class NetworkClient {
  private ws: WebSocket | null = null;
  private url: string;
  private worldView: WorldView;
  private reconnectDelay = 1000;
  private maxReconnectDelay = 10000;
  private connected = false;
  /** Our player entity ID, assigned by the gateway. */
  playerId: string | null = null;

  constructor(url: string, worldView: WorldView) {
    this.url = url;
    this.worldView = worldView;
    this.connect();
  }

  private connect(): void {
    console.log(`Connecting to ${this.url}...`);
    this.ws = new WebSocket(this.url);

    this.ws.onopen = () => {
      console.log('Connected to server');
      this.connected = true;
      this.reconnectDelay = 1000;
    };

    this.ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        if (msg.type === 'WorldSnapshot') {
          this.worldView.applySnapshot(msg as WorldSnapshot);
        } else if (msg.type === 'PlayerAssigned') {
          this.playerId = msg.player_id;
          this.worldView.localPlayerId = msg.player_id;
          console.log('Assigned player:', msg.player_id);
        }
      } catch (e) {
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
    this.send({
      type: 'SpawnEntity',
      position: [x, y, z],
      entity_type: entityType,
    });
  }

  moveInput(entityId: string, dx: number, dy: number, dz: number): void {
    this.send({
      type: 'MoveInput',
      entity_id: entityId,
      direction: [dx, dy, dz],
    });
  }

  isConnected(): boolean {
    return this.connected;
  }
}
