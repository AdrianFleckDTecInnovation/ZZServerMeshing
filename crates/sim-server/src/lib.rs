//! Original browser demo and its synchronously stepable worker.
pub mod components;
pub mod conversion;
pub mod coordinator_client;
pub mod cpu;
pub mod delta;
pub mod encoding;
pub mod game_loop;
pub mod metrics;
pub mod metrics_server;
pub mod network;
pub mod peer_bounds;
mod peer_interest;
pub mod receiver;
pub mod resources;
pub mod schedule;
pub mod systems;

pub use game_loop::DemoWorker;
