mod inbound;
mod input;
mod transfers;
mod drain;
mod replication;
mod snapshot;
mod simulation;
mod coordinator;
mod shadow;
mod metrics;

pub use inbound::{drain_inbound_events, drain_client_events};
pub use input::process_client_inputs;
pub use transfers::{process_transfer_acks, process_transfer_timeouts, process_transfers};
pub use drain::process_drain;
pub use replication::prepare_replication;
pub use snapshot::prepare_client_snapshot;
pub use simulation::{npc_behavior, physics_integration, clamp_positions};
pub use coordinator::process_coord_events;
pub use shadow::shadow_cleanup;
pub use metrics::record_metrics;

/// TICK_DURATION is used by simulation.rs (physics_integration)
pub(crate) const TICK_DURATION: std::time::Duration =
    std::time::Duration::from_millis(shared::tick::TICK_DURATION_MS);
