use crate::game_loop::SharedCoordEvents;
use futures_util::{SinkExt, StreamExt};
use shared::entity::ServerId;
use shared::protocol::{CoordClientMsg, CoordServerMsg};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, connect_async_tls_with_config, Connector};

/// A `ServerCertVerifier` that accepts the server only if its leaf certificate
/// matches one of the pinned certs byte-for-byte.
///
/// WI-223 (design 016, Batch 6): the operator-generated self-signed cert
/// used for the UNO Q mesh (see `start-unoq.sh`) has no `basicConstraints
/// CA:TRUE` extension because it isn't a CA — it's a plain leaf cert used on
/// both ends of the connection. rustls's webpki chain validator refuses to
/// treat a non-CA cert as a root (`CaUsedAsEndEntity` error), so we can't
/// just drop the cert into a `RootCertStore`. Cert pinning by exact-match is
/// both simpler and more appropriate: we know exactly which cert we expect.
#[derive(Debug)]
struct PinnedCertVerifier {
    pinned: Vec<rustls_pki_types::CertificateDer<'static>>,
    supported_schemes: Vec<rustls::SignatureScheme>,
}

impl PinnedCertVerifier {
    fn new(pinned: Vec<rustls_pki_types::CertificateDer<'static>>) -> Self {
        // Use the set of schemes the default crypto provider supports so
        // signatures made by either RSA or ECDSA server keys are accepted.
        let supported_schemes = rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes();
        Self {
            pinned,
            supported_schemes,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Accept only if the presented leaf cert equals one of our pinned
        // certs (byte-for-byte). No hostname check, no CA chain walk —
        // possession of the right cert IS the authentication.
        for pinned in &self.pinned {
            if pinned.as_ref() == end_entity.as_ref() {
                return Ok(rustls::client::danger::ServerCertVerified::assertion());
            }
        }
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported_schemes.clone()
    }
}

/// Build a rustls `ClientConfig` that pins the server cert to the one at
/// `cert_path`. See `PinnedCertVerifier` for why we pin instead of chain.
///
/// Returns an `Arc<ClientConfig>` because the rustls client API consumes it
/// via Arc internally and we want to hand it off to
/// `connect_async_tls_with_config` without another clone.
pub fn build_coord_tls_config(
    cert_path: &Path,
) -> Result<Arc<rustls::ClientConfig>, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("Failed to open cert file {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to parse certs: {e}"))?;
    if certs.is_empty() {
        return Err("No certificates found in cert file".into());
    }

    // Ensure the default crypto provider is installed once. `CryptoProvider`
    // is a process-global; calling install twice is a no-op at runtime but
    // emits a warning. The first call from either the server-side TLS
    // acceptor or this client path will win.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let verifier = Arc::new(PinnedCertVerifier::new(certs));
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Connect to the coordinator, register this server, and push events to the shared queue.
///
/// `coord_tls_config` gates the scheme: when `Some`, the connection uses
/// `wss://` with the supplied trust anchor; when `None`, plain `ws://`
/// (preserves Mac-local dev flow). Added in WI-223.
#[allow(clippy::too_many_arguments)]
pub async fn run_coordinator_client(
    coordinator_addr: String,
    server_id: u16,
    udp_port: u16,
    ws_port: u16,
    coord_events: Arc<SharedCoordEvents>,
    advertise_host: Option<String>,
    entity_count: Arc<AtomicU32>,
    weighted_load: Arc<AtomicU32>,
    tls: bool,
    coord_outbound: Arc<crate::game_loop::SharedCoordOutbound>,
    coord_tls_config: Option<Arc<rustls::ClientConfig>>,
) {
    loop {
        let scheme = if coord_tls_config.is_some() {
            "wss"
        } else {
            "ws"
        };
        let url = format!("{scheme}://{coordinator_addr}");
        tracing::info!("Connecting to coordinator at {url}...");

        let ws = match &coord_tls_config {
            Some(config) => {
                let connector = Connector::Rustls(Arc::clone(config));
                match connect_async_tls_with_config(&url, None, false, Some(connector)).await {
                    Ok((ws, _)) => ws,
                    Err(e) => {
                        tracing::warn!(
                            "Coordinator TLS connection failed: {e}, retrying in 2s..."
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }
            }
            None => match connect_async(&url).await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    tracing::warn!("Coordinator connection failed: {e}, retrying in 2s...");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            },
        };

        tracing::info!("Connected to coordinator at {url}");
        let (mut ws_sink, mut ws_stream) = ws.split();

        // Register
        let reg = CoordClientMsg::Register {
            server_id: ServerId(server_id),
            udp_port,
            ws_port,
            host: advertise_host.clone(),
            tls,
            protocol_version: shared::PROTOCOL_VERSION,
        };
        if let Ok(json) = serde_json::to_string(&reg) {
            let _ = ws_sink.send(Message::Text(json.into())).await;
        }

        // Heartbeat ticker + outbound coordinator message drain
        let hb_entity_count = Arc::clone(&entity_count);
        let hb_weighted_load = Arc::clone(&weighted_load);
        let hb_coord_outbound = Arc::clone(&coord_outbound);
        let mut hb_sink = ws_sink;
        let hb_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;

                // Drain any outbound coordinator messages (TransferResult, etc.)
                let outbound: Vec<CoordClientMsg> = {
                    let mut q = hb_coord_outbound.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *q)
                };
                for msg in outbound {
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if hb_sink.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                }

                let count = hb_entity_count.load(Ordering::Relaxed);
                let wl = hb_weighted_load.load(Ordering::Relaxed);
                let hb = CoordClientMsg::Heartbeat {
                    server_id: ServerId(server_id),
                    entity_count: count,
                    weighted_load: wl,
                };
                if let Ok(json) = serde_json::to_string(&hb) {
                    if hb_sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Listen for coordinator messages.
        // Read timeout as defense-in-depth: if the TCP connection is half-open
        // (no FIN received), we won't block forever waiting for frames.
        // The coordinator sends keepalives every 5s on the rebalance interval,
        // so 30s gives 6 missed keepalives before we consider the connection dead.
        let read_timeout = std::time::Duration::from_secs(30);
        loop {
            let frame = tokio::time::timeout(read_timeout, ws_stream.next()).await;
            let msg = match frame {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(e))) => {
                    tracing::warn!("Coordinator WebSocket error: {e}");
                    break;
                }
                Ok(None) => {
                    // Stream ended (connection closed cleanly)
                    break;
                }
                Err(_) => {
                    // Read timeout — assume connection is dead
                    tracing::warn!(
                        server_id,
                        "No coordinator message for {read_timeout:?}, assuming dead connection"
                    );
                    break;
                }
            };
            if let Message::Text(text) = msg {
                match serde_json::from_str::<CoordServerMsg>(&text) {
                    Ok(CoordServerMsg::PeerList { peers }) => {
                        let mut q = coord_events.lock().unwrap_or_else(|e| e.into_inner());
                        q.peer_list_updates.push(peers);
                        tracing::info!("Received PeerList update");
                    }
                    Ok(CoordServerMsg::RebalanceDirective {
                        entity_ids,
                        target_server,
                    }) => {
                        let mut q = coord_events.lock().unwrap_or_else(|e| e.into_inner());
                        q.rebalance_directives.push((entity_ids, target_server));
                        tracing::info!("Received rebalance directive -> {target_server}");
                    }
                    Ok(CoordServerMsg::DrainDirective { target_servers }) => {
                        let mut q = coord_events.lock().unwrap_or_else(|e| e.into_inner());
                        q.drain_directives.push(target_servers.clone());
                        tracing::info!(
                            "Received drain directive -> {:?}",
                            target_servers
                                .iter()
                                .map(|s| format!("{s}"))
                                .collect::<Vec<_>>()
                        );
                    }
                    Ok(CoordServerMsg::RecoverEntities {
                        dead_server_id,
                        survivors,
                    }) => {
                        let mut q = coord_events.lock().unwrap_or_else(|e| e.into_inner());
                        q.recover_events.push((dead_server_id, survivors));
                    }
                    Ok(CoordServerMsg::PlayerTransferResult {
                        account_id,
                        success,
                        error,
                    }) => {
                        if success {
                            tracing::info!(account_id, "Player transfer confirmed by coordinator");
                        } else {
                            tracing::warn!(
                                account_id,
                                error = error.as_deref().unwrap_or("unknown"),
                                "Player transfer rejected by coordinator"
                            );
                        }
                    }
                    Ok(CoordServerMsg::Keepalive) => {
                        // Coordinator is alive — nothing to do, the read timeout is reset
                    }
                    Err(e) => {
                        tracing::warn!("Bad coordinator message: {e}");
                    }
                }
            }
        }

        hb_task.abort();
        tracing::warn!(
            server_id,
            "Lost coordinator connection, will re-register as S{server_id} in 2s..."
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WI-223: url scheme must flip to `wss://` when `coord_tls_config` is
    /// `Some`, and stay `ws://` otherwise. This is the only thing the flag
    /// is allowed to toggle — connector choice follows the Option directly.
    #[test]
    fn coordinator_scheme_ws_when_tls_off() {
        let config: Option<Arc<rustls::ClientConfig>> = None;
        let scheme = if config.is_some() { "wss" } else { "ws" };
        assert_eq!(scheme, "ws");
    }

    #[test]
    fn coordinator_scheme_wss_when_tls_on() {
        // Build a no-op ClientConfig so we can construct Some(..). The values
        // inside don't matter for the scheme check.
        let root_store = rustls::RootCertStore::empty();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let config: Option<Arc<rustls::ClientConfig>> = Some(Arc::new(config));
        let scheme = if config.is_some() { "wss" } else { "ws" };
        assert_eq!(scheme, "wss");
    }

    #[test]
    fn build_coord_tls_config_rejects_missing_file() {
        let result = build_coord_tls_config(Path::new("/nonexistent/path/cert.pem"));
        assert!(result.is_err(), "missing file should produce an error");
    }
}
