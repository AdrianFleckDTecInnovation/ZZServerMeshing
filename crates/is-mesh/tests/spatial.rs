use std::collections::{BTreeMap, BTreeSet};
use is_mesh::*;
fn w(n: u32) -> Worker {
    Worker {
        slot: n,
        incarnation: 1,
    }
}

#[test]
fn cached_islands_follow_contact_activity_deletion_and_frame_changes() {
    let mut d = Directory::default();
    let frame = Id::numbered(910);
    let bounds = |x| Box3::new(frame, [x, 0, 0], [x + 10, 10, 10]).unwrap();
    let mut a = area(1, 1, bounds(-10));
    let mut b = area(2, 1, bounds(1));
    d.register_area(a.clone()).unwrap();
    d.register_area(b.clone()).unwrap();
    assert_eq!(d.island_groups().len(), 2);
    b.bounds = bounds(0);
    d.register_area(b.clone()).unwrap();
    assert_eq!(d.island_groups().len(), 1);
    b.bounds = bounds(1);
    b.velocity = [-10, 0, 0];
    b.ahead = 5;
    d.register_area(b.clone()).unwrap();
    assert_eq!(d.island_groups().len(), 2);
    a.activity = false;
    d.register_area(a.clone()).unwrap();
    assert_eq!(d.island_groups(), &[vec![b.id]]);
    a.activity = true;
    a.bounds.frame = Id::numbered(999);
    d.register_area(a.clone()).unwrap();
    assert_eq!(d.island_groups().len(), 2);
    d.remove_area(b.id);
    assert_eq!(d.island_groups(), &[vec![a.id]]);
}

#[test]
fn moving_faces_match_exhaustive_hysteresis_through_repeated_crossings() {
    let mut directory = Directory::default();
    let mut areas: Vec<_> = (0..12)
        .map(|i| area(i + 1, i as u32 + 1, bounds(i as i64 * 4, 0, 0, 5)))
        .collect();
    let mut objects: Vec<_> = (0..12)
        .map(|i| obj(i + 50, bounds(i as i64 * 4, 0, 0, 2)))
        .collect();
    let mut memberships = [[false; 12]; 12];
    for a in &areas {
        directory.register_area(a.clone()).unwrap();
    }
    for p in &objects {
        directory.upsert(*p).unwrap();
    }
    let mut seed = 743u64;
    for step in 0..500 {
        for (i, p) in objects.iter().enumerate() {
            let actual: BTreeSet<_> = directory.subscribers(p.id).map(|(r, _)| *r).collect();
            for (j, a) in areas.iter().enumerate() {
                let pad = if memberships[i][j] { a.hysteresis } else { 0 };
                memberships[i][j] = a
                    .bounds
                    .sweep(a.velocity, a.ahead, pad)
                    .intersects(p.bounds.sweep(p.velocity, p.ahead, 0));
                assert_eq!(
                    actual.contains(&a.to),
                    memberships[i][j],
                    "step {step} object {i} area {j}"
                );
            }
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = ((seed >> 32) % 12) as usize;
        let x = ((seed >> 48) % 58) as i64 - 8;
        let frame = Id::numbered(if step % 7 == 0 { 2 } else { 1 });
        if step % 3 == 0 {
            let a = &mut areas[j];
            a.bounds = bounds(x, j as i64, 0, 5);
            a.bounds.frame = frame;
            a.velocity = [((seed >> 20) % 5) as i64 - 2, 0, 0];
            a.ahead = 3;
            directory.register_area(a.clone()).unwrap();
        } else {
            let p = &mut objects[j];
            p.bounds = bounds(x, j as i64, 0, 2);
            p.bounds.frame = frame;
            directory.upsert(*p).unwrap();
        }
        directory.changes();
    }
}
fn bounds(x: i64, y: i64, z: i64, r: i64) -> Box3 {
    let p = [x, y, z];
    Box3::new(Id::numbered(1), p.map(|v| v - r), p.map(|v| v + r)).unwrap()
}
fn area(id: u128, worker: u32, b: Box3) -> Area {
    Area {
        id: Id::numbered(id),
        worker: w(worker),
        to: Recipient::Worker(w(worker)),
        bounds: b,
        velocity: [0; 3],
        ahead: 0,
        hysteresis: 3,
        activity: true,
        components: BTreeMap::from([(
            2,
            Demand {
                detail: Detail::Exact,
                period: 1,
            },
        )]),
    }
}
fn obj(id: u128, b: Box3) -> Presence {
    Presence {
        velocity: [0; 3],
        ahead: 0,
        id: Id::numbered(id),
        owner: w(99),
        principal: Id::numbered(id),
        bounds: b,
    }
}
#[test]
fn multiple_regions_prediction_hysteresis_and_no_merge_flood() {
    let mut d = Directory::default();
    let a = area(1, 1, bounds(0, 0, 0, 5));
    let b = area(2, 2, bounds(8, 0, 0, 5));
    let mut c = area(3, 1, bounds(10000, 0, 0, 5));
    let f = area(4, 3, bounds(10008, 0, 0, 5));
    for x in [a.clone(), b.clone(), c.clone(), f.clone()] {
        d.register_area(x).unwrap()
    }
    assert_eq!(d.islands().len(), 2);
    let mut travel = area(5, 7, bounds(5000, 0, 0, 6000));
    travel.activity = false;
    d.register_area(travel).unwrap();
    assert_eq!(d.islands().len(), 2);
    let p = obj(55, bounds(8, 0, 0, 2));
    d.upsert(p).unwrap();
    let count = d.subscribers(p.id).count();
    assert_eq!(count, 2);
    d.changes();
    c.bounds = bounds(10, 0, 0, 5);
    d.register_area(c.clone()).unwrap();
    assert_eq!(d.islands().len(), 2);
    // Identical demand through two regions for one worker is coalesced.
    d.register_area(area(6, 1, bounds(10, 0, 0, 5))).unwrap();
    let event = d.changes();
    assert!(
        event
            .iter()
            .filter(|e| e.to == Recipient::Worker(w(1)))
            .count()
            <= 1
    );
    let mut fast = area(10, 8, bounds(0, 100, 0, 1));
    fast.velocity = [0, -10, 0];
    fast.ahead = 10;
    d.register_area(fast).unwrap();
    assert!(d
        .subscribers(p.id)
        .all(|(to, _)| *to != Recipient::Worker(w(8))));
    let q = obj(56, bounds(0, 40, 0, 1));
    d.upsert(q).unwrap();
    assert!(d
        .subscribers(q.id)
        .any(|(to, _)| *to == Recipient::Worker(w(8))));
    let edge = obj(57, bounds(5, 0, 0, 0));
    d.upsert(edge).unwrap();
    d.changes();
    d.upsert(obj(57, bounds(7, 0, 0, 0))).unwrap();
    assert!(d
        .subscribers(edge.id)
        .any(|(to, _)| *to == Recipient::Worker(w(1))));
    d.upsert(obj(57, bounds(19, 0, 0, 0))).unwrap();
    assert!(d
        .subscribers(edge.id)
        .all(|(to, _)| *to != Recipient::Worker(w(1))));
}
#[test]
fn moving_object_replaces_observers_at_constant_count_and_fences_identity_changes() {
    let mut directory = Directory::default();
    directory
        .register_area(area(10, 1, bounds(0, 0, 0, 4)))
        .unwrap();
    directory
        .register_area(area(11, 2, bounds(80, 0, 0, 4)))
        .unwrap();
    let mut p = obj(123, bounds(0, 0, 0, 0));
    directory.upsert(p).unwrap();
    let first = directory.changes().pop().unwrap();
    p.bounds = bounds(6, 0, 0, 0);
    directory.upsert(p).unwrap();
    assert!(directory.changes().is_empty());
    p.bounds = bounds(80, 0, 0, 0);
    directory.upsert(p).unwrap();
    let changes = directory.changes();
    assert_eq!(changes.len(), 2);
    assert!(changes
        .iter()
        .any(|c| c.to == first.to && c.subscription.is_none()));
    let current = changes
        .iter()
        .find(|c| c.to == Recipient::Worker(w(2)))
        .unwrap();
    assert!(current.subscription.is_some());
    p.principal = Id::numbered(77);
    directory.upsert(p).unwrap();
    let replaced = directory.changes();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].to, current.to);
    assert!(replaced[0].subscription.is_some() && replaced[0].epoch > current.epoch);
    p.owner = w(2);
    directory.upsert(p).unwrap();
    assert!(directory.subscribers(p.id).next().is_none());
}

#[test]
fn indexed_queries_equal_full_3d_reference_including_large_coordinates() {
    let mut state = 333u64;
    let mut rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 32) % 160) as i64 - 80
    };
    let mut d = Directory::default();
    let mut regions = vec![];
    let origin = 1i64 << 58;
    for i in 1..=64 {
        let mut a = area(i, i as u32, bounds(origin + rand(), rand(), rand(), 10));
        a.hysteresis = 0;
        d.register_area(a.clone()).unwrap();
        regions.push(a);
    }
    for i in 0..400 {
        let mut p = obj(
            1000 + i,
            bounds(origin + rand(), rand(), rand(), i as i64 % 12),
        );
        if i % 17 == 0 {
            p.bounds.frame = Id::numbered(2)
        }
        d.upsert(p).unwrap();
        let expected: BTreeSet<_> = regions
            .iter()
            .filter(|a| a.bounds.intersects(p.bounds))
            .map(|a| a.to)
            .collect();
        assert_eq!(expected, d.subscribers(p.id).map(|(r, _)| *r).collect());
    }
    assert!(d.candidates < 64 * 400 / 5);
}
#[test]
fn late_route_deltas_and_deletions_are_fenced() {
    let id = Id::new();
    let to = Recipient::Client(id);
    let mut v = Views::default();
    let route = |epoch| InterestChange {
        object: id,
        to,
        epoch,
        subscription: Some(Subscription {
            epoch,
            components: BTreeMap::new(),
        }),
    };
    v.route(&route(10));
    let mut u = Update {
        object: id,
        epoch: 10,
        generation: 1,
        revision: 1,
        serial: 1,
        base: None,
        deleted: false,
        fields: BTreeMap::from([(2, Some(Field::new(1, &14u16).unwrap()))]),
    };
    assert!(v.apply(&u));
    assert!(!v.apply(&u));
    u.serial = 3;
    u.base = Some(2);
    assert!(!v.apply(&u));
    u.base = None;
    assert!(v.apply(&u));
    u.serial = 4;
    u.fields.insert(2, None);
    assert!(v.apply(&u));
    assert!(v.fields(id).unwrap().is_empty());
    v.route(&InterestChange {
        object: id,
        to,
        epoch: 11,
        subscription: None,
    });
    assert!(v.fields(id).is_none());
    assert!(!v.apply(&u));
    v.watermark(11);
    v.route(&route(10));
    assert!(v.fields(id).is_none());
    v.route(&route(12));
    u.epoch = 12;
    u.generation = 3;
    u.serial = 1;
    u.deleted = true;
    assert!(v.apply(&u));
    u.deleted = false;
    u.serial = 8;
    assert!(!v.apply(&u));
}
#[test]
fn placement_drain_budget_and_session_fairness() {
    let caps = vec![
        Capacity {
            worker: w(1),
            tick_us: 1100,
            budget_us: 1000,
            used: 80,
            memory: 100,
            draining: false,
        },
        Capacity {
            worker: w(2),
            tick_us: 20,
            budget_us: 1000,
            used: 0,
            memory: 100,
            draining: false,
        },
    ];
    let mut g = GroupLoad {
        group: Id::new(),
        owner: w(1),
        cost_us: 400,
        bytes: 45,
        moved_at: 0,
        affinity: Default::default(),
        cached: Default::default(),
    };
    assert_eq!(place(&g, &caps, 40, 20, 100), Placement::Move(w(2)));
    assert_eq!(place(&g, &caps, 4, 20, 100), Placement::Overloaded);
    assert_eq!(place(&g, &caps, 40, 20, 10), Placement::Overloaded);
    g.cost_us = 2000;
    assert_eq!(place(&g, &caps, 40, 20, 100), Placement::Overloaded);
    let a = Id::new();
    let b = Id::new();
    let make = |s, seq| Intent {
        key: ActionKey { session: s, seq },
        group: a,
        after: None,
        kind: 1,
        version: 1,
        bytes: vec![],
    };
    let mut q = FairQueue::new(3, 2);
    q.push(make(a, 1)).unwrap();
    q.push(make(a, 2)).unwrap();
    assert!(q.push(make(a, 3)).is_err());
    q.push(make(b, 1)).unwrap();
    let mut order = vec![];
    q.drain(3, std::time::Duration::from_secs(1), |i| {
        order.push(i.key.session)
    });
    assert_eq!(order, [a, b, a]);
}
#[test]
fn voxel_encodings_match_all_densities() {
    let base = vec![0; 4096];
    for stride in [1, 2, 7, 100, 4095] {
        let mut new = base.clone();
        for i in (0..new.len()).step_by(stride) {
            new[i] = ((i / 5) % 7 + 1) as u8;
        }
        for v in VoxelWrites::alternatives(&base, &new).unwrap() {
            let b = encode(&v).unwrap();
            let v: VoxelWrites = decode(&b).unwrap();
            let mut dest = base.clone();
            v.apply(&mut dest).unwrap();
            assert_eq!(dest, new);
        }
    }
}

#[test]
fn inbound_fast_passive_object_is_requested_before_entry() {
    let mut d = Directory::default();
    d.register_area(area(1, 1, bounds(0, 0, 0, 5))).unwrap();
    let mut p = obj(2, bounds(0, 0, 400, 3));
    p.velocity = [0, 0, -100];
    p.ahead = 4;
    d.upsert(p).unwrap();
    assert!(d
        .subscribers(p.id)
        .any(|(r, _)| *r == Recipient::Worker(w(1))));
    assert_eq!(d.islands().len(), 1);
    p.velocity = [0, 0, 100];
    d.upsert(p).unwrap();
    assert_eq!(d.subscribers(p.id).count(), 0);
}

#[test]
fn moving_overlapping_observers_preserve_epochs_and_aggregate_changed_demands() {
    let mut directory = Directory::bounded(32);
    let p = obj(50, bounds(0, 0, 0, 0));
    directory.upsert(p).unwrap();
    let mut a = area(10, 1, bounds(0, 0, 0, 5));
    let mut b = area(11, 1, a.bounds);
    directory.register_area(a.clone()).unwrap();
    directory.register_area(b.clone()).unwrap();
    let initial = directory.changes().pop().unwrap().epoch;
    // Keep A through hysteresis, then B leaves. Both are areas of the same recipient.
    a.bounds = bounds(7, 0, 0, 5);
    directory.register_area(a.clone()).unwrap();
    b.bounds = bounds(30, 0, 0, 5);
    directory.register_area(b.clone()).unwrap();
    assert!(directory.changes().is_empty());
    assert_eq!(directory.subscribers(p.id).next().unwrap().1.epoch, initial);
    a.components.get_mut(&2).unwrap().period = 3;
    directory.register_area(a.clone()).unwrap();
    let revised = directory.changes().pop().unwrap();
    assert!(revised.epoch > initial);
    assert_eq!(revised.subscription.unwrap().components[&2].period, 3);
    a.to = Recipient::Worker(w(2));
    directory.register_area(a.clone()).unwrap();
    let changes = directory.changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].to, Recipient::Worker(w(1)));
    assert!(changes[0].subscription.is_none());
    a.bounds = bounds(0, 0, 0, 5);
    directory.register_area(a.clone()).unwrap();
    assert_eq!(
        directory.subscribers(p.id).next().unwrap().0,
        &Recipient::Worker(w(2))
    );
    a.bounds = bounds(9, 0, 0, 5);
    directory.register_area(a).unwrap();
    assert!(directory.subscribers(p.id).next().is_none());
    assert_eq!(directory.islands().len(), 2);
}
