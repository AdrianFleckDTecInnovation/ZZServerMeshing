use super::*;
pub const DRIVE: u32 = 10;
/// Input leases last four simulation ticks. Session and applied sequence migrate
/// exactly, so a retried sample cannot extend an old lease.
#[derive(Component, Clone, Default, Serialize, Deserialize)]
pub struct Drive {
    pub session: Option<Id>,
    pub sequence: u64,
    pub at_ms: u64,
    pub axes: [i16; 3],
    pub remaining: u8,
}
pub struct MovementPlugin;
impl Plugin for MovementPlugin {
    fn build(&self, app: &mut App) {
        app.mesh_component::<Drive>(DRIVE, 1, rules(Visibility::Private), Default::default())
            .unwrap()
            .insert_resource(InputHandler(|w, actor, i, _| {
                let state = w.resource::<MeshState>();
                let entity = state.entities[&actor];
                let revision = state.version(actor);
                let mut drive = w.get_mut::<Drive>(entity).ok_or(mesh::Error::Denied)?;
                if drive.session != Some(i.session)
                    && drive.session.is_some()
                    && (drive.remaining > 0 || i.at_ms <= drive.at_ms)
                {
                    return Err(invalid("another input session is still active"));
                }
                if drive.session != Some(i.session) || i.sequence > drive.sequence {
                    if i.at_ms < drive.at_ms {
                        return Err(invalid("old input timestamp"));
                    }
                    drive.session = Some(i.session);
                    drive.sequence = i.sequence;
                    drive.at_ms = i.at_ms;
                    drive.axes = i.axes;
                    drive.remaining = 4;
                }
                Ok(InputAck {
                    sequence: drive.sequence,
                    revision,
                })
            }))
            .add_systems(Update, advance);
    }
}
fn advance(mut movers: Query<(&mut Position, &mut Drive), With<Simulated>>) {
    for (mut p, mut d) in &mut movers {
        if d.remaining == 0 {
            continue;
        }
        for (x, input) in p.mm.iter_mut().zip(d.axes) {
            *x = x.saturating_add(i64::from(input) * 250 / 32767);
        }
        d.remaining -= 1;
        if d.remaining == 0 {
            d.axes = [0; 3];
        }
    }
}
