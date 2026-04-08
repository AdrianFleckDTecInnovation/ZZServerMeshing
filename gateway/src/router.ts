import { WebSocket } from 'ws';
import { Aggregator } from './aggregator.js';

interface ClientInput {
  type: string;
  entity_id?: string;
  [key: string]: unknown;
}

/**
 * Routes client inputs to the correct authority server.
 */
export class Router {
  routeInput(
    msg: ClientInput,
    serverConnections: Map<number, WebSocket>,
    aggregator: Aggregator,
  ) {
    if (msg.type === 'MoveInput' && msg.entity_id) {
      // Route to the server that owns this entity
      const authServer = aggregator.getAuthorityServer(msg.entity_id as string);
      if (authServer !== null) {
        const ws = serverConnections.get(authServer);
        if (ws && ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify(msg));
          return;
        }
      }
      // Fallback: broadcast to all servers
      this.broadcastToAll(msg, serverConnections);
    } else if (msg.type === 'SpawnEntity') {
      // Round-robin spawn across servers
      const serverIds = [...serverConnections.keys()].filter(id => id > 0);
      if (serverIds.length === 0) return;
      const target = serverIds[Math.floor(Math.random() * serverIds.length)];
      const ws = serverConnections.get(target);
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(msg));
      }
    } else {
      this.broadcastToAll(msg, serverConnections);
    }
  }

  private broadcastToAll(msg: unknown, connections: Map<number, WebSocket>) {
    const json = JSON.stringify(msg);
    for (const [id, ws] of connections) {
      if (id > 0 && ws.readyState === WebSocket.OPEN) {
        ws.send(json);
      }
    }
  }
}
