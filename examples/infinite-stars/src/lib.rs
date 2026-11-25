//! Game-owned schemas and rules. This crate is also an external API consumer.
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use zz_mesh::{self as mesh, *};
pub mod data;
pub mod deployment;
pub mod host;
pub mod movement;
pub mod prediction;

pub const PLAYER: u32 = 1;
pub const POSITION: u32 = 2;
pub const INVENTORY: u32 = 3;
pub const TOOL: u32 = 4;
pub const PLANET: u32 = 5;
pub const CHUNK: u32 = 6;
pub const ASSEMBLY: u32 = 7;
pub const ITEM: u32 = 8;
pub const ORDER: u32 = 9;
pub const STONE: u16 = 1;
pub const COIN: u16 = 2;
#[derive(Component, Clone, Debug, Default, Serialize, Deserialize)]
pub struct Player;
#[derive(Component, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub frame: Id,
    pub mm: [i64; 3],
}
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory(pub BTreeMap<u16, u32>);
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Tool {
    pub durability: u32,
    pub next_work_ms: u64,
    pub ms_per_block: u32,
}
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Planet {
    pub seed: u64,
    pub generator: u32,
}
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub planet: Id,
    pub part: u64,
    pub seed: u64,
    pub generator: u32,
    pub voxel_revision: u64,
    pub removed: BTreeSet<u16>,
    pub placed: BTreeMap<u16, u16>,
    pub origin: Position,
}
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Assembly {
    pub blueprint: DataKey,
    pub pieces: Vec<Id>,
    #[serde(skip)]
    pub local_pieces: Vec<Entity>,
    pub charge_mj: u64,
    pub liquid_ml: u64,
}
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct Dropped {
    pub kind: u16,
    pub count: u32,
}
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct TradeOrder {
    pub buyer: Id,
    pub give: u16,
    pub want: u16,
    pub limit: u32,
}
#[derive(Component)]
pub struct Collider(pub usize);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub kind: u16,
    pub count: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub chunk: Id,
    pub index: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MineBlocks(pub Vec<Target>);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DropItems(pub Vec<Stack>);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pickup(pub Vec<Id>);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Build(pub Vec<Target>);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub counterparty: Id,
    pub quantity: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effects {
    pub inventory: Inventory,
    pub drops: Vec<(Id, Stack)>,
    pub blocks: Vec<(Target, bool)>,
    pub picked: Vec<Id>,
}
macro_rules! action {
    ($a:ty,$id:expr) => {
        impl Action for $a {
            const ID: u32 = $id;
            const VERSION: u16 = 1;
            type Output = Effects;
        }
    };
}
action!(MineBlocks, 101);
action!(DropItems, 102);
action!(Pickup, 103);
action!(Build, 104);
action!(Trade, 105);
impl Inventory {
    pub fn add(&mut self, k: u16, n: u32) -> mesh::Result<()> {
        if n == 0 {
            return Err(invalid("empty item stack"));
        }
        let old = self.0.get(&k).copied().unwrap_or(0);
        self.0.insert(
            k,
            old.checked_add(n)
                .ok_or_else(|| invalid("inventory capacity"))?,
        );
        Ok(())
    }
    pub fn take(&mut self, k: u16, n: u32) -> mesh::Result<()> {
        if n == 0 {
            return Err(invalid("empty item stack"));
        }
        let v = self.0.get_mut(&k).ok_or_else(|| invalid("missing item"))?;
        *v = v
            .checked_sub(n)
            .ok_or_else(|| invalid("insufficient stock"))?;
        if *v == 0 {
            self.0.remove(&k);
        }
        Ok(())
    }
}
impl Position {
    fn reachable(&self, p: &Self) -> bool {
        self.frame == p.frame
            && (0..3).all(|i| self.mm[i].abs_diff(p.mm[i]) <= 8000)
            && (0..3)
                .map(|i| self.mm[i].abs_diff(p.mm[i]).pow(2))
                .sum::<u64>()
                <= 8000 * 8000
    }
}
pub fn block_position(c: &Chunk, index: u16) -> mesh::Result<Position> {
    if index >= 4096 {
        return Err(invalid("voxel outside 16 cubed chunk"));
    }
    let mut p = c.origin.clone();
    for (i, offset) in [index % 16, (index / 16) % 16, index / 256]
        .into_iter()
        .enumerate()
    {
        p.mm[i] = p.mm[i]
            .checked_add(i64::from(offset) * 1000)
            .ok_or_else(|| invalid("voxel coordinate overflow"))?;
    }
    Ok(p)
}
fn player(e: &Edit) -> mesh::Result<(Record, Inventory, Position)> {
    let r = e.read(e.actor)?;
    if r.group != e.group || !r.fields.contains_key(&PLAYER) {
        return Err(mesh::Error::Denied);
    }
    let inv = r.get(INVENTORY, 1)?;
    let pos = r.get(POSITION, 1)?;
    Ok((r, inv, pos))
}
fn finish(
    e: &mut Edit,
    mut r: Record,
    inv: Inventory,
    mut effects: Effects,
) -> mesh::Result<Effects> {
    r.put(INVENTORY, 1, &inv)?;
    e.save(r)?;
    effects.inventory = inv;
    Ok(effects)
}
fn bounded(n: usize) -> mesh::Result<()> {
    if n == 0 || n > 64 {
        Err(invalid("action requires 1..64 targets"))
    } else {
        Ok(())
    }
}
pub fn drop_id(key: ActionKey, index: usize) -> Id {
    key.session.child(key.seq).child(index as u64)
}
fn drop_items(e: &mut Edit, items: DropItems) -> mesh::Result<Effects> {
    bounded(items.0.len())?;
    let (p, mut inv, pos) = player(e)?;
    let mut effect = Effects::default();
    for (i, stack) in items.0.into_iter().enumerate() {
        inv.take(stack.kind, stack.count)?;
        let id = drop_id(e.key, i);
        let mut item = Record::new(id, p.group);
        item.put(
            ITEM,
            1,
            &Dropped {
                kind: stack.kind,
                count: stack.count,
            },
        )?;
        item.put(POSITION, 1, &pos)?;
        e.save(item)?;
        effect.drops.push((id, stack));
    }
    finish(e, p, inv, effect)
}
fn pickup(e: &mut Edit, a: Pickup) -> mesh::Result<Effects> {
    bounded(a.0.len())?;
    let (p, mut inv, pos) = player(e)?;
    let mut effect = Effects::default();
    for id in a.0 {
        let r = e.read(id)?;
        if !pos.reachable(&r.get(POSITION, 1)?) {
            return Err(invalid("pickup out of reach"));
        }
        let item: Dropped = r.get(ITEM, 1)?;
        inv.add(item.kind, item.count)?;
        e.delete(id)?;
        effect.picked.push(id);
    }
    finish(e, p, inv, effect)
}
fn voxels(e: &mut Edit, targets: Vec<Target>, mining: bool) -> mesh::Result<Effects> {
    bounded(targets.len())?;
    let (mut p, mut inv, pos) = player(e)?;
    let mut tool: Tool = p.get(TOOL, 1)?;
    if tool.ms_per_block == 0 || e.now < tool.next_work_ms {
        return Err(invalid("tool cooling down"));
    }
    let count = targets.len() as u32;
    if mining {
        tool.durability = tool
            .durability
            .checked_sub(count)
            .ok_or_else(|| invalid("tool worn out"))?;
    }
    // At most one second of server-authorized accumulated work may be spent at once.
    let work = u64::from(count) * u64::from(tool.ms_per_block);
    let start = tool.next_work_ms.max(e.now.saturating_sub(1000));
    if work > 1000 || start.saturating_add(work) > e.now {
        return Err(invalid("work exceeds elapsed server time"));
    }
    tool.next_work_ms = start + work;
    let mut chunks: BTreeMap<Id, (Record, Chunk)> = BTreeMap::new();
    let mut effect = Effects::default();
    for t in targets {
        if let std::collections::btree_map::Entry::Vacant(entry) = chunks.entry(t.chunk) {
            let r = e.read(t.chunk)?;
            let c = r.get(CHUNK, 1)?;
            entry.insert((r, c));
        }
        let (_, c) = chunks.get_mut(&t.chunk).unwrap();
        if !pos.reachable(&block_position(c, t.index)?) {
            return Err(invalid("voxel out of reach"));
        }
        let solid = c.placed.contains_key(&t.index) || !c.removed.contains(&t.index);
        if mining {
            if !solid {
                return Err(invalid("block already mined"));
            }
            let k = c.placed.remove(&t.index).unwrap_or(STONE);
            c.removed.insert(t.index);
            inv.add(k, 1)?;
        } else {
            if solid {
                return Err(invalid("build position occupied"));
            }
            inv.take(STONE, 1)?;
            c.placed.insert(t.index, STONE);
        }
        effect.blocks.push((t, !mining));
    }
    for (_, (mut r, mut c)) in chunks {
        c.voxel_revision += 1;
        r.put(CHUNK, 1, &c)?;
        e.require(r.group, data::voxel_manifest(&c)?)?;
        e.save(r)?;
    }
    p.put(TOOL, 1, &tool)?;
    finish(e, p, inv, effect)
}
fn trade(e: &mut Edit, t: Trade) -> mesh::Result<Effects> {
    if t.counterparty == e.actor || t.quantity == 0 {
        return Err(invalid("invalid trade"));
    }
    let (p, mut inv, _) = player(e)?;
    let mut other = e.read(t.counterparty)?;
    let mut order: TradeOrder = other.get(ORDER, 1)?;
    if order.buyer != e.actor || t.quantity > order.limit {
        return Err(mesh::Error::Denied);
    }
    let mut stock: Inventory = other.get(INVENTORY, 1)?;
    inv.take(order.want, t.quantity)?;
    stock.take(order.give, t.quantity)?;
    inv.add(order.give, t.quantity)?;
    stock.add(order.want, t.quantity)?;
    order.limit -= t.quantity;
    other.put(ORDER, 1, &order)?;
    other.put(INVENTORY, 1, &stock)?;
    e.save(other)?;
    finish(e, p, inv, Effects::default())
}
fn rules(visibility: Visibility) -> Rules {
    Rules {
        visibility,
        migrate: true,
        persist: true,
    }
}
pub struct InventoryPlugin;
impl Plugin for InventoryPlugin {
    fn build(&self, app: &mut App) {
        app.mesh_component::<Inventory>(
            INVENTORY,
            1,
            rules(Visibility::Private),
            Default::default(),
        )
        .unwrap()
        .mesh_component::<TradeOrder>(ORDER, 1, rules(Visibility::Private), Default::default())
        .unwrap()
        .mesh_component::<Dropped>(ITEM, 1, rules(Visibility::Public), Default::default())
        .unwrap()
        .mesh_action::<DropItems>(drop_items)
        .unwrap()
        .mesh_action::<Pickup>(pickup)
        .unwrap()
        .mesh_action::<Trade>(trade)
        .unwrap();
    }
}
pub struct WorldPlugin;
impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((InventoryPlugin, movement::MovementPlugin));
        app.mesh_component::<Player>(PLAYER, 1, rules(Visibility::Public), Default::default())
            .unwrap()
            .mesh_component::<Position>(
                POSITION,
                1,
                rules(Visibility::Public),
                ComponentCodec {
                    encode: |p, d| {
                        let mut q = p.clone();
                        if d == Detail::Coarse {
                            q.mm = q.mm.map(|v| v / 100 * 100);
                        }
                        encode(&q)
                    },
                    ..Default::default()
                },
            )
            .unwrap()
            .mesh_component::<Tool>(TOOL, 1, rules(Visibility::Private), Default::default())
            .unwrap()
            .mesh_component::<Planet>(PLANET, 1, rules(Visibility::Public), Default::default())
            .unwrap()
            .mesh_component::<Chunk>(
                CHUNK,
                1,
                rules(Visibility::Workers),
                ComponentCodec {
                    references: |c| vec![Reference::Remote(c.planet)],
                    ..Default::default()
                },
            )
            .unwrap()
            .mesh_component::<Assembly>(
                ASSEMBLY,
                1,
                rules(Visibility::Workers),
                ComponentCodec {
                    references: |v| {
                        v.pieces
                            .iter()
                            .copied()
                            .map(Reference::OptionalLocal)
                            .collect()
                    },
                    resolve: |v, map| {
                        v.local_pieces = v
                            .pieces
                            .iter()
                            .filter_map(|id| map.get(id).copied())
                            .collect();
                        Ok(())
                    },
                    ..Default::default()
                },
            )
            .unwrap()
            .mesh_datasets::<Chunk>(|chunk| Ok(vec![data::voxel_manifest(chunk)?]))
            .unwrap()
            .mesh_datasets::<Assembly>(|a| Ok(vec![data::BlueprintProvider.describe(a.blueprint)?]))
            .unwrap()
            .mesh_rebuild(|w, e| {
                if let Some(c) = w.get::<Chunk>(e) {
                    let solid = 4096 - c.removed.len() + c.placed.len();
                    w.entity_mut(e).insert(Collider(solid));
                }
                Ok(())
            })
            .mesh_action::<MineBlocks>(|e, a| voxels(e, a.0, true))
            .unwrap()
            .mesh_action::<Build>(|e, a| voxels(e, a.0, false))
            .unwrap();
        app.mesh_provider(data::BlueprintProvider).unwrap();
        if let Some(db) = app.world().get_resource::<Store>().cloned() {
            app.mesh_provider(data::VoxelProvider::new(db)).unwrap();
        }
        app.insert_resource(SpatialRule(|w, e| {
            let p = w.get::<Position>(e)?;
            let id = w.get::<Managed>(e)?.id;
            let player = w.get::<Player>(e).is_some();
            let extent = if player {
                5000
            } else if w.get::<Chunk>(e).is_some() {
                16000
            } else {
                500
            };
            Some(SpatialProfile {
                bounds: Box3::new(
                    p.frame,
                    p.mm.map(|v| v.saturating_sub(extent)),
                    p.mm.map(|v| v.saturating_add(extent)),
                )
                .ok()?,
                velocity: w
                    .get::<movement::Drive>(e)
                    .map_or([0; 3], |d| d.axes.map(|v| i64::from(v) * 250 / 32767)),
                ahead: 5,
                retention: 1000,
                principal: id,
                active: player,
                needs: BTreeMap::from([
                    (
                        POSITION,
                        Demand {
                            detail: Detail::Exact,
                            period: 1,
                        },
                    ),
                    (
                        CHUNK,
                        Demand {
                            detail: Detail::Exact,
                            period: 4,
                        },
                    ),
                ]),
            })
        }));
    }
}
pub fn world(db: &Store, worker: Worker) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(db.clone())
        .add_plugins((MeshPlugin::persistent(db.clone(), worker), WorldPlugin));
    app
}
pub const ACTOR: Id = Id([1; 16]);
pub const GROUP: Id = Id([2; 16]);
pub const PLANET_ID: Id = Id([3; 16]);
pub fn seed(app: &mut App) -> mesh::Result<Id> {
    let pos = Position {
        frame: PLANET_ID,
        mm: [0; 3],
    };
    let chunk = PLANET_ID.child(0);
    let c = Chunk {
        planet: PLANET_ID,
        part: 0,
        seed: 7,
        generator: 1,
        voxel_revision: 0,
        origin: pos.clone(),
        removed: Default::default(),
        placed: Default::default(),
    };
    app.world_mut().spawn((
        Managed::new(ACTOR, GROUP),
        Player,
        pos.clone(),
        movement::Drive::default(),
        Inventory(BTreeMap::from([(STONE, 10), (COIN, 10)])),
        Tool {
            durability: 1000,
            next_work_ms: 0,
            ms_per_block: 50,
        },
    ));
    app.world_mut().spawn((
        Managed::new(PLANET_ID, GROUP),
        Planet {
            seed: 7,
            generator: 1,
        },
    ));
    app.world_mut()
        .spawn((Managed::new(chunk, GROUP), c.clone(), pos.clone()));
    let plan = data::BlueprintProvider::manifest();
    app.world_mut().spawn((
        Managed::new(PLANET_ID.child(88), GROUP),
        pos,
        Assembly {
            blueprint: plan.key,
            pieces: vec![chunk],
            local_pieces: vec![],
            charge_mj: 500,
            liquid_ml: 30,
        },
    ));
    app.update();
    Ok(chunk)
}
