import { WebSocketServer, WebSocket } from 'ws';
import { Aggregator } from './aggregator.js';
import { Router } from './router.js';

const COORDINATOR_URL = process.env.COORDINATOR_URL || 'ws://127.0.0.1:7870';
const GATEWAY_PORT = parseInt(process.env.GATEWAY_PORT || '4322');

interface PeerInfo {
  server_id: number;
  udp_addr: string;
  ws_addr: string;
}

interface CoordMessage {
  type: string;
  peers?: PeerInfo[];
  // Phase 3: rebalance directives will come through here too
}

class Gateway {
  private aggregator = new Aggregator();
  private router = new Router();
  private coordWs: WebSocket | null = null;
  private serverConnections = new Map<number, WebSocket>();
  private clientWss: WebSocketServer;
  private clients = new Set<WebSocket>();
  /** Map from client WebSocket to their player entity ID */
  private clientPlayers = new Map<WebSocket, string>();

  constructor() {
    this.clientWss = new WebSocketServer({ port: GATEWAY_PORT });
    console.log(`Gateway listening for clients on ws://localhost:${GATEWAY_PORT}`);

    this.clientWss.on('connection', (ws) => {
      this.clients.add(ws);
      console.log(`Client connected (${this.clients.size} total)`);

      // Auto-spawn a player for this client on the least-loaded server
      this.assignPlayer(ws);

      ws.on('message', (data) => {
        try {
          const msg = JSON.parse(data.toString());
          this.router.routeInput(msg, this.serverConnections, this.aggregator);
        } catch (e) {
          console.warn('Bad client message:', e);
        }
      });

      ws.on('close', () => {
        // Clean up player entity
        const playerId = this.clientPlayers.get(ws);
        if (playerId) {
          this.removePlayerEntity(playerId);
          this.clientPlayers.delete(ws);
        }
        this.clients.delete(ws);
        console.log(`Client disconnected (${this.clients.size} total)`);
      });
    });

    // Broadcast aggregated state to clients at tick rate
    setInterval(() => {
      const snapshot = this.aggregator.getSnapshot();
      if (!snapshot) return;
      const metrics = this.aggregator.updateMetrics();
      const json = JSON.stringify({ ...snapshot, _metrics: metrics });
      for (const client of this.clients) {
        if (client.readyState === WebSocket.OPEN) {
          client.send(json);
        }
      }
    }, 50); // 20Hz

    this.connectCoordinator();
  }

  /** Spawn a player entity on the least-loaded server and notify the client. */
  private assignPlayer(clientWs: WebSocket) {
    const serverIds = [...this.serverConnections.keys()].filter(id => id > 0);
    if (serverIds.length === 0) {
      // No servers yet — retry after a delay
      setTimeout(() => this.assignPlayer(clientWs), 1000);
      return;
    }

    // Pick least-loaded server
    const targetId = this.aggregator.getLeastLoadedServer(serverIds);
    const serverWs = this.serverConnections.get(targetId);
    if (!serverWs || serverWs.readyState !== WebSocket.OPEN) return;

    // Send spawn request to the server — the server will create it and include
    // it in the next WorldSnapshot. We generate the UUID client-side so we can
    // immediately tell the client their player ID.
    const playerId = crypto.randomUUID();
    serverWs.send(JSON.stringify({
      type: 'SpawnPlayer',
      player_id: playerId,
      position: [0, 0, 0],
    }));

    this.clientPlayers.set(clientWs, playerId);

    // Tell the client their player ID
    clientWs.send(JSON.stringify({
      type: 'PlayerAssigned',
      player_id: playerId,
    }));

    console.log(`Assigned player ${playerId.substring(0, 8)}... to S${targetId}`);
  }

  /** Tell the authority server to remove a player entity. */
  private removePlayerEntity(playerId: string) {
    const authServer = this.aggregator.getAuthorityServer(playerId);
    if (authServer !== null) {
      const ws = this.serverConnections.get(authServer);
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({
          type: 'RemoveEntity',
          entity_id: playerId,
        }));
      }
    }
    console.log(`Removed player ${playerId.substring(0, 8)}...`);
  }

  private connectCoordinator() {
    console.log(`Connecting to coordinator at ${COORDINATOR_URL}...`);
    this.coordWs = new WebSocket(COORDINATOR_URL);

    this.coordWs.on('open', () => {
      console.log('Connected to coordinator');
      this.coordWs!.send(JSON.stringify({
        type: 'Register',
        server_id: 0,
        udp_port: 0,
        ws_port: GATEWAY_PORT,
      }));
    });

    this.coordWs.on('message', (data) => {
      try {
        const msg: CoordMessage = JSON.parse(data.toString());
        if (msg.type === 'PeerList' && msg.peers) {
          this.handlePeerList(msg.peers);
        }
      } catch (e) {
        console.warn('Bad coordinator message:', e);
      }
    });

    this.coordWs.on('close', () => {
      console.log('Lost coordinator connection, retrying in 2s...');
      setTimeout(() => this.connectCoordinator(), 2000);
    });

    this.coordWs.on('error', () => {});
  }

  private handlePeerList(peers: PeerInfo[]) {
    const simPeers = peers.filter(p => p.server_id > 0);
    console.log(`Peer list: ${simPeers.map(p => `S${p.server_id}(${p.ws_addr})`).join(', ')}`);

    for (const peer of simPeers) {
      if (!this.serverConnections.has(peer.server_id)) {
        this.connectToServer(peer);
      }
    }

    const activeSids = new Set(simPeers.map(p => p.server_id));
    for (const [sid, ws] of this.serverConnections) {
      if (!activeSids.has(sid)) {
        ws.close();
        this.serverConnections.delete(sid);
        this.aggregator.removeServer(sid);
      }
    }
  }

  private connectToServer(peer: PeerInfo) {
    const url = `ws://${peer.ws_addr}`;
    console.log(`Connecting to sim-server S${peer.server_id} at ${url}...`);
    const ws = new WebSocket(url);

    ws.on('open', () => {
      console.log(`Connected to S${peer.server_id}`);
      this.serverConnections.set(peer.server_id, ws);
    });

    ws.on('message', (data) => {
      try {
        const raw = data.toString();
        const msg = JSON.parse(raw);
        if (msg.type === 'WorldSnapshot') {
          this.aggregator.onServerSnapshot(peer.server_id, msg, raw.length);
        }
      } catch (e) {}
    });

    ws.on('close', () => {
      console.log(`Lost connection to S${peer.server_id}`);
      this.serverConnections.delete(peer.server_id);
      this.aggregator.removeServer(peer.server_id);
    });

    ws.on('error', () => {});
  }
}

new Gateway();
