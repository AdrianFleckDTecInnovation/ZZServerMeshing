//! Delta compression: compare current entity state against a per-client baseline
//! to produce a minimal update set (new/changed entities + removed entity IDs).
//!
//! This module is pure logic with no ECS dependencies -- operates on EntityState
//! and HashMap only, making it thoroughly unit-testable.

use glam::Vec3;
use shared::entity::{EntityId, EntityState, EntityType, ServerId};
use std::collections::{HashMap, HashSet};

/// Threshold for considering a float "changed" (avoids floating-point noise).
/// 0.01 world units is sub-pixel at typical zoom levels.
const CHANGE_THRESHOLD: f32 = 0.01;

/// How often to force a full snapshot regardless of delta state.
/// 100 ticks = 5 seconds at 20Hz. Matches SHADOW_STALE_TICKS.
pub const FULL_SNAPSHOT_INTERVAL: u32 = 100;

/// Compact entity state for baseline comparison.
/// No EntityId field -- the baseline map is keyed by EntityId.
#[derive(Clone, Debug)]
pub struct CompactEntityState {
    pub position: Vec3,
    pub velocity: Vec3,
    pub entity_type: EntityType,
    pub authority_server: ServerId,
    pub last_input_seq: u32,
}

impl From<&EntityState> for CompactEntityState {
    fn from(e: &EntityState) -> Self {
        Self {
            position: e.position,
            velocity: e.velocity,
            entity_type: e.entity_type,
            authority_server: e.authority_server,
            last_input_seq: e.last_input_seq,
        }
    }
}

/// Result of a delta computation.
pub struct DeltaResult {
    /// New or changed entities (full state for encoding).
    pub updates: Vec<EntityState>,
    /// Entities present in the baseline but absent from current state.
    pub removals: Vec<EntityId>,
}

/// Compare current entity set against a baseline. Returns the delta.
pub fn compute_delta(
    current: &[EntityState],
    baseline: &HashMap<EntityId, CompactEntityState>,
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
pub fn build_baseline(entities: &[EntityState]) -> HashMap<EntityId, CompactEntityState> {
    entities
        .iter()
        .map(|e| (e.id, CompactEntityState::from(e)))
        .collect()
}

/// Update a baseline incrementally after sending a delta.
/// More efficient than rebuilding from scratch -- O(changed) instead of O(all).
pub fn update_baseline(
    baseline: &mut HashMap<EntityId, CompactEntityState>,
    delta: &DeltaResult,
) {
    for entity in &delta.updates {
        baseline.insert(entity.id, CompactEntityState::from(entity));
    }
    for id in &delta.removals {
        baseline.remove(id);
    }
}

fn entity_changed(current: &EntityState, baseline: &CompactEntityState) -> bool {
    vec3_changed(current.position, baseline.position)
        || vec3_changed(current.velocity, baseline.velocity)
        || current.entity_type != baseline.entity_type
        || current.authority_server != baseline.authority_server
        || current.last_input_seq != baseline.last_input_seq
}

fn vec3_changed(a: Vec3, b: Vec3) -> bool {
    (a.x - b.x).abs() > CHANGE_THRESHOLD
        || (a.y - b.y).abs() > CHANGE_THRESHOLD
        || (a.z - b.z).abs() > CHANGE_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(delta.updates.len(), 0, "movement below threshold should not produce update");
    }

    #[test]
    fn above_threshold_in_delta() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by 0.02 -- above threshold
        let mut current = entities.clone();
        current[0].position.x += 0.02;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "movement above threshold should produce update");
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
        assert_eq!(delta.updates.len(), 1, "velocity change should produce update");
    }

    #[test]
    fn authority_change_detected() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let baseline = build_baseline(&entities);

        // Change authority only
        let mut current = entities.clone();
        current[0].authority_server = ServerId(2);

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "authority change should produce update");
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
        assert_eq!(delta.updates.len(), 1, "last_input_seq change should produce update");
    }

    #[test]
    fn entity_type_change_detected() {
        let entities = vec![make_npc(Vec3::ZERO, 1)];
        let baseline = build_baseline(&entities);

        // Change entity_type
        let mut current = entities.clone();
        current[0].entity_type = EntityType::Player;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "entity_type change should produce update");
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
        assert_eq!(delta2.updates.len(), 0, "baseline should match current after update");
        assert_eq!(delta2.removals.len(), 0, "baseline should match current after update");
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
        assert_eq!(delta.updates.len(), 50, "10% of 500 entities should be in delta");
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
        assert_eq!(delta.updates.len(), 10, "all entities moved -> delta should contain all");
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
        assert_eq!(delta.removals.len(), 3, "all baseline entities should be removals");
    }

    #[test]
    fn empty_current_empty_baseline() {
        let baseline: HashMap<EntityId, CompactEntityState> = HashMap::new();
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
        assert_eq!(delta.updates.len(), 0, "movement exactly at threshold should not produce update");
    }

    #[test]
    fn threshold_boundary_just_above() {
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        // Move by threshold + small epsilon
        let mut current = entities.clone();
        current[0].position.x += CHANGE_THRESHOLD + 0.001;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "movement just above threshold should produce update");
    }

    #[test]
    fn y_axis_change_detected() {
        // Verify Y axis is not silently ignored
        let entities = vec![make_npc(Vec3::new(1.0, 0.0, 2.0), 1)];
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position.y += 5.0;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "Y axis position change should produce update");
    }

    #[test]
    fn negative_position_change_detected() {
        let entities = vec![make_npc(Vec3::new(100.0, 0.0, 100.0), 1)];
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].position.x = -100.0;

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 1, "large negative position change should produce update");
    }

    #[test]
    fn velocity_below_threshold_ignored() {
        let mut entities = vec![make_npc(Vec3::ZERO, 1)];
        entities[0].velocity = Vec3::new(1.0, 0.0, 0.0);
        let baseline = build_baseline(&entities);

        let mut current = entities.clone();
        current[0].velocity.x += 0.005; // below threshold

        let delta = compute_delta(&current, &baseline);
        assert_eq!(delta.updates.len(), 0, "velocity below threshold should not produce update");
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
            entities[0].clone(),  // unchanged
            entities[2].clone(),  // will be moved
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
        assert_eq!(FULL_SNAPSHOT_INTERVAL, 100, "periodic full snapshot should be every 100 ticks (5s at 20Hz)");
    }

    #[test]
    fn compact_entity_state_from_entity_state() {
        let mut e = make_player(Vec3::new(1.0, 2.0, 3.0), 5);
        e.velocity = Vec3::new(4.0, 5.0, 6.0);
        e.last_input_seq = 42;

        let compact = CompactEntityState::from(&e);
        assert_eq!(compact.position, e.position);
        assert_eq!(compact.velocity, e.velocity);
        assert_eq!(compact.entity_type, e.entity_type);
        assert_eq!(compact.authority_server, e.authority_server);
        assert_eq!(compact.last_input_seq, e.last_input_seq);
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
        assert_eq!(delta.updates[0].authority_server, entities[0].authority_server);
    }
}
