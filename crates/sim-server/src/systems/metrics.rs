use crate::components::*;
use crate::resources::*;
use bevy_ecs::prelude::*;

/// Record per-tick metrics. Actual metric collection happens in the game loop.
/// This system exists as a schedule ordering point.
pub fn record_metrics(
    _tick: Res<TickCounter>,
    _auth_query: Query<(), With<Authoritative>>,
    _shadow_query: Query<(), With<Shadow>>,
    _peer_addrs: Res<PeerAddresses>,
) {
    // Metrics are recorded by the game loop after schedule.run().
    // This system exists as a schedule ordering point.
}
