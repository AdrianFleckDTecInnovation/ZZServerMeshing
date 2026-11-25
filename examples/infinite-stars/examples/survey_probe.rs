//! A new game type, bulk dependency and action implemented by an application.
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use zz_mesh::*;

#[derive(Component, Serialize, Deserialize)]
struct Probe {
    charge: u16,
    calibration: DataKey,
}

struct Calibration;
impl DataProvider for Calibration {
    fn schema(&self) -> u32 {
        700
    }
    fn describe(&self, key: DataKey) -> Result<Manifest> {
        if key
            != (DataKey {
                provider: 700,
                object: Id::numbered(72),
                part: 0,
            })
        {
            return Err(invalid("unknown calibration"));
        }
        let bytes = b"probe calibration v1";
        let h = digest(bytes);
        Ok(Manifest {
            key,
            revision: 1,
            basis: Basis::Artifact(h),
            hash: h,
            length: bytes.len() as u64,
            dependencies: vec![],
        })
    }
    fn snapshot(&self, m: &Manifest) -> Result<Vec<u8>> {
        if self.describe(m.key)? != *m {
            return Err(invalid("calibration version"));
        }
        Ok(b"probe calibration v1".to_vec())
    }
}
#[derive(Serialize, Deserialize)]
struct Scan(Vec<u8>);
impl Action for Scan {
    const ID: u32 = 702;
    const VERSION: u16 = 1;
    type Output = u16;
}
struct SurveyPlugin;
impl Plugin for SurveyPlugin {
    fn build(&self, app: &mut App) {
        app.mesh_provider(Calibration)
            .unwrap()
            .mesh_component::<Probe>(
                701,
                1,
                Rules {
                    visibility: Visibility::Private,
                    migrate: true,
                    persist: true,
                },
                Default::default(),
            )
            .unwrap()
            .mesh_datasets::<Probe>(|p| Ok(vec![Calibration.describe(p.calibration)?]))
            .unwrap()
            .mesh_action::<Scan>(|edit, channels| {
                if channels.0.is_empty() || channels.0.len() > 8 {
                    return Err(invalid("scan requires one to eight channels"));
                }
                let mut r = edit.read(edit.actor)?;
                if r.group != edit.group {
                    return Err(Error::Denied);
                }
                let mut p: Probe = r.get(701, 1)?;
                p.charge = p
                    .charge
                    .checked_sub(channels.0.len() as u16)
                    .ok_or_else(|| invalid("probe discharged"))?;
                let remaining = p.charge;
                r.put(701, 1, &p)?;
                edit.save(r)?;
                Ok(remaining)
            })
            .unwrap();
    }
}
fn main() -> Result<()> {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, MeshPlugin::default(), SurveyPlugin));
    let managed = Managed::default();
    let calibration = DataKey {
        provider: 700,
        object: Id::numbered(72),
        part: 0,
    };
    app.world_mut().spawn((
        managed,
        Probe {
            charge: 10,
            calibration,
        },
    ));
    app.update();
    let key = ActionKey {
        session: Id::new(),
        seq: 1,
    };
    let scan = Intent::typed(key, managed.group, &Scan(vec![2, 3, 5]))?;
    let reply = commit_batch(app.world_mut(), managed.id, &[scan], 0)?;
    let Reply::Decided(Receipt {
        outcome: Outcome::Committed(bytes),
        ..
    }) = &reply[0]
    else {
        return Err(invalid("scan not committed"));
    };
    let remaining: u16 = decode(bytes)?;
    assert_eq!(remaining, 7);
    assert_eq!(
        app.world()
            .resource::<Store>()
            .snapshot(managed.group)?
            .required
            .len(),
        1
    );
    println!(
        "Probe {} scanned three channels, {} charge left.",
        managed.id, remaining
    );
    Ok(())
}
