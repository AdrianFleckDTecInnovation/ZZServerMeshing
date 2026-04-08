import WebSocket from 'ws';

// ── Server-side metrics (from /metrics HTTP endpoint) ──

export interface Percentiles {
  p50: number;
  p95: number;
  p99: number;
}

export interface PhaseMetrics {
  input_drain_ms: Percentiles;
  simulation_ms: Percentiles;
  transfers_ms: Percentiles;
  replication_ms: Percentiles;
  shadow_cleanup_ms: Percentiles;
  total_ms: Percentiles;
}

export interface ServerMetrics {
  server_id: number;
  tick: number;
  entity_count: number;
  shadow_count: number;
  peer_count: number;
  tick_budget_pct: number;
  phases: PhaseMetrics;
}

// ── Gateway snapshot ──

export interface EntityState {
  id: string;
  position: [number, number, number];
  velocity: [number, number, number];
  entity_type: string;
  authority_server: number;
}

export interface WorldSnapshot {
  type: 'WorldSnapshot';
  tick: number;
  entities: EntityState[];
  _metrics?: {
    snapshotsPerSec: number;
    bandwidthKBps: number;
    rebalanceEvents: number;
    serverCount: number;
  };
}

// ── Test context provided to scenarios ──

export interface SpawnOptions {
  position?: [number, number, number];
  entityType?: 'npc' | 'player';
  /** Target a specific server by sending directly to its WS port */
  targetServerWsPort?: number;
}

export interface TestContext {
  /** WebSocket connection to the gateway */
  gateway: WebSocket;
  /** Gateway URL for creating additional connections */
  gatewayUrl: string;
  /** Base metrics URL (e.g., http://localhost:8101) — use getMetrics() instead */
  metricsBasePort: number;
  /** Number of sim-servers in the cluster */
  serverCount: number;

  /** Fetch current metrics from a sim-server (1-indexed) */
  getMetrics(serverId: number): Promise<ServerMetrics>;
  /** Fetch metrics from all servers */
  getAllMetrics(): Promise<ServerMetrics[]>;
  /** Wait for N ticks to pass (observes via gateway snapshots) */
  waitForTicks(count: number): Promise<void>;
  /** Wait for a duration in milliseconds */
  sleep(ms: number): Promise<void>;
  /** Spawn entities via the gateway */
  spawn(count: number, options?: SpawnOptions): Promise<void>;
  /** Get the latest snapshot */
  getLatestSnapshot(): WorldSnapshot | null;
  /** Log a message with scenario prefix */
  log(msg: string): void;
}

// ── Scenario definition ──

export type ScenarioStatus = 'pass' | 'warn' | 'fail';

export interface ScenarioResult {
  name: string;
  status: ScenarioStatus;
  summary: string;
  details: string[];
  durationMs: number;
}

export interface Scenario {
  name: string;
  description: string;
  run(ctx: TestContext): Promise<ScenarioResult>;
}

// ── Runner config ──

export interface RunnerConfig {
  gatewayUrl: string;
  metricsBasePort: number;
  serverCount: number;
  scenarios?: string[];  // filter by name
}
