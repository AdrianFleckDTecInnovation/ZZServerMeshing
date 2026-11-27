use crate::*;
use bevy_app::App;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

pub const MAX_COMMAND_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandStatus {
    Applied,
    UnknownType,
    Rejected,
    Stale,
    /// The request may have run. It must not be replayed automatically.
    Unconfirmed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoutedCommand {
    pub binding: ControlBinding,
    pub owner: Authority,
    pub sequence: u64,
    pub at_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub kind: u16,
    pub version: u16,
    pub data: Vec<u8>,
}
impl RoutedCommand {
    pub(crate) fn valid(&self) -> bool {
        self.sequence != 0
            && self.kind != 0
            && self.version != 0
            && self.data.len() <= MAX_COMMAND_BYTES
    }
}

#[derive(Component, Serialize, Deserialize)]
struct CommandCursor {
    binding: ControlBinding,
    sequence: u64,
}

type Handler = Arc<dyn Fn(&mut World, Entity, Id, &[u8]) -> CommandStatus + Send + Sync>;
#[derive(Resource, Default)]
struct Commands(BTreeMap<(u16, u16), Handler>);

/// Register a bounded, synchronous game rule for the component on the controlled
/// entity. The returned value replaces that component on success. Returning None
/// rejects the command without changing the component. Target IDs in the payload
/// are untrusted. Register the same types, versions and component codecs on each worker.
pub fn register_session_command<T: Component>(
    app: &mut App,
    kind: u16,
    version: u16,
    rule: impl Fn(&T, Id, &[u8]) -> Option<T> + Send + Sync + 'static,
) -> Result<()> {
    if kind == 0 || version == 0 {
        return Err(invalid("zero command type or version"));
    }
    if !app.world().contains_resource::<Commands>() {
        app.mesh_component::<CommandCursor>(
            0x5a5a_0002,
            1,
            Rules {
                visibility: Visibility::Local,
                persist: true,
                migrate: true,
            },
            Default::default(),
        )?;
        app.init_resource::<Commands>();
    }
    let mut registry = app.world_mut().resource_mut::<Commands>();
    if registry.0.contains_key(&(kind, version)) {
        return Err(invalid("duplicate command type and version"));
    }
    if registry.0.len() >= 256 {
        return Err(Error::Busy("game command registry"));
    }
    registry.0.insert(
        (kind, version),
        Arc::new(move |world, entity, actor, data| {
            let Some(mut state) = world.get_mut::<T>(entity) else {
                return CommandStatus::Rejected;
            };
            let Some(next) = rule(&state, actor, data) else {
                return CommandStatus::Rejected;
            };
            *state = next;
            CommandStatus::Applied
        }),
    );
    Ok(())
}

pub(crate) fn execute(world: &mut World, command: RoutedCommand) -> CommandStatus {
    use CommandStatus::*;
    if !command.valid() {
        return Rejected;
    }
    let Ok(now) = crate::session::control_now() else {
        return Stale;
    };
    if command.at_unix_ms > now.saturating_add(100) {
        return Stale;
    }
    let deadline = command
        .expires_unix_ms
        .min(command.at_unix_ms.saturating_add(500));
    let actor = command.binding.object;
    let Some(controls) = world.get_resource::<PreparedSessionControls>().cloned() else {
        return Stale;
    };
    let Some(&entity) = world.resource::<MeshState>().entities.get(&actor) else {
        return Stale;
    };
    let Some(object) = world.get::<Managed>(entity).copied() else {
        return Stale;
    };
    if world.get::<Simulated>(entity).is_none()
        || world.get::<Paused>(entity).is_some()
        || world.resource::<MeshState>().authority(object.group) != Some(command.owner)
        || world
            .get::<CommandCursor>(entity)
            .is_some_and(|c| c.binding == command.binding && c.sequence >= command.sequence)
    {
        return Stale;
    }
    let handler = world
        .get_resource::<Commands>()
        .and_then(|r| r.0.get(&(command.kind, command.version)))
        .cloned();
    world.resource_scope(|world, mesh: Mut<MeshState>| {
        controls
            .apply_command(&mesh, &object, command.binding, deadline, || {
                let Some(handler) = handler else {
                    return UnknownType;
                };
                let status = handler(world, entity, actor, &command.data);
                world.entity_mut(entity).insert(CommandCursor {
                    binding: command.binding,
                    sequence: command.sequence,
                });
                status
            })
            .unwrap_or(Stale)
    })
}
