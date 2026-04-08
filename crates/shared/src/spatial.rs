use glam::Vec3;

/// Size of each spatial grid cell in world units.
pub const CELL_SIZE: f32 = 50.0;

/// World half-extent: entities are clamped to [-WORLD_EXTENT, WORLD_EXTENT] on X and Z.
pub const WORLD_EXTENT: f32 = 500.0;

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

/// Clamp a position to the world bounds.
pub fn clamp_to_world(pos: Vec3) -> Vec3 {
    Vec3::new(
        pos.x.clamp(-WORLD_EXTENT, WORLD_EXTENT),
        pos.y,
        pos.z.clamp(-WORLD_EXTENT, WORLD_EXTENT),
    )
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
    fn clamp_inside_bounds() {
        let pos = Vec3::new(100.0, 5.0, -200.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped, pos);
    }

    #[test]
    fn clamp_exceeds_positive() {
        let pos = Vec3::new(600.0, 0.0, 999.0);
        let clamped = clamp_to_world(pos);
        assert_eq!(clamped.x, WORLD_EXTENT);
        assert_eq!(clamped.z, WORLD_EXTENT);
    }

    #[test]
    fn clamp_exceeds_negative() {
        let pos = Vec3::new(-800.0, 0.0, -501.0);
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
}
