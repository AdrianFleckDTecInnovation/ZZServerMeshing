use std::sync::Arc;

/// Build a TLS acceptor from PEM certificate and key files.
///
/// Used by both sim-server (WSS) and coordinator (HTTPS).
pub fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    use rustls::pki_types::PrivateKeyDer;
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("Failed to open cert file {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to parse certs: {e}"))?;
    if certs.is_empty() {
        return Err("No certificates found in cert file".into());
    }

    let key_file = std::fs::File::open(key_path)
        .map_err(|e| format!("Failed to open key file {}: {e}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("Failed to parse private key: {e}"))?
        .ok_or("No private key found in key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("Failed to build TLS config: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}
