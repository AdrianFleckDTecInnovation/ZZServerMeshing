//! Delta compression: compare current entity state against a per-client baseline
//! to produce a minimal update set (new/changed entities + removed entity IDs).
//!
//! This module is pure logic with no ECS dependencies -- operates on EntityState
//! and HashMap only, making it thoroughly unit-testable.

use glam::Vec3;
use shared::entity::{EntityId, EntityState};
use std::collections::{HashMap, HashSet};

/// Threshold for considering a float "changed" (avoids floating-point noise).
/// 0.01 world units is sub-pixel at typical zoom levels.
const CHANGE_THRESHOLD: f32 = 0.01;

/// Squared distance threshold for vector comparison (avoids sqrt).
/// Used by `vec3_changed` to detect position/velocity changes uniformly
/// across all movement directions (fixes audit finding #022).
const CHANGE_THRESHOLD_SQ: f32 = CHANGE_THRESHOLD * CHANGE_THRESHOLD;

/// How often to force a full snapshot regardless of delta state.
/// 100 ticks = 5 seconds at 20Hz. Well within SHADOW_STALE_TICKS (300 = 15s).
pub const FULL_SNAPSHOT_INTERVAL: u32 = 100;

/// Result of a delta computation.
pub struct DeltaResult {
    /// New or changed entities (full state for encoding).
    pub updates: Vec<EntityState>,
    /// Entities present in the baseline but absent from current state.
    pub removals: Vec<EntityId>,
}

/// Compute the fraction of entities that changed relative to a baseline.
/// Returns a value in [0.0, 1.0]. Counts new entities (not in baseline) and
/// changed entities (different fields), but does NOT allocate -- just counts.
/// Use this to decide whether delta compression is worthwhile before committing
/// to the full `compute_delta` call (which allocates Vecs).
///
/// Entities removed from the baseline are NOT counted as changes here because
/// the purpose is to estimate delta frame size (removed entities are 16-byte UUIDs,
/// not 47-byte entity records).
#[cfg(test)]
pub fn change_ratio(current: &[EntityState], baseline: &HashMap<EntityId, EntityState>) -> f32 {
    if current.is_empty() {
        return 0.0;
    }
    let mut changed = 0u32;
    for entity in current {
        match baseline.get(&entity.id) {
            None => changed += 1,
            Some(base) => {
                if entity_changed(entity, base) {
                    changed += 1;
                }
            }
        }
    }
    changed as f32 / current.len() as f32
}

/// Compare current entity set against a baseline. Returns the delta.
pub fn compute_delta(
    current: &[EntityState],
    baseline: &HashMap<EntityId, EntityState>,
) -> DeltaResult {
    let mut updates = Vec::new();
    let mut current_ids = HashSet::with_capacity(current.len());

    for entity in current {
        current_ids.insert(entity.id);
        match baseline.get(&entity.id) {
            None => {
                // New entity -- not in baseline
                updates.push(entity.clone());
            }
            Some(base) => {
                if entity_changed(entity, base) {
                    updates.push(entity.clone());
                }
            }
        }
    }

    let removals: Vec<EntityId> = baseline
        .keys()
        .filter(|id| !current_ids.contains(id))
        .copied()
        .collect();

    DeltaResult { updates, removals }
}

/// Build a baseline map from a full entity snapshot.
pub fn build_baseline(entities: &[EntityState]) -> HashMap<EntityId, EntityState> {
    entities.iter().map(|e| (e.id, e.clone())).collect()
}

/// Update a baseline incrementally after sending a delta.
/// More efficient than rebuilding from scratch -- O(changed) instead of O(all).
pub fn update_baseline(baseline: &mut HashMap<EntityId, EntityState>, delta: &DeltaResult) {
    for entity in &delta.updates {
        baseline.insert(entity.id, entity.clone());
    }
    for id in &delta.removals {
        baseline.remove(id);
    }
}

fn entity_changed(current: &EntityState, baseline: &EntityState) -> bool {
    vec3_changed(current.position, baseline.position)
        || vec3_changed(current.velocity, baseline.velocity)
        || current.entity_type != baseline.entity_type
        || current.authority_server != baseline.authority_server
        || current.last_input_seq != baseline.last_input_seq
}

fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    a.distance_squared(b) > CHANGE_THRESHOLD_SQ
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::entity::{EntityType, ServerId};

    fn make_npc(pos: Vec3, server: u16) -> EntityState {
        EntityState::new_npc(pos, ServerId(server))
    }

    fn make_player(pos: Vec3, server: u16) -> EntityState {
        EntityState::new_player(pos, ServerId(server))
    }

    #[test]
    fn empty_baseline_all_entities_are_updates() {
        let current = vec![
            make_npc(Vec3::ZERO, 1),
            make_npc(Vec3::X, 1),
            make_npc(Vec3::Z, 1),
            make_npc(Vec3::new(5.0, 0.0, 5.0), 2),
            make_player(Vec3::NEG_X, 1),
        ];
        let baseline = HashMap::new();
        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 5);
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn identical_baseline_empty_delta() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 1),
            make_npc(Vec3::new(5.0, 0.0, 6.0), 2),
            make_player(Vec3::new(7.0, 0.0, 8.0), 1),
            make_npc(Vec3::new(9.0, 0.0, 10.0), 2),
        ];
        let baseline = build_baseline(&entities);
        let delta = compute_delta(&entities, &baseline);
        assert_eq!(delta.updates.len(), 0);
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn two_entities_moved() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 1),
            make_npc(Vec3::new(5.0, 0.0, 6.0), 2),
            make_player(Vec3::new(7.0, 0.0, 8.0), 1),
            make_npc(Vec3::new(9.0, 0.0, 10.0), 2),
        ];
        let baseline = build_baseline(&entities);

        // Move entities 0 and 2
        let mut current = entities.clone();
        current[0].position = Vec3::new(100.0, 0.0, 200.0);
        current[2].position = Vec3::new(50.0, 0.0, 60.0);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 2);
        assert_eq!(delta.removals.len(), 0);

        let update_ids: HashSet<EntityId> = delta.updates.iter().map(|e| e.id).collect();
        assert!(update_ids.contains(&entities[0].id));
        assert!(update_ids.contains(&entities[2].id));
    }

    #[test]
    fn one_entity_removed() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 1),
            make_npc(Vec3::new(5.0, 0.0, 6.0), 2),
            make_player(Vec3::new(7.0, 0.0, 8.0), 1),
            make_npc(Vec3::new(9.0, 0.0, 10.0), 2),
        ];
        let baseline = build_baseline(&entities);

        // Remove entity at index 2
        let mut current = entities.clone();
        current.remove(2);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 0);
        assert_eq!(delta.removals.len(), 1);
        assert_eq!(delta.removals[0], entities[2].id);
    }

    #[test]
    fn one_entity_added() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 1),
            make_npc(Vec3::new(5.0, 0.0, 6.0), 2),
            make_player(Vec3::new(7.0, 0.0, 8.0), 1),
            make_npc(Vec3::new(9.0, 0.0, 10.0), 2),
        ];
        let baseline = build_baseline(&entities);

        // Add a new entity
        let mut current = entities.clone();
        let new_entity = make_npc(Vec3::new(100.0, 0.0, 100.0), 1);
        current.push(new_entity.clone());

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1);
        assert_eq!(delta.updates[0].id, new_entity.id);
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn below_threshold_not_in_delta() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by 0.005 -- below threshold
        let mut current = entities.clone();
        current[0].position.x += 0.005;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "movement below threshold should not produce update"
        );
    }

    #[test]
    fn above_threshold_in_delta() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by 0.02 -- above threshold
        let mut current = entities.clone();
        current[0].position.x += 0.02;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "movement above threshold should produce update"
        );
    }

    #[test]
    fn velocity_change_detected() {
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::new(1.0, 0.0, 0.0);
        let baseline = build_baseline(&entities);

        // Change velocity only
        let mut current = entities.clone();
        current[0].velocity = Vec3::new(0.0, 0.0, 1.0);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "velocity change should produce update"
        );
    }

    #[test]
    fn authority_change_detected() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let baseline = build_baseline(&entities);

        // Change authority only
        let mut current = entities.clone();
        current[0].authority_server = ServerId(2);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "authority change should produce update"
        );
    }

    #[test]
    fn last_input_seq_change_detected() {
        let mut entities = vec![make_player(Vec3::ZERO, 1)];
        entities[0].last_input_seq = 5;
        let baseline = build_baseline(&entities);

        // Change last_input_seq only
        let mut current = entities.clone();
        current[0].last_input_seq = 6;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "last_input_seq change should produce update"
        );
    }

    #[test]
    fn entity_type_change_detected() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let baseline = build_baseline(&entities);

        // Change entity_type
        let mut current = entities.clone();
        current[0].entity_type = EntityType::Player;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "entity_type change should produce update"
        );
    }

    #[test]
    fn build_baseline_roundtrips() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 2.0, 3.0), 1),
            make_player(Vec3::new(4.0, 5.0, 6.0), 2),
        ];
        let baseline = build_baseline(&entities);
        let delta = compute_delta(&entities, &baseline);
        assert_eq!(delta.updates.len(), 0);
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn update_baseline_applies_changes() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 1),
            make_npc(Vec3::new(5.0, 0.0, 6.0), 2),
        ];
        let mut baseline = build_baseline(&entities);

        // Move entity 0, remove entity 2, add entity 3
        let mut current = entities.clone();
        current[0].position = Vec3::new(100.0, 0.0, 200.0);
        current.remove(2);
        let new_entity = make_npc(Vec3::new(77.0, 0.0, 88.0), 1);
        current.push(new_entity.clone());

        let delta = compute_delta(&current, &baseline);
        update_baseline(&mut baseline, &delta);

        // Now comparing current against updated baseline should yield empty delta
        let delta2 = compute_delta(&current, &baseline);
        assert_eq!(
            delta2.updates.len(),
            0,
            "baseline should match current after update"
        );
        assert_eq!(
            delta2.removals.len(),
            0,
            "baseline should match current after update"
        );
    }

    #[test]
    fn large_entity_set_performance() {
        // Ensure delta computation works correctly with many entities
        let entities: Vec<EntityState> = (0..500)
            .map(|i| make_npc(Vec3::new(i as f32, 0.0, i as f32 * 2.0), (i % 3 + 1) as u16))
            .collect();
        let baseline = build_baseline(&entities);

        // Move 10% of entities
        let mut current = entities.clone();
        for i in (0..500).step_by(10) {
            current[i].position.x += 1.0;
        }

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            50,
            "10% of 500 entities should be in delta"
        );
        assert_eq!(delta.removals.len(), 0);
    }

    // -- Edge cases for delta computation --

    #[test]
    fn all_entities_changed_delta_equals_full() {
        let entities: Vec<EntityState> = (0..10)
            .map(|i| make_npc(Vec3::new(i as f32, 0.0, 0.0), 1))
            .collect();
        let baseline = build_baseline(&entities);

        // Move ALL entities
        let mut current = entities.clone();
        for e in &mut current {
            e.position.x += 100.0;
        }

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            10,
            "all entities moved -> delta should contain all"
        );
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn empty_current_all_removed() {
        let entities = vec![
            make_npc(Vec3::ZERO, 1),
            make_npc(Vec3::X, 2),
            make_player(Vec3::Z, 1),
        ];
        let baseline = build_baseline(&entities);

        let delta = compute_delta(&[], &baseline);
        assert_eq!(delta.updates.len(), 0);
        assert_eq!(
            delta.removals.len(),
            3,
            "all baseline entities should be removals"
        );
    }

    #[test]
    fn empty_current_empty_baseline() {
        let baseline: HashMap<EntityId, EntityState> = HashMap::new();
        let delta = compute_delta(&[], &baseline);
        assert_eq!(delta.updates.len(), 0);
        assert_eq!(delta.removals.len(), 0);
    }

    #[test]
    fn threshold_boundary_exactly_at_threshold() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by exactly the threshold (0.01) -- should NOT trigger since > is strict
        let mut current = entities.clone();
        current[0].position.x += CHANGE_THRESHOLD;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "movement exactly at threshold should not produce update"
        );
    }

    #[test]
    fn threshold_boundary_just_above() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by threshold + small epsilon
        let mut current = entities.clone();
        current[0].position.x += CHANGE_THRESHOLD + 0.001;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "movement just above threshold should produce update"
        );
    }

    #[test]
    fn y_axis_change_detected() {
        // Verify Y axis is not silently ignored
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position.y += 5.0;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "Y axis position change should produce update"
        );
    }

    #[test]
    fn negative_position_change_detected() {
        let entities = vec![make_npc(Vec3::new(100.0, 0.0, 100.0), 1)];
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position.x = -100.0;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "large negative position change should produce update"
        );
    }

    #[test]
    fn velocity_below_threshold_ignored() {
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::new(1.0, 0.0, 0.0);
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].velocity.x += 0.005; // below threshold

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "velocity below threshold should not produce update"
        );
    }

    #[test]
    fn simultaneous_add_remove_move() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 0.0), 1), // stays
            make_npc(Vec3::new(2.0, 0.0, 0.0), 1), // removed
            make_npc(Vec3::new(3.0, 0.0, 0.0), 2), // moved
        ];
        let baseline = build_baseline(&entities);

        let mut current = vec![
            entities[0].clone(), // unchanged
            entities[2].clone(), // will be moved
        ];
        current[1].position.x = 300.0; // move entity[2]
        let new_entity = make_npc(Vec3::new(4.0, 0.0, 0.0), 1);
        current.push(new_entity.clone()); // add new

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 2, "moved + new = 2 updates");
        assert_eq!(delta.removals.len(), 1, "one entity removed");
        assert_eq!(delta.removals[0], entities[1].id);
    }

    #[test]
    fn incremental_baseline_update_multiple_rounds() {
        let entities: Vec<EntityState> = (0..5)
            .map(|i| make_npc(Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1))
            .collect();
        let mut baseline = build_baseline(&entities);

        // Round 1: move entity 0
        let mut current = entities.clone();
        current[0].position.x = 999.0;
        let delta1 = compute_delta(&current, &baseline);
        assert_eq!(delta1.updates.len(), 1);
        update_baseline(&mut baseline, &delta1);

        // Round 2: move entity 0 again, verify only it shows up
        current[0].position.x = 1999.0;
        let delta2 = compute_delta(&current, &baseline);
        assert_eq!(delta2.updates.len(), 1);
        assert_eq!(delta2.updates[0].id, entities[0].id);
        update_baseline(&mut baseline, &delta2);

        // Round 3: no changes -> empty delta
        let delta3 = compute_delta(&current, &baseline);
        assert_eq!(delta3.updates.len(), 0);
        assert_eq!(delta3.removals.len(), 0);
    }

    #[test]
    fn update_baseline_with_empty_delta() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let mut baseline = build_baseline(&entities);
        assert_eq!(baseline.len(), 1);

        let delta = DeltaResult {
            updates: vec![],
            removals: vec![],
        };
        update_baseline(&mut baseline, &delta);
        assert_eq!(baseline.len(), 1, "empty delta should not change baseline");
    }

    #[test]
    fn full_snapshot_interval_constant_value() {
        assert_eq!(
            FULL_SNAPSHOT_INTERVAL, 100,
            "periodic full snapshot should be every 100 ticks (5s at 20Hz)"
        );
    }

    #[test]
    fn delta_preserves_full_entity_state_in_updates() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let baseline = HashMap::new(); // empty baseline -> all are new

        let delta = compute_delta(&entities, &baseline);
        assert_eq!(delta.updates.len(), 1);
        // Verify the full entity state is preserved, not just the changed fields
        assert_eq!(delta.updates[0].id, entities[0].id);
        assert_eq!(delta.updates[0].position, entities[0].position);
        assert_eq!(delta.updates[0].velocity, entities[0].velocity);
        assert_eq!(delta.updates[0].entity_type, entities[0].entity_type);
        assert_eq!(
            delta.updates[0].authority_server,
            entities[0].authority_server
        );
    }

    // -- Diagonal movement tests (vec3 distance threshold, audit #022) --

    #[test]
    fn diagonal_movement_above_threshold() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by 0.008 on X and Z. Per-component: each is below 0.01.
        // Vector distance: sqrt(0.008^2 + 0.008^2) = 0.01131 > 0.01.
        // Squared: 0.000064 + 0.000064 = 0.000128 > 0.0001.
        let mut current = entities.clone();
        current[0].position.x += 0.008;
        current[0].position.z += 0.008;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "diagonal movement above vector threshold should produce update"
        );
    }

    #[test]
    fn diagonal_movement_below_threshold() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by 0.005 on X and Z.
        // Squared: 0.000025 + 0.000025 = 0.000050 < 0.0001.
        let mut current = entities.clone();
        current[0].position.x += 0.005;
        current[0].position.z += 0.005;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "diagonal movement below vector threshold should not produce update"
        );
    }

    #[test]
    fn change_threshold_sq_value() {
        assert_eq!(
            CHANGE_THRESHOLD_SQ, 0.0001,
            "squared threshold should be 0.01^2 = 0.0001"
        );
    }

    // -- vec3_changed unit tests (WI-020: distance_squared threshold) --

    #[test]
    fn vec3_changed_zero_movement() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        assert!(
            !vec3_changed(a, a),
            "identical vectors should not be changed"
        );
    }

    #[test]
    fn vec3_changed_zero_vectors() {
        assert!(
            !vec3_changed(Vec3::ZERO, Vec3::ZERO),
            "two zero vectors should not be changed"
        );
    }

    #[test]
    fn vec3_changed_three_axis_diagonal_above() {
        // Move by 0.007 on each of X, Y, Z.
        // Per-component: each is below 0.01.
        // Vector distance_sq: 3 * 0.007^2 = 3 * 0.000049 = 0.000147 > 0.0001.
        let a = Vec3::ZERO;
        let b = Vec3::new(0.007, 0.007, 0.007);
        assert!(
            vec3_changed(a, b),
            "three-axis diagonal movement (magnitude ~0.012) should be detected as changed"
        );
    }

    #[test]
    fn vec3_changed_three_axis_diagonal_below() {
        // Move by 0.004 on each of X, Y, Z.
        // Vector distance_sq: 3 * 0.004^2 = 3 * 0.000016 = 0.000048 < 0.0001.
        let a = Vec3::ZERO;
        let b = Vec3::new(0.004, 0.004, 0.004);
        assert!(
            !vec3_changed(a, b),
            "three-axis diagonal movement (magnitude ~0.007) should not be detected as changed"
        );
    }

    #[test]
    fn vec3_changed_single_axis_just_below() {
        // Move by 0.009 on X only.
        // distance_sq: 0.009^2 = 0.000081 < 0.0001. NOT changed.
        let a = Vec3::ZERO;
        let b = Vec3::new(0.009, 0.0, 0.0);
        assert!(
            !vec3_changed(a, b),
            "single-axis movement of 0.009 (sq=0.000081 < 0.0001) should not be changed"
        );
    }

    #[test]
    fn vec3_changed_single_axis_just_above() {
        // Move by 0.011 on X only.
        // distance_sq: 0.011^2 = 0.000121 > 0.0001. Changed.
        let a = Vec3::ZERO;
        let b = Vec3::new(0.011, 0.0, 0.0);
        assert!(
            vec3_changed(a, b),
            "single-axis movement of 0.011 (sq=0.000121 > 0.0001) should be changed"
        );
    }

    #[test]
    fn vec3_changed_exactly_at_threshold() {
        // Move by exactly 0.01. distance_sq = 0.0001 = CHANGE_THRESHOLD_SQ.
        // Strict > means NOT changed.
        let a = Vec3::ZERO;
        let b = Vec3::new(CHANGE_THRESHOLD, 0.0, 0.0);
        assert!(
            !vec3_changed(a, b),
            "movement of exactly CHANGE_THRESHOLD should NOT be changed (strict >)"
        );
    }

    #[test]
    fn vec3_changed_large_movement() {
        let a = Vec3::new(-500.0, 0.0, -500.0);
        let b = Vec3::new(500.0, 0.0, 500.0);
        assert!(vec3_changed(a, b), "1000-unit movement should be changed");
    }

    #[test]
    fn vec3_changed_negative_direction() {
        let a = Vec3::new(1.0, 0.0, 0.0);
        let b = Vec3::new(1.0 - 0.02, 0.0, 0.0);
        assert!(
            vec3_changed(a, b),
            "negative movement above threshold should be changed"
        );
    }

    // -- Velocity diagonal detection (uses same vec3_changed) --

    #[test]
    fn velocity_diagonal_above_threshold_detected() {
        // Velocity change on two axes, each below per-component threshold but above distance threshold
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::new(1.0, 0.0, 0.0);
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].velocity = Vec3::new(1.008, 0.0, 0.008);
        // distance_sq = 0.008^2 + 0.008^2 = 0.000128 > 0.0001

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "diagonal velocity change above distance threshold should be detected"
        );
    }

    #[test]
    fn velocity_diagonal_below_threshold_ignored() {
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::new(1.0, 0.0, 0.0);
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].velocity = Vec3::new(1.005, 0.0, 0.005);
        // distance_sq = 0.005^2 + 0.005^2 = 0.00005 < 0.0001

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "diagonal velocity change below distance threshold should not be detected"
        );
    }

    // -- Position + velocity simultaneous small changes --

    #[test]
    fn position_below_velocity_below_no_update() {
        // Both position and velocity change, each below threshold independently
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::ZERO;
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position = Vec3::new(0.005, 0.0, 0.0);
        current[0].velocity = Vec3::new(0.005, 0.0, 0.0);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            0,
            "both position and velocity below threshold should produce no update"
        );
    }

    #[test]
    fn position_below_velocity_above_produces_update() {
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::ZERO;
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position = Vec3::new(0.005, 0.0, 0.0); // below threshold
        current[0].velocity = Vec3::new(0.02, 0.0, 0.0); // above threshold

        let delta = compute_delta(&current, &baseline);
        assert_eq!(
            delta.updates.len(),
            1,
            "velocity above threshold should trigger update even if position is below"
        );
    }

    // -- change_ratio tests (audit #049) --

    #[test]
    fn change_ratio_empty_current() {
        let baseline = build_baseline(&[make_npc(Vec3::ZERO, 1)]);
        assert_eq!(
            change_ratio(&[], &baseline),
            0.0,
            "empty current should return 0.0 regardless of baseline"
        );
    }

    #[test]
    fn change_ratio_empty_baseline_all_new() {
        let current = vec![make_npc(Vec3::ZERO, 1), make_npc(Vec3::X, 2)];
        let baseline = HashMap::new();
        assert_eq!(
            change_ratio(&current, &baseline),
            1.0,
            "all entities new (empty baseline) should return 1.0"
        );
    }

    #[test]
    fn change_ratio_no_changes() {
        let entities = vec![
            make_npc(Vec3::new(1.0, 0.0, 2.0), 1),
            make_npc(Vec3::new(3.0, 0.0, 4.0), 2),
        ];
        let baseline = build_baseline(&entities);
        assert_eq!(
            change_ratio(&entities, &baseline),
            0.0,
            "identical entities should return 0.0"
        );
    }

    #[test]
    fn change_ratio_all_changed() {
        let entities: Vec<EntityState> = (0..10)
            .map(|i| make_npc(Vec3::new(i as f32, 0.0, 0.0), 1))
            .collect();
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        for e in &mut current {
            e.position.x += 100.0;
        }
        assert_eq!(
            change_ratio(&current, &baseline),
            1.0,
            "all entities moved should return 1.0"
        );
    }

    #[test]
    fn change_ratio_partial() {
        let entities: Vec<EntityState> = (0..10)
            .map(|i| make_npc(Vec3::new(i as f32 * 10.0, 0.0, 0.0), 1))
            .collect();
        let baseline = build_baseline(&entities);

        // Move 7 of 10
        let mut current = entities.clone();
        for i in 0..7 {
            current[i].position.x += 100.0;
        }
        let ratio = change_ratio(&current, &baseline);
        assert!(
            (ratio - 0.7).abs() < 0.001,
            "7/10 changed should return ~0.7, got {}",
            ratio
        );
    }

    #[test]
    fn change_ratio_agrees_with_compute_delta() {
        // Verify that change_ratio produces the same value as
        // compute_delta().updates.len() / current.len()
        let entities: Vec<EntityState> = (0..20)
            .map(|i| make_npc(Vec3::new(i as f32 * 5.0, 0.0, 0.0), (i % 3 + 1) as u16))
            .collect();
        let baseline = build_baseline(&entities);

        // Move 13 of 20
        let mut current = entities.clone();
        for i in 0..13 {
            current[i].position.x += 50.0;
        }

        let ratio = change_ratio(&current, &baseline);
        let delta = compute_delta(&current, &baseline);
        let delta_ratio = delta.updates.len() as f32 / current.len() as f32;
        assert!(
            (ratio - delta_ratio).abs() < 0.001,
            "change_ratio ({}) should agree with compute_delta ratio ({})",
            ratio,
            delta_ratio
        );
    }
}
