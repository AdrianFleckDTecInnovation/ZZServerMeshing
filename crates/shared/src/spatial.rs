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
