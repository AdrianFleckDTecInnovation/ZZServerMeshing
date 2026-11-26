//! Persistente Worker für Bevy 0.15.
//!
//! Öffne den [`Store`] und starte eine Inkarnation mit [`Store::start_worker`].
//! Installiere [`MeshPlugin::persistent`] und registriere Spielkomponenten über
//! [`MeshApp::mesh_component`]. [`Managed`] verbindet Objekt und Übergabegruppe.
//! Schreibende Spielsysteme verwenden den Filter `With<Simulated>`.
//!
//! Lade beim Neustart jede zugewiesene Gruppe mit [`restore_group`]. Verwende
//! [`checkpoint_group`] für ausstehende Komponenten und [`commit_batch`] für
//! dauerhafte Aktionen. [`Directory`] verwaltet räumliche Interessen.
//! [`Pool`] betreibt mehrere Worker im Prozess, [`remote`] und [`wire`] verbinden
//! getrennte Prozesse.
//!
//! ## Vollständiges Beispiel
//!
//! Dieses Programm erwartet den Pfad zu einer Weltdatei. Jeder Start erhöht die
//! gespeicherte Energie des Reaktors um eins.
#![doc = concat!("\n```no_run\n", include_str!("../examples/persistent_game.rs"), "\n```")]

/// Imports for registering game components and running a persistent worker.
pub mod prelude {
    pub use crate::{
        checkpoint_group, restore_group, ComponentCodec, Id, Managed, MeshApp, MeshPlugin,
        MeshState, Replica, Rules, Simulated, Staged, Store, Visibility,
    };
}
pub mod authority_store;
mod bevy_runtime;
mod bulk;
mod store;
mod types;
mod voxel_wire;
pub use voxel_wire::*;
mod handoff;
mod octree;
mod placement;
mod pool;
mod regions;
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
