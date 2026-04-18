/**
 * Central client configuration.
 *
 * All tunable constants live here so there's one place to look.
 * Values that must match the Rust server are marked with MUST MATCH comments.
 */

// ── Netcode ──

export const NETCODE = {
  /** Initial WebSocket reconnection delay (ms) */
  RECONNECT_DELAY_MS: 1000,
  /** Maximum reconnection delay after exponential backoff (ms) */
  RECONNECT_MAX_DELAY_MS: 10000,
  /** Remove entities not updated by the server for this long (ms).
   *  Set above FULL_SNAPSHOT_INTERVAL (5s) to avoid spurious removals. */
  STALE_ENTITY_MS: 6000,
  /** Number of snapshots to receive before enabling interpolation */
  WARMUP_SNAPSHOTS: 3,
  /** Max interpolation alpha (1.0 = exact lerp, >1.0 allows slight extrapolation) */
  MAX_INTERPOLATION_ALPHA: 1.2,
  /** Exponential moving average weight for snapshot interval measurement (0-1, higher = more responsive) */
  INTERVAL_EMA_NEW: 0.2,
  /** Valid range for measured snapshot intervals (ms) — filters outliers */
  INTERVAL_MIN_MS: 10,
  INTERVAL_MAX_MS: 500,
} as const;

// ── Gameplay ──

export const GAMEPLAY = {
  /** Player movement speed (units/second). Must match PLAYER_SPEED in crates/shared/src/tick.rs */
  PLAYER_SPEED: 5.0,
  /** Maximum pending unacknowledged inputs before dropping oldest */
  MAX_PENDING_INPUTS: 128,
  /** Exponential blend factor for server reconciliation (0-1, lower = smoother but slower correction) */
  CORRECTION_BLEND: 0.2,
} as const;

// ── Rendering ──

export const RENDERING = {
  /** Camera field of view (degrees) */
  CAMERA_FOV: 90,
  /** Camera near clipping plane */
  CAMERA_NEAR: 0.1,
  /** Camera far clipping plane */
  CAMERA_FAR: 20000,
  /** Default camera height above the world */
  CAMERA_HEIGHT: 600,
  /** Camera height when following a player */
  CAMERA_FOLLOW_HEIGHT: 2000,
  /** NPC sphere radius */
  NPC_RADIUS: 12,
  /** NPC sphere geometry segments (width, height) */
  NPC_SEGMENTS: [16, 12] as const,
  /** Player box size (all axes) */
  PLAYER_SIZE: 16,
  /** Material roughness for all entity materials */
  MATERIAL_ROUGHNESS: 0.5,
  /** Material metalness for all entity materials */
  MATERIAL_METALNESS: 0.3,
  /** AoI radius for visual ring. MUST MATCH: crates/shared/src/spatial.rs AOI_RADIUS */
  AOI_RADIUS: 500,
  /** AoI ring visual thickness (pixels each side) */
  AOI_RING_THICKNESS: 3,
  /** AoI ring geometry segments */
  AOI_RING_SEGMENTS: 64,
  /** Ground plane size (should be WORLD_EXTENT * 2). MUST MATCH: crates/shared/src/spatial.rs WORLD_EXTENT */
  GROUND_SIZE: 10000,
  /** Grid subdivision count */
  GRID_DIVISIONS: 20,
  /** Y offset for player entities (stacking visibility) */
  PLAYER_Y_OFFSET: 20,
  /** Y offset multiplier per server ID for NPC stacking */
  NPC_Y_OFFSET_PER_SERVER: 8,
} as const;

// ── Colors ──

export const COLORS = {
  /** Background color */
  SCENE_BACKGROUND: 0x1a1a2e,
  /** Ground plane */
  GROUND: 0x1a2a1a,
  /** World bounds outline */
  BOUNDS: 0x444444,
  /** AoI ring */
  AOI_RING: 0x00ff00,
  /** AoI ring opacity (0-1) */
  AOI_RING_OPACITY: 0.4,
  /** Ambient light color */
  AMBIENT_LIGHT: 0x606080,
  /** Local player entity (bright green) */
  LOCAL_PLAYER: 0x00ff88,
  /** Local player emissive glow */
  LOCAL_PLAYER_EMISSIVE: 0x003311,
  /** Other players (gold) */
  PLAYER: 0xffd700,
  /** Per-server NPC colors (up to 8 servers) */
  SERVERS: [
    0xff4444, // Server 1 - Red
    0x4488ff, // Server 2 - Blue
    0x44ff44, // Server 3 - Green
    0xffaa00, // Server 4 - Orange
    0xff44ff, // Server 5 - Magenta
    0x00ffff, // Server 6 - Cyan
    0xffff44, // Server 7 - Yellow
    0xffffff, // Server 8 - White
  ] as const,
} as const;

// ── Binary Protocol ──

export const PROTOCOL = {
  /** Full snapshot frame type byte */
  FRAME_FULL: 0x01,
  /** Delta frame type byte */
  FRAME_DELTA: 0x02,
  /** Tick ACK frame type byte (client → server) */
  FRAME_ACK: 0x03,
  /** Bytes per entity in binary frames. MUST MATCH: crates/sim-server/src/encoding.rs */
  ENTITY_SIZE: 47,
  /** Full snapshot header size: type(1) + tick(8) + count(4) + metrics(14) */
  FULL_HEADER_SIZE: 27,
  /** Delta header size: type(1) + tick(8) + updates(4) + removals(4) + metrics(14) */
  DELTA_HEADER_SIZE: 31,
} as const;

// ── Diagnostics ──

export const DIAGNOSTICS = {
  /** Maximum frame records kept for diagnostic history */
  MAX_FRAME_HISTORY: 200,
  /** Log a network summary every N ticks (100 ticks = 5s at 20Hz) */
  SUMMARY_TICK_INTERVAL: 100,
  /** Position jump detection threshold (units) */
  JUMP_THRESHOLD: 5.0,
  /** Coordinator server totals polling interval (ms) */
  COORDINATOR_POLL_MS: 2000,
  /** Debug overlay update interval (ms) */
  OVERLAY_UPDATE_MS: 100,
  /** Progress bar character width */
  BAR_WIDTH: 20,
} as const;
