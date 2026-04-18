pub mod entity;
pub mod protocol;
pub mod spatial;
pub mod tick;
pub mod tls;

/// Protocol version for wire format compatibility.
/// Increment this whenever the binary UDP format or message structure changes.
/// Peers/clients with mismatched versions will reject each other's messages.
///
/// v2 (2026-04-17): PeerMessage variants gained a `session_id: u64` field to
/// enable peer-restart detection (design 013).
pub const PROTOCOL_VERSION: u16 = 2;
