pub mod entity;
pub mod protocol;
pub mod spatial;
pub mod tick;
pub mod tls;

/// Protocol version for wire format compatibility.
/// Increment this whenever the binary UDP format or message structure changes.
/// Peers/clients with mismatched versions will reject each other's messages.
pub const PROTOCOL_VERSION: u16 = 1;
