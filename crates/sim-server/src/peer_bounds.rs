use glam::Vec3;

/// Radius for peer-to-peer replication filtering, in world units.
/// Larger than client AOI_RADIUS (500) to include a movement margin so
/// entities approaching a peer's client AoI are pre-replicated.
/// 750 = 500 (client AoI) + 250 (movement margin: ~5 units/tick * 50 ticks = 250 units look-ahead).
pub const PEER_AOI_RADIUS: f32 = 750.0;

/// Pre-computed squared peer AoI radius for hot-tier distance checks in peer replication.
/// Used to classify entities as hot (within PEER_AOI_RADIUS of a peer's player) vs cold.
pub const PEER_AOI_RADIUS_SQ: f32 = PEER_AOI_RADIUS * PEER_AOI_RADIUS;

/// Axis-aligned bounding box on the XZ plane, expanded by a radius.
/// Used to determine which of our authoritative entities are spatially
/// relevant to a peer server for filtered UDP replication.
#[derive(Debug, Clone)]
pub struct PeerBounds {
    pub min_x: f32,
    pub max_x: f32,
    pub min_z: f32,
    pub max_z: f32,
}

impl PeerBounds {
    /// Build from a set of positions, expanding by the given radius.
    /// Returns None if the iterator yields no positions.
    pub fn from_positions(positions: impl Iterator<Item = Vec3>, radius: f32) -> Option<Self> {
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_z = f32::MAX;
        let mut max_z = f32::MIN;
        let mut any = false;

        for pos in positions {
            any = true;
            min_x = min_x.min(pos.x);
            max_x = max_x.max(pos.x);
            min_z = min_z.min(pos.z);
            max_z = max_z.max(pos.z);
        }

        if !any {
            return None;
        }

        Some(Self {
            min_x: min_x - radius,
            max_x: max_x + radius,
            min_z: min_z - radius,
            max_z: max_z + radius,
        })
    }

    /// Check if a position falls within this bounding box (inclusive).
    #[inline]
    pub fn contains(&self, pos: Vec3) -> bool {
        pos.x >= self.min_x
            && pos.x <= self.max_x
            && pos.z >= self.min_z
            && pos.z <= self.max_z
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::spatial::AOI_RADIUS;

    // -- PEER_AOI_RADIUS tests --

    #[test]
    fn peer_aoi_radius_is_750() {
        assert_eq!(PEER_AOI_RADIUS, 750.0);
    }

    #[test]
    fn peer_aoi_radius_larger_than_client_aoi() {
        assert!(PEER_AOI_RADIUS > AOI_RADIUS);
    }

    #[test]
    fn peer_aoi_radius_sq_is_correct() {
        assert_eq!(PEER_AOI_RADIUS_SQ, 562500.0);
        assert_eq!(PEER_AOI_RADIUS_SQ, PEER_AOI_RADIUS * PEER_AOI_RADIUS);
    }

    // -- PeerBounds tests --

    #[test]
    fn peer_bounds_single_position() {
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(100.0, 0.0, 200.0)),
            750.0,
        )
        .unwrap();
        assert_eq!(bounds.min_x, -650.0);
        assert_eq!(bounds.max_x, 850.0);
        assert_eq!(bounds.min_z, -550.0);
        assert_eq!(bounds.max_z, 950.0);
    }

    #[test]
    fn peer_bounds_two_corners() {
        let positions = vec![
            Vec3::new(-100.0, 0.0, -200.0),
            Vec3::new(300.0, 0.0, 400.0),
        ];
        let bounds = PeerBounds::from_positions(positions.into_iter(), 750.0).unwrap();
        assert_eq!(bounds.min_x, -850.0);
        assert_eq!(bounds.max_x, 1050.0);
        assert_eq!(bounds.min_z, -950.0);
        assert_eq!(bounds.max_z, 1150.0);
    }

    #[test]
    fn peer_bounds_empty_returns_none() {
        let bounds = PeerBounds::from_positions(std::iter::empty(), 750.0);
        assert!(bounds.is_none());
    }

    #[test]
    fn peer_bounds_contains_inside() {
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(0.0, 0.0, 0.0)),
            750.0,
        )
        .unwrap();
        assert!(bounds.contains(Vec3::new(0.0, 0.0, 0.0)));
        assert!(bounds.contains(Vec3::new(500.0, 0.0, 500.0)));
        assert!(bounds.contains(Vec3::new(-700.0, 0.0, -700.0)));
    }

    #[test]
    fn peer_bounds_contains_on_boundary() {
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(0.0, 0.0, 0.0)),
            750.0,
        )
        .unwrap();
        // Exactly on boundary (inclusive)
        assert!(bounds.contains(Vec3::new(750.0, 0.0, 0.0)));
        assert!(bounds.contains(Vec3::new(-750.0, 0.0, 750.0)));
    }

    #[test]
    fn peer_bounds_contains_outside() {
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(0.0, 0.0, 0.0)),
            750.0,
        )
        .unwrap();
        assert!(!bounds.contains(Vec3::new(751.0, 0.0, 0.0)));
        assert!(!bounds.contains(Vec3::new(0.0, 0.0, -751.0)));
        assert!(!bounds.contains(Vec3::new(3000.0, 0.0, 3000.0)));
    }

    #[test]
    fn peer_bounds_contains_negative_coords() {
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(-2000.0, 0.0, -3000.0)),
            750.0,
        )
        .unwrap();
        assert!(bounds.contains(Vec3::new(-2000.0, 0.0, -3000.0)));
        assert!(bounds.contains(Vec3::new(-1300.0, 0.0, -2300.0)));
        assert!(!bounds.contains(Vec3::new(-1200.0, 0.0, -3000.0)));
    }

    #[test]
    fn peer_bounds_ignores_y() {
        // Two positions with different Y but same XZ should produce same bounds
        let positions = vec![
            Vec3::new(100.0, 0.0, 200.0),
            Vec3::new(100.0, 999.0, 200.0),
        ];
        let bounds = PeerBounds::from_positions(positions.into_iter(), 750.0).unwrap();
        assert_eq!(bounds.min_x, -650.0);
        assert_eq!(bounds.max_x, 850.0);
        assert_eq!(bounds.min_z, -550.0);
        assert_eq!(bounds.max_z, 950.0);
        // Y is ignored -- contains only checks XZ
        assert!(bounds.contains(Vec3::new(100.0, 5000.0, 200.0)));
    }

    // -- PEER_AOI_RADIUS boundary precision tests --

    #[test]
    fn peer_aoi_radius_boundary_distance_check() {
        // Verify that distance checks using PEER_AOI_RADIUS_SQ work correctly
        // at the exact boundary. Entity at 750 units should pass, 750.01 should fail.
        let player = Vec3::new(0.0, 0.0, 0.0);
        let at_boundary = Vec3::new(750.0, 0.0, 0.0);
        let just_outside = Vec3::new(750.01, 0.0, 0.0);

        let dx1 = player.x - at_boundary.x;
        let dz1 = player.z - at_boundary.z;
        assert!((dx1 * dx1 + dz1 * dz1) <= PEER_AOI_RADIUS_SQ, "750 should be within radius");

        let dx2 = player.x - just_outside.x;
        let dz2 = player.z - just_outside.z;
        assert!((dx2 * dx2 + dz2 * dz2) > PEER_AOI_RADIUS_SQ, "750.01 should be outside radius");
    }

    #[test]
    fn peer_aoi_diagonal_boundary() {
        // Diagonal distance at PEER_AOI_RADIUS: sqrt(530.33^2 + 530.33^2) = 750.0
        // Just inside: sqrt(530^2 + 530^2) = 749.53
        // Just outside: sqrt(531^2 + 531^2) = 750.95
        let player = Vec3::ZERO;

        let inside = Vec3::new(530.0, 0.0, 530.0);
        let dx = player.x - inside.x;
        let dz = player.z - inside.z;
        let dist_sq = dx * dx + dz * dz;
        assert!(dist_sq <= PEER_AOI_RADIUS_SQ,
            "diagonal 749.53 should be inside PEER_AOI_RADIUS (dist_sq={dist_sq}, limit={PEER_AOI_RADIUS_SQ})");

        let outside = Vec3::new(531.0, 0.0, 531.0);
        let dx = player.x - outside.x;
        let dz = player.z - outside.z;
        let dist_sq = dx * dx + dz * dz;
        assert!(dist_sq > PEER_AOI_RADIUS_SQ,
            "diagonal 750.95 should be outside PEER_AOI_RADIUS (dist_sq={dist_sq}, limit={PEER_AOI_RADIUS_SQ})");
    }

    #[test]
    fn peer_bounds_multiple_players_expands_box() {
        // PeerBounds with multiple player positions should create a bounding box
        // that covers all of them, expanded by the radius.
        let positions = vec![
            Vec3::new(-200.0, 0.0, 0.0),
            Vec3::new(300.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 400.0),
        ];
        let bounds = PeerBounds::from_positions(positions.into_iter(), PEER_AOI_RADIUS).unwrap();
        // min_x = -200 - 750 = -950
        assert_eq!(bounds.min_x, -950.0);
        // max_x = 300 + 750 = 1050
        assert_eq!(bounds.max_x, 1050.0);
        // min_z = 0 - 750 = -750
        assert_eq!(bounds.min_z, -750.0);
        // max_z = 400 + 750 = 1150
        assert_eq!(bounds.max_z, 1150.0);
    }

    #[test]
    fn peer_bounds_with_peer_aoi_radius() {
        // Verify PeerBounds.contains with PEER_AOI_RADIUS expansion
        // matches the expected AABB behavior.
        let bounds = PeerBounds::from_positions(
            std::iter::once(Vec3::new(1000.0, 0.0, 2000.0)),
            PEER_AOI_RADIUS,
        )
        .unwrap();

        // Inside: entity at (1000, 2000) should be contained
        assert!(bounds.contains(Vec3::new(1000.0, 0.0, 2000.0)));
        // Edge: at exactly (1750, 2000) should be contained (1000 + 750)
        assert!(bounds.contains(Vec3::new(1750.0, 0.0, 2000.0)));
        // Outside: at (1751, 2000) should NOT be contained
        assert!(!bounds.contains(Vec3::new(1751.0, 0.0, 2000.0)));
        // Corner: (1750, 2750) exactly on corner
        assert!(bounds.contains(Vec3::new(1750.0, 0.0, 2750.0)));
        // Just outside corner
        assert!(!bounds.contains(Vec3::new(1751.0, 0.0, 2750.0)));
    }
}
