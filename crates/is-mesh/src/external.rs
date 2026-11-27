//! A game hands the request to its async service. Confirmed full projections
//! can be retried at the current owner until it acknowledges their revision.
use crate::*;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

impl Rules {
    /// The source of truth rehydrates this component after restore or handoff.
    pub fn external(visibility: Visibility) -> Self {
        Self {
            visibility,
            persist: false,
            migrate: false,
        }
    }
}
#[derive(Component, Serialize, Deserialize)]
pub struct ExternalValue<T> {
    pub revision: u64,
    pub value: T,
}
#[derive(Debug)]
pub struct ExternalRequest<T> {
    pub correlation: Id,
    pub actor: Id,
    pub principal: PrincipalId,
    pub session: SessionId,
    pub authority: Authority,
    pub payload: T,
}
/// Producer handle for a bounded, nonblocking hand-off to an external service.
pub struct ExternalActions<T>(SyncSender<ExternalRequest<T>>);
impl<T> ExternalActions<T> {
    pub fn bounded(capacity: std::num::NonZeroUsize) -> (Self, Receiver<ExternalRequest<T>>) {
        let (tx, rx) = sync_channel(capacity.get());
        (Self(tx), rx)
    }
    pub fn submit(
        &self,
        world: &World,
        binding: ControlBinding,
        correlation: Id,
        payload: T,
    ) -> Result<()> {
        let (entity, authority) = current_writer(world, binding.object)?;
        let state = world
            .get_resource::<SessionDirectory>()
            .ok_or_else(|| invalid("missing MeshSessionPlugin"))?
            .for_worker(
                authority.worker,
                &[(
                    binding.object,
                    world.get::<Managed>(entity).unwrap().group,
                    authority.generation,
                )],
            )?;
        if state[0].is_none_or(|s| s.binding != binding) {
            return Err(Error::Denied);
        }
        let req = ExternalRequest {
            correlation,
            actor: binding.object,
            principal: binding.principal,
            session: binding.session,
            authority,
            payload,
        };
        self.0.try_send(req).map_err(|e| match e {
            TrySendError::Full(_) => Error::Busy("external action queue"),
            TrySendError::Disconnected(_) => invalid("external service disconnected"),
        })
    }
}
fn current_writer(world: &World, object: Id) -> Result<(Entity, Authority)> {
    let state = world.resource::<MeshState>();
    let entity = *state.entities.get(&object).ok_or(Error::Fenced)?;
    let managed = world.get::<Managed>(entity).ok_or(Error::Fenced)?;
    let authority = state.authority(managed.group).ok_or(Error::Fenced)?;
    if world.get::<Simulated>(entity).is_none() {
        return Err(Error::Fenced);
    }
    if let Some(store) = world.get_resource::<Store>() {
        store.check_authority(managed.group, authority, true)?;
    }
    Ok((entity, authority))
}
/// A new owner must first read a full projection from the authoritative external
/// source. Delayed results cannot initialize the value from an old checkpoint.
pub fn hydrate_external<T: Send + Sync + 'static>(
    world: &mut World,
    object: Id,
    revision: u64,
    value: T,
) -> Result<bool> {
    let (e, _) = current_writer(world, object)?;
    if world
        .get::<ExternalValue<T>>(e)
        .is_some_and(|s| s.revision >= revision)
    {
        return Ok(false);
    }
    world
        .entity_mut(e)
        .insert(ExternalValue { revision, value });
    Ok(true)
}
/// Return true when installed, false for an already applied or superseded result.
/// On Fenced resolve the same object again. On Missing hydrate from the service.
pub fn apply_external<T: Send + Sync + 'static>(
    world: &mut World,
    object: Id,
    revision: u64,
    value: T,
) -> Result<bool> {
    let (e, _) = current_writer(world, object)?;
    let mut current = world
        .get_mut::<ExternalValue<T>>(e)
        .ok_or_else(|| Error::Missing("external source snapshot".into()))?;
    if revision <= current.revision {
        return Ok(false);
    }
    *current = ExternalValue { revision, value };
    Ok(true)
}
