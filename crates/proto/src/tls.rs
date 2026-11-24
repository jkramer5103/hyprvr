//! TLS identities and QUIC endpoint configuration.
//!
//! Both sides use self-signed certificates. The TLS layer verifies handshake signatures
//! (proof of key possession) but accepts any certificate; trust comes from pinning the
//! SHA-256 certificate fingerprint established by PIN pairing (see `pairing`).

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::pairing::Fingerprint;

/// A self-signed certificate and its PKCS#8 private key (both DER).
#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn generate(common_name: &str) -> Self {
        let ck = rcgen::generate_simple_self_signed(vec![common_name.to_string()])
            .expect("self-signed certificate generation");
        Identity { cert_der: ck.cert.der().to_vec(), key_der: ck.signing_key.serialize_der() }
    }

    /// Load `<dir>/<name>.crt` + `<dir>/<name>.key` (DER), generating them if missing.
    pub fn load_or_generate(dir: &std::path::Path, name: &str) -> std::io::Result<Self> {
        let crt = dir.join(format!("{name}.crt"));
        let key = dir.join(format!("{name}.key"));
        if crt.exists() && key.exists() {
            return Ok(Identity { cert_der: std::fs::read(crt)?, key_der: std::fs::read(key)? });
        }
        std::fs::create_dir_all(dir)?;
        let id = Identity::generate("hyprvr");
        std::fs::write(&crt, &id.cert_der)?;
        write_private(&key, &id.key_der)?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert_der)
    }

    fn chain(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from(self.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone())),
        )
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

/// Human-readable short form for logs/UI, e.g. "3f9a:01c2:...".
pub fn short_fingerprint(fp: &Fingerprint) -> String {
    fp[..8].chunks(2).map(hex::encode).collect::<Vec<_>>().join(":")
}

/// Fingerprint of the peer's certificate on an established connection.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let id = conn.peer_identity()?;
    let certs = id.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| fingerprint(c.as_ref()))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AcceptAnyCert {
    algs: WebPkiSupportedAlgorithms,
}

impl AcceptAnyCert {
    fn new() -> Arc<Self> {
        Arc::new(AcceptAnyCert { algs: provider().signature_verification_algorithms })
    }
}

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for AcceptAnyCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Transport tuning shared by both ends: low-latency LAN streaming with big bursts.
pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(Duration::from_secs(6).try_into().unwrap()));
    t.max_concurrent_uni_streams(64u32.into());
    t.max_concurrent_bidi_streams(8u32.into());
    t.stream_receive_window((32u32 * 1024 * 1024).into());
    t.receive_window((96u32 * 1024 * 1024).into());
    t.send_window(96 * 1024 * 1024);
    t.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    t.datagram_send_buffer_size(4 * 1024 * 1024);
    t.initial_rtt(Duration::from_millis(10));
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

pub fn server_config(id: &Identity) -> quinn::ServerConfig {
    let (chain, key) = id.chain();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 supported")
        .with_client_cert_verifier(AcceptAnyCert::new())
        .with_single_cert(chain, key)
        .expect("valid server certificate");
    tls.alpn_protocols = vec![crate::ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let qc = quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("QUIC-compatible TLS config");
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qc));
    sc.transport_config(Arc::new(transport_config()));
    sc
}

pub fn client_config(id: &Identity) -> quinn::ClientConfig {
    let (chain, key) = id.chain();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 supported")
        .dangerous()
        .with_custom_certificate_verifier(AcceptAnyCert::new())
        .with_client_auth_cert(chain, key)
        .expect("valid client certificate");
    tls.alpn_protocols = vec![crate::ALPN.to_vec()];
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC-compatible TLS config");
    let mut cc = quinn::ClientConfig::new(Arc::new(qc));
    cc.transport_config(Arc::new(transport_config()));
    cc
}

/// Server name used in the TLS handshake (not verified).
pub const SERVER_NAME: &str = "hyprvr";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_exposes_fingerprints() {
        let sid = Identity::generate("hyprvr");
        let cid = Identity::generate("hyprvr");
        let server = quinn::Endpoint::server(server_config(&sid), "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(client_config(&cid));
        let accept = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            peer_fingerprint(&conn).unwrap()
        });
        let conn = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();
        assert_eq!(peer_fingerprint(&conn).unwrap(), sid.fingerprint());
        assert_eq!(accept.await.unwrap(), cid.fingerprint());
        assert_eq!(conn.max_datagram_size().is_some(), true);
    }
}
