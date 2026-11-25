#![doc = include_str!("../README.md")]

/// Imports for registering game components and running a persistent worker.
pub mod prelude {
    pub use crate::{
        checkpoint_group, restore_group, ComponentCodec, Id, Managed, MeshApp, MeshPlugin,
        MeshState, Replica, Rules, Simulated, Staged, Store, Visibility,
    };
}
mod bevy_runtime;
mod bulk;
mod store;
mod types;
mod voxel_wire;
pub use voxel_wire::*;
mod handoff;
mod placement;
mod pool;
mod spatial;
pub use pool::*;
mod publish;
pub mod wire;
pub use bevy_runtime::*;
pub use bulk::*;
pub use handoff::*;
pub use placement::*;
pub use publish::*;
pub use spatial::*;
pub use store::{Action, Actions, Edit, Lookup, Snapshot, Store};
pub use types::*;
pub mod remote;
mod timing;
pub use timing::{thread_cpu_cycles, thread_cpu_ns};
pub mod network_mesh;
