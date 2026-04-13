use shared::entity::{EntityId, EntityState, EntityType};

pub const HEADER_SIZE: usize = 27; // 1 + 8 + 4 + 14
pub const DELTA_HEADER_SIZE: usize = 31; // 1 + 8 + 4 + 4 + 14
pub const ENTITY_SIZE: usize = 47; // 16 + 12 + 12 + 1 + 2 + 4

/// Encode a single entity into the binary format (47 bytes).
pub fn encode_entity(buf: &mut Vec<u8>, e: &EntityState) {
    // UUID (16 bytes)
    buf.extend_from_slice(e.id.0.as_bytes());
    // Position (3 x f32 LE)
    buf.extend_from_slice(&e.position.x.to_le_bytes());
    buf.extend_from_slice(&e.position.y.to_le_bytes());
    buf.extend_from_slice(&e.position.z.to_le_bytes());
    // Velocity (3 x f32 LE)
    buf.extend_from_slice(&e.velocity.x.to_le_bytes());
    buf.extend_from_slice(&e.velocity.y.to_le_bytes());
    buf.extend_from_slice(&e.velocity.z.to_le_bytes());
    // Entity type (u8: 0 = Npc, 1 = Player)
    buf.push(match e.entity_type {
        EntityType::Player => 1,
        EntityType::Npc => 0,
    });
    // Authority server (u16 LE)
    buf.extend_from_slice(&e.authority_server.0.to_le_bytes());
    // Last input seq (u32 LE)
    buf.extend_from_slice(&e.last_input_seq.to_le_bytes());
}

/// Encode a full snapshot into the binary frame format expected by the client.
///
/// Format: [type:u8=0x01][tick:u64 LE][count:u32 LE][metrics:14 bytes]
///         [entity x N: 47 bytes each]
pub fn encode_binary_snapshot(tick: u64, entities: &[EntityState]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + entities.len() * ENTITY_SIZE);

    buf.push(0x01);
    buf.extend_from_slice(&tick.to_le_bytes());
    buf.extend_from_slice(&(entities.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0u8; 14]); // metrics placeholder

    for e in entities {
        encode_entity(&mut buf, e);
    }

    buf
}

/// Encode a delta frame: only changed/new entities + removed entity IDs.
///
/// Format: [type:u8=0x02][tick:u64 LE][update_count:u32 LE][removal_count:u32 LE][metrics:14 bytes]
///         [updated entities x N: 47 bytes each]
///         [removed UUIDs x M: 16 bytes each]
pub fn encode_binary_delta(tick: u64, updates: &[EntityState], removals: &[EntityId]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        DELTA_HEADER_SIZE + updates.len() * ENTITY_SIZE + removals.len() * 16,
    );

    buf.push(0x02);
    buf.extend_from_slice(&tick.to_le_bytes());
    buf.extend_from_slice(&(updates.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(removals.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0u8; 14]); // metrics placeholder

    for e in updates {
        encode_entity(&mut buf, e);
    }
    for id in removals {
        buf.extend_from_slice(id.0.as_bytes());
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;
    use shared::entity::{EntityState, ServerId};

    #[test]
    fn encode_entity_produces_47_bytes() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(buf.len(), ENTITY_SIZE);
    }

    #[test]
    fn encode_entity_uuid_first_16_bytes() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(&buf[0..16], e.id.0.as_bytes());
    }

    #[test]
    fn encode_entity_position_layout() {
        let e = EntityState::new_npc(Vec3::new(1.5, 2.5, -3.5), ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let px = f32::from_le_bytes(buf[16..20].try_into().unwrap());
        let py = f32::from_le_bytes(buf[20..24].try_into().unwrap());
        let pz = f32::from_le_bytes(buf[24..28].try_into().unwrap());
        assert_eq!(px, 1.5);
        assert_eq!(py, 2.5);
        assert_eq!(pz, -3.5);
    }

    #[test]
    fn encode_entity_velocity_layout() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        e.velocity = Vec3::new(4.0, 5.0, 6.0);
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let vx = f32::from_le_bytes(buf[28..32].try_into().unwrap());
        let vy = f32::from_le_bytes(buf[32..36].try_into().unwrap());
        let vz = f32::from_le_bytes(buf[36..40].try_into().unwrap());
        assert_eq!(vx, 4.0);
        assert_eq!(vy, 5.0);
        assert_eq!(vz, 6.0);
    }

    #[test]
    fn encode_entity_type_npc_is_zero() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(buf[40], 0);
    }

    #[test]
    fn encode_entity_type_player_is_one() {
        let e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        assert_eq!(buf[40], 1);
    }

    #[test]
    fn encode_entity_authority_at_offset_41() {
        let e = EntityState::new_npc(Vec3::ZERO, ServerId(42));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let auth = u16::from_le_bytes(buf[41..43].try_into().unwrap());
        assert_eq!(auth, 42);
    }

    #[test]
    fn encode_entity_last_input_seq_at_offset_43() {
        let mut e = EntityState::new_player(Vec3::ZERO, ServerId(1));
        e.last_input_seq = 777;
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let seq = u32::from_le_bytes(buf[43..47].try_into().unwrap());
        assert_eq!(seq, 777);
    }

    #[test]
    fn encode_entity_nan_position_preserved() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        e.position = Vec3::new(f32::NAN, 0.0, 0.0);
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let px = f32::from_le_bytes(buf[16..20].try_into().unwrap());
        assert!(px.is_nan());
    }

    #[test]
    fn encode_entity_negative_zero() {
        let mut e = EntityState::new_npc(Vec3::ZERO, ServerId(1));
        e.position = Vec3::new(-0.0, -0.0, -0.0);
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e);
        let px = f32::from_le_bytes(buf[16..20].try_into().unwrap());
        assert!(px.is_sign_negative());
    }

    #[test]
    fn encode_multiple_entities_appends_correctly() {
        let e1 = EntityState::new_npc(Vec3::new(1.0, 0.0, 0.0), ServerId(1));
        let e2 = EntityState::new_player(Vec3::new(2.0, 0.0, 0.0), ServerId(2));
        let mut buf = Vec::new();
        encode_entity(&mut buf, &e1);
        encode_entity(&mut buf, &e2);
        assert_eq!(buf.len(), ENTITY_SIZE * 2);
        // Second entity uuid starts at offset 47
        assert_eq!(&buf[47..63], e2.id.0.as_bytes());
    }

    #[test]
    fn snapshot_header_size_constant_matches() {
        let buf = encode_binary_snapshot(0, &[]);
        assert_eq!(buf.len(), HEADER_SIZE);
    }

    #[test]
    fn snapshot_frame_type_is_0x01() {
        let buf = encode_binary_snapshot(0, &[]);
        assert_eq!(buf[0], 0x01);
    }

    #[test]
    fn snapshot_tick_encoded_as_u64_le() {
        let buf = encode_binary_snapshot(0x0102030405060708u64, &[]);
        let tick = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(tick, 0x0102030405060708u64);
    }

    #[test]
    fn snapshot_entity_count_in_header() {
        let entities: Vec<EntityState> = (0..5)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();
        let buf = encode_binary_snapshot(1, &entities);
        let count = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(count, 5);
    }

    #[test]
    fn snapshot_metrics_placeholder_is_zeroes() {
        let buf = encode_binary_snapshot(1, &[]);
        assert_eq!(&buf[13..27], &[0u8; 14]);
    }

    #[test]
    fn snapshot_total_size_formula() {
        for n in [0, 1, 10, 100] {
            let entities: Vec<EntityState> = (0..n)
                .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
                .collect();
            let buf = encode_binary_snapshot(1, &entities);
            assert_eq!(buf.len(), HEADER_SIZE + n * ENTITY_SIZE);
        }
    }

    #[test]
    fn delta_header_size_constant_matches() {
        let buf = encode_binary_delta(0, &[], &[]);
        assert_eq!(buf.len(), DELTA_HEADER_SIZE);
    }

    #[test]
    fn delta_frame_type_is_0x02() {
        let buf = encode_binary_delta(0, &[], &[]);
        assert_eq!(buf[0], 0x02);
    }

    #[test]
    fn delta_update_and_removal_counts() {
        let updates: Vec<EntityState> = (0..3)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();
        let removals: Vec<EntityId> = (0..2).map(|_| EntityId::new()).collect();
        let buf = encode_binary_delta(1, &updates, &removals);
        let update_count = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        let removal_count = u32::from_le_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(update_count, 3);
        assert_eq!(removal_count, 2);
    }

    #[test]
    fn delta_total_size_formula() {
        let updates: Vec<EntityState> = (0..4)
            .map(|_| EntityState::new_npc(Vec3::ZERO, ServerId(1)))
            .collect();
        let removals: Vec<EntityId> = (0..3).map(|_| EntityId::new()).collect();
        let buf = encode_binary_delta(1, &updates, &removals);
        assert_eq!(
            buf.len(),
            DELTA_HEADER_SIZE + 4 * ENTITY_SIZE + 3 * 16
        );
    }

    #[test]
    fn delta_removal_uuids_at_correct_offset() {
        let updates = vec![EntityState::new_npc(Vec3::ZERO, ServerId(1))];
        let r1 = EntityId::new();
        let r2 = EntityId::new();
        let buf = encode_binary_delta(1, &updates, &[r1, r2]);
        let removal_start = DELTA_HEADER_SIZE + ENTITY_SIZE;
        assert_eq!(&buf[removal_start..removal_start + 16], r1.0.as_bytes());
        assert_eq!(
            &buf[removal_start + 16..removal_start + 32],
            r2.0.as_bytes()
        );
    }

    #[test]
    fn delta_metrics_placeholder_is_zeroes() {
        let buf = encode_binary_delta(1, &[], &[]);
        assert_eq!(&buf[17..31], &[0u8; 14]);
    }
}
