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

/// Re-export for submodule use (simulation.rs physics_integration).
pub(crate) use shared::tick::TICK_DURATION;

use shared::protocol::PeerMessage;
use std::net::SocketAddr;

/// Serialize a PeerMessage and push to an outbound queue.
/// Used by input.rs, transfers.rs, and drain.rs to avoid repeating the
/// serialize_versioned + push pattern.
pub(super) fn enqueue_outbound(
    messages: &mut Vec<(SocketAddr, Vec<u8>)>,
    addr: SocketAddr,
    msg: &PeerMessage,
) {
    if let Some(bytes) = crate::game_loop::serialize_versioned(msg) {
        messages.push((addr, bytes));
    }
}
