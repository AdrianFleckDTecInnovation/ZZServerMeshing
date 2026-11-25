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

/// The native development launcher restricts its WS, HTTP and UDP listeners to
/// loopback. Standalone board and cloud launchers retain their network interfaces.
pub fn listen_addr(port: u16) -> std::net::SocketAddrV4 {
    use std::net::Ipv4Addr;
    let ip = if std::env::var_os("MESH_LOOPBACK_ONLY").is_some() {
        Ipv4Addr::LOCALHOST
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    std::net::SocketAddrV4::new(ip, port)
}
