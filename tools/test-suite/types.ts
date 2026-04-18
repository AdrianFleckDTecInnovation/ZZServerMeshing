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

export interface PeerHealthSnapshot {
  latest_tick: number;
  staleness_ticks: number;
  packets_received: number;
  chunk_evictions: number;
  /**
   * Most recently observed session id for this peer (WI-136). The server emits
   * a u64 decimal number in JSON, but JavaScript `Number` only has 53 bits of
   * precision. The runner re-parses the raw JSON with a precision-safe regex
   * and delivers this as a decimal string so scenarios can compare exactly.
   */
  session_id: string;
}

export interface ServerMetrics {
  server_id: number;
  /** Local sim-server session id (WI-136). See PeerHealthSnapshot.session_id for precision notes. */
  session_id?: string;
  tick: number;
  entity_count: number;
  shadow_count: number;
  peer_count: number;
  tick_budget_pct: number;
  phases: PhaseMetrics;
  /** Per-peer network health map (WI-103, WI-136). Keyed by peer server_id as a string. */
  peer_health?: Record<string, PeerHealthSnapshot>;
}

// ── Gateway snapshot ──

export interface EntityState {
  id: string;
  position: [number, number, number];
  velocity: [number, number, number];
  entity_type: string;
  authority_server: number;
  last_input_seq: number;
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
  /** WebSocket connection to the server */
  server: WebSocket;
  /** Gateway URL for creating additional connections */
  serverUrl: string;
  /** Base metrics URL (e.g., http://localhost:8101) — use getMetrics() instead */
  metricsBasePort: number;
  /** Number of sim-servers in the cluster */
  serverCount: number;

  /** Fetch current metrics from a sim-server (1-indexed) */
  getMetrics(serverId: number): Promise<ServerMetrics>;
  /** Fetch metrics from all servers */
  getAllMetrics(): Promise<ServerMetrics[]>;
  /** Wait for N ticks to pass (observes via server snapshots) */
  waitForTicks(count: number): Promise<void>;
  /** Wait for a duration in milliseconds */
  sleep(ms: number): Promise<void>;
  /** Spawn entities via the server */
  spawn(count: number, options?: SpawnOptions): Promise<void>;
  /** Spawn `count` entities clustered within `radius` of `center`. */
  spawnCluster(count: number, center: [number, number, number], radius: number, options?: Omit<SpawnOptions, 'position'>): Promise<void>;
  /** Spawn entities at exact positions. */
  spawnAtPositions(positions: [number, number, number][], options?: Omit<SpawnOptions, 'position'>): Promise<void>;
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
  serverUrl: string;
  metricsBasePort: number;
  serverCount: number;
  scenarios?: string[];  // filter by name
}
