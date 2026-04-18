use glam::Vec3;

/// Size of each spatial grid cell in world units.
pub const CELL_SIZE: f32 = 50.0;

/// World half-extent: entities are clamped to [-WORLD_EXTENT, WORLD_EXTENT] on X and Z.
pub const WORLD_EXTENT: f32 = 5000.0;

/// Convert a world position to a grid cell coordinate.
pub fn world_to_cell(pos: Vec3) -> (i32, i32) {
    let cx = (pos.x / CELL_SIZE).floor() as i32;
    let cz = (pos.z / CELL_SIZE).floor() as i32;
    (cx, cz)
}

/// Get all 8 neighboring cells plus the cell itself (9 total).
pub fn cell_and_neighbors(cell: (i32, i32)) -> Vec<(i32, i32)> {
    let mut result = Vec::with_capacity(9);
    for dx in -1..=1 {
        for dz in -1..=1 {
            result.push((cell.0 + dx, cell.1 + dz));
        }
    }
    result
}

/// Get all cells within a given radius (in cells) from a center cell.
pub fn cells_within_radius(center: (i32, i32), radius: i32) -> Vec<(i32, i32)> {
    let mut cells = Vec::with_capacity(((2 * radius + 1) * (2 * radius + 1)) as usize);
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            cells.push((center.0 + dx, center.1 + dz));
        }
    }
    cells
}

/// Clamp a position to the world bounds.
pub fn clamp_to_world(pos: Vec3) -> Vec3 {
    Vec3::new(
        pos.x.clamp(-WORLD_EXTENT, WORLD_EXTENT),
        pos.y,
        pos.z.clamp(-WORLD_EXTENT, WORLD_EXTENT),
    )
}

/// Area-of-Interest radius for client snapshots. Entities within this distance
/// of a client's player entity are included in their snapshot.
pub const AOI_RADIUS: f32 = 500.0;

/// Pre-computed squared AoI radius (avoids sqrt in distance checks).
pub const AOI_RADIUS_SQ: f32 = AOI_RADIUS * AOI_RADIUS;

/// Check if an entity position is within the Area of Interest of a player.
/// Uses squared 2D distance on the XZ plane (Y is ignored -- flat world).
pub fn in_aoi(player_pos: Vec3, entity_pos: Vec3) -> bool {
    let dx = player_pos.x - entity_pos.x;
    let dz = player_pos.z - entity_pos.z;
    (dx * dx + dz * dz) <= AOI_RADIUS_SQ
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_to_cell_origin() {
        assert_eq!(world_to_cell(Vec3::new(0.0, 0.0, 0.0)), (0, 0));
    }

    #[test]
    fn world_to_cell_positive() {
        assert_eq!(world_to_cell(Vec3::new(75.0, 0.0, 125.0)), (1, 2));
    }

    #[test]
    fn world_to_cell_negative() {
        assert_eq!(world_to_cell(Vec3::new(-10.0, 0.0, -60.0)), (-1, -2));
    }

    #[test]
    fn world_to_cell_on_boundary() {
        // Exactly on cell boundary
        assert_eq!(world_to_cell(Vec3::new(50.0, 0.0, 100.0)), (1, 2));
    }

    #[test]
    fn cell_and_neighbors_count() {
        let cells = cell_and_neighbors((0, 0));
        assert_eq!(cells.len(), 9);
    }

    #[test]
    fn cell_and_neighbors_contains_self() {
        let cells = cell_and_neighbors((3, -2));
        assert!(cells.contains(&(3, -2)));
    }

    #[test]
    fn cell_and_neighbors_all_adjacent() {
        let cells = cell_and_neighbors((0, 0));
        for dx in -1..=1 {
            for dz in -1..=1 {
                assert!(cells.contains(&(dx, dz)));
            }
        }
    }

    #[test]
    fn cells_within_radius_zero() {
        let cells = cells_within_radius((0, 0), 0);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], (0, 0));
    }

    #[test]
    fn cells_within_radius_one_equals_nine() {
        let cells = cells_within_radius((5, 5), 1);
        assert_eq!(cells.len(), 9);
        assert!(cells.contains(&(5, 5)));
        assert!(cells.contains(&(4, 4)));
        assert!(cells.contains(&(6, 6)));
    }

    #[test]
    fn cells_within_radius_two() {
        let cells = cells_within_radius((0, 0), 2);
        assert_eq!(cells.len(), 25); // 5x5
    }

    #[test]
    fn clamp_inside_bounds() {
        let pos = Vec3::new(100.0, 5.0, -200.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped, pos);
    }

    #[test]
    fn clamp_exceeds_positive() {
        let pos = Vec3::new(6000.0, 0.0, 9999.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped.x, WORLD_EXTENT);
        assert_eq!(clamped.z, WORLD_EXTENT);
    }

    #[test]
    fn clamp_exceeds_negative() {
        let pos = Vec3::new(-8000.0, 0.0, -5001.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped.x, -WORLD_EXTENT);
        assert_eq!(clamped.z, -WORLD_EXTENT);
    }

    #[test]
    fn clamp_preserves_y() {
        // Y axis is not clamped (used for rendering, not world bounds)
        let pos = Vec3::new(0.0, 9999.0, 0.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped.y, 9999.0);
    }

    #[test]
    fn clamp_at_exact_boundary() {
        let pos = Vec3::new(WORLD_EXTENT, 0.0, -WORLD_EXTENT);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped, pos);
    }

    #[test]
    fn clamp_at_new_extent() {
        let pos = Vec3::new(5000.0, 0.0, -5000.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped, pos);
    }

    #[test]
    fn clamp_beyond_new_extent() {
        let pos = Vec3::new(6000.0, 0.0, -6000.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped, Vec3::new(5000.0, 0.0, -5000.0));
    }

    // -- in_aoi tests --

    #[test]
    fn in_aoi_same_position() {
        let pos = Vec3::new(100.0, 0.0, 200.0);
        assert!(in_aoi(pos, pos));
    }

    #[test]
    fn in_aoi_inside_radius() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(300.0, 0.0, 0.0);
        assert!(in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_exactly_on_boundary() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(500.0, 0.0, 0.0); // exactly AOI_RADIUS away
        assert!(in_aoi(player, entity)); // uses <=
    }

    #[test]
    fn in_aoi_just_outside() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(501.0, 0.0, 0.0);
        assert!(!in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_far_away() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(3000.0, 0.0, 0.0);
        assert!(!in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_negative_coords() {
        let player = Vec3::new(-200.0, 0.0, -300.0);
        let entity = Vec3::new(-600.0, 0.0, -300.0); // dist = 400
        assert!(in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_diagonal() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        // dist = sqrt(354^2 + 354^2) = sqrt(250632) = 500.63... > 500
        let entity = Vec3::new(354.0, 0.0, 354.0);
        assert!(!in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_ignores_y() {
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(0.0, 9999.0, 0.0); // huge Y, but XZ distance is 0
        assert!(in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_both_far_from_origin_within_range() {
        // Both positions far from origin, but within 500 of each other.
        let player = Vec3::new(4000.0, 0.0, -3000.0);
        let entity = Vec3::new(4200.0, 0.0, -2800.0); // dist = sqrt(200^2 + 200^2) = 282.8
        assert!(in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_both_far_from_origin_outside_range() {
        // Both positions far from origin, but more than 500 apart.
        let player = Vec3::new(4000.0, 0.0, -3000.0);
        let entity = Vec3::new(4400.0, 0.0, -2500.0); // dist = sqrt(400^2 + 500^2) = 640.3
        assert!(!in_aoi(player, entity));
    }

    #[test]
    fn in_aoi_symmetric() {
        // AoI should be symmetric: in_aoi(a, b) == in_aoi(b, a)
        let a = Vec3::new(100.0, 0.0, 200.0);
        let b = Vec3::new(500.0, 0.0, 400.0); // dist = sqrt(400^2 + 200^2) = 447.2
        assert_eq!(in_aoi(a, b), in_aoi(b, a));
    }

    #[test]
    fn in_aoi_max_world_corners() {
        // Two entities at opposite corners of the 10k world. Should be far outside AoI.
        let corner_a = Vec3::new(-5000.0, 0.0, -5000.0);
        let corner_b = Vec3::new(5000.0, 0.0, 5000.0);
        assert!(!in_aoi(corner_a, corner_b));
    }

    #[test]
    fn in_aoi_diagonal_just_inside() {
        // Diagonal distance just under 500: sqrt(353^2 + 353^2) = 499.31
        let player = Vec3::new(0.0, 0.0, 0.0);
        let entity = Vec3::new(353.0, 0.0, 353.0);
        assert!(in_aoi(player, entity));
    }
}
