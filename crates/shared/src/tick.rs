/// Simulation tick rate in Hz.
pub const TICK_RATE: u32 = 20;

/// Duration of one tick in milliseconds.
pub const TICK_DURATION_MS: u64 = 1000 / TICK_RATE as u64;

/// Duration of one tick as a `std::time::Duration`.
pub const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(TICK_DURATION_MS);

/// Player movement speed in units per second.
/// Must match `GAMEPLAY.PLAYER_SPEED` in `client/src/config.ts`.
pub const PLAYER_SPEED: f32 = 5.0;

/// Default NPC/non-player entity speed in units per second (used for directed inputs).
pub const NPC_SPEED: f32 = 2.0;
