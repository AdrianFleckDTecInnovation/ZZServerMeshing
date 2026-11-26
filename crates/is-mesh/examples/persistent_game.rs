use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use is_mesh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize)]
struct Energy(u32);

fn charge(mut reactors: Query<&mut Energy, With<Simulated>>) {
    for mut energy in &mut reactors {
        energy.0 = energy.0.saturating_add(1);
    }
}

fn main() -> is_mesh::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| is_mesh::invalid("pass the world.sqlite path"))?;
    let store = Store::open(path)?;
    let worker = store.start_worker(1)?;
    let mut app = App::new();
    app.add_plugins(MeshPlugin::persistent(store.clone(), worker));
    app.mesh_component::<Energy>(
        1,
        1,
        Rules {
            visibility: Visibility::Public,
            migrate: true,
            persist: true,
        },
        ComponentCodec::default(),
    )?;
    app.add_systems(Update, charge);

    let reactor = Id::numbered(1);
    let group = Id::numbered(2);
    for owned in store.owned(worker.slot)? {
        restore_group(app.world_mut(), owned)?;
    }
    match store.lookup(reactor)? {
        is_mesh::Lookup::Unknown => {
            app.world_mut()
                .spawn((Managed::new(reactor, group), Energy(0)));
        }
        is_mesh::Lookup::Present(_) => {}
        is_mesh::Lookup::Deleted => return Err(is_mesh::invalid("reactor was retired")),
    }
    app.update();
    checkpoint_group(app.world_mut(), group)?;
    // Read back the durable record, as the next process will see it.
    let is_mesh::Lookup::Present(record) = store.lookup(reactor)? else {
        return Err(is_mesh::invalid("reactor checkpoint missing"));
    };
    let energy: Energy = record.get(1, 1)?;
    println!("reactor={reactor} energy={}", energy.0);
    Ok(())
}
