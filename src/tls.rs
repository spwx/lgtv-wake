//! TLS client config: pinned-certificate (or insecure) `ServerCertVerifier`.
//!
//! The TV's certificate (`CN=LGE TV SSG`, issued by LG's private intermediate CA)
//! doesn't match its address, so normal WebPKI verification can't work. Instead we
//! pin the exact end-entity certificate, ignoring the name and the chain, and still
//! check the handshake signatures so the server must hold the matching private key.
//!
//! The pin is `~/.config/lgtv-wake/tv-cert.der` if `lgtv-wake pin` saved one, otherwise
//! the certificate embedded in the binary.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};

use crate::config::{self, TlsMode};

/// The TV's leaf certificate (DER), fetched with
/// `openssl s_client -connect 192.168.11.232:3001 -showcerts`.
/// `CN=LGE TV SSG`, valid 2018-03-12 to 2034-08-15,
/// SHA-256 `11:C5:B1:C5:90:77:50:AB:B9:DA:2A:66:65:CC:CE:2B:B2:88:A5:83:F4:5A:33:39:E7:1F:87:BF:2F:80:85:52`.
pub const PINNED_CERT: &[u8] = include_bytes!("../certs/lg-c6.der");

/// Text of the error returned when the TV presents a different certificate.
pub const PIN_MISMATCH: &str = "TV certificate does not match the pinned certificate \
     (a TV firmware update may have changed it): run `lgtv-wake pin` with the TV on \
     to pin the new one, or set `tls = \"insecure\"` in ~/.config/lgtv-wake/config.toml";

/// The certificate connections are checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub der: Vec<u8>,
    /// The file it came from, `None` for the one embedded in the binary.
    pub path: Option<PathBuf>,
}

impl Pin {
    /// Where the pin came from, for messages.
    pub fn source(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => "embedded in the binary".to_owned(),
        }
    }
}

/// The current pin: `~/.config/lgtv-wake/tv-cert.der` if it exists, else [`PINNED_CERT`].
pub fn current_pin() -> Result<Pin> {
    load_pin_from(&config::cert_path()?)
}

pub fn load_pin_from(path: &Path) -> Result<Pin> {
    match fs::read(path) {
        Ok(der) => Ok(Pin {
            der,
            path: Some(path.to_owned()),
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Pin {
            der: PINNED_CERT.to_vec(),
            path: None,
        }),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Build a rustls `ClientConfig` (ring provider) that verifies the TV per `mode`.
pub fn client_config(mode: TlsMode) -> Result<Arc<ClientConfig>> {
    let pinned = match mode {
        TlsMode::Pinned => Some(current_pin()?.der),
        TlsMode::Insecure => None,
    };
    build(pinned)
}

/// `pinned: None` accepts any certificate.
fn build(pinned: Option<Vec<u8>>) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(TvVerifier {
        pinned,
        algs: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("building TLS config")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Complete a TLS handshake with `host:port`, accepting any certificate (the handshake
/// signatures are still checked), and return the server's end-entity certificate (DER).
///
/// Blocking. Network failures keep their `std::io::Error` in the chain, so
/// [`crate::ssap::is_unreachable`] recognises a TV that's off.
pub fn fetch_leaf(host: &str, port: u16, timeout: Duration) -> Result<Vec<u8>> {
    let fetch = || -> Result<Vec<u8>> {
        let name = ServerName::try_from(host.to_owned())
            .with_context(|| format!("invalid host {host:?}"))?;
        let mut conn = ClientConnection::new(build(None)?, name).context("starting TLS")?;
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .context("no address for host")?;
        let mut sock = TcpStream::connect_timeout(&addr, timeout)?;
        sock.set_read_timeout(Some(timeout))?;
        sock.set_write_timeout(Some(timeout))?;
        let mut io = Timeouts(&mut sock);
        while conn.is_handshaking() {
            conn.complete_io(&mut io)?;
        }
        let leaf = conn
            .peer_certificates()
            .and_then(|certs| certs.first())
            .context("the server sent no certificate")?
            .to_vec();
        conn.send_close_notify();
        let _ = conn.complete_io(&mut io);
        Ok(leaf)
    };
    fetch().with_context(|| format!("TLS handshake with {host}:{port}"))
}

/// Reports socket timeouts as `TimedOut`: Unix reports them as `WouldBlock`, which rustls
/// would pass on as "try again".
struct Timeouts<'a>(&'a mut TcpStream);

fn timed_out(e: std::io::Error) -> std::io::Error {
    if e.kind() == ErrorKind::WouldBlock {
        ErrorKind::TimedOut.into()
    } else {
        e
    }
}

impl Read for Timeouts<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(timed_out)
    }
}

impl Write for Timeouts<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf).map_err(timed_out)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().map_err(timed_out)
    }
}

/// Accepts the pinned end-entity certificate (or any, if insecure); always checks signatures.
#[derive(Debug)]
struct TvVerifier {
    /// `None` accepts any certificate.
    pinned: Option<Vec<u8>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for TvVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match &self.pinned {
            Some(pinned) if end_entity.as_ref() != pinned.as_slice() => {
                Err(rustls::Error::General(PIN_MISMATCH.to_string()))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(pinned: Option<&[u8]>, cert: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
        let v = TvVerifier {
            pinned: pinned.map(<[u8]>::to_vec),
            algs: ring::default_provider().signature_verification_algorithms,
        };
        let name = ServerName::try_from("192.168.11.232").unwrap();
        v.verify_server_cert(
            &CertificateDer::from(cert.to_vec()),
            &[],
            &name,
            &[],
            UnixTime::now(),
        )
    }

    #[test]
    fn pinned_cert_is_der() {
        // DER SEQUENCE with a two-byte length.
        assert_eq!(&PINNED_CERT[..2], &[0x30, 0x82]);
        let len = u16::from_be_bytes([PINNED_CERT[2], PINNED_CERT[3]]) as usize;
        assert_eq!(PINNED_CERT.len(), len + 4);
    }

    #[test]
    fn pinned_accepts_only_the_pinned_cert() {
        assert!(verify(Some(PINNED_CERT), PINNED_CERT).is_ok());
        let mut other = PINNED_CERT.to_vec();
        *other.last_mut().unwrap() ^= 1;
        let err = verify(Some(PINNED_CERT), &other).unwrap_err().to_string();
        assert!(err.contains("tls = \"insecure\""), "{err}");
        assert!(err.contains("lgtv-wake pin"), "{err}");
        // A re-pinned certificate replaces the embedded one.
        assert!(verify(Some(&other), &other).is_ok());
        assert!(verify(Some(&other), PINNED_CERT).is_err());
    }

    #[test]
    fn insecure_accepts_any_cert() {
        assert!(verify(None, b"not a certificate").is_ok());
    }

    #[test]
    fn pin_file_overrides_embedded() {
        let dir = std::env::temp_dir().join(format!("lgtv-wake-pin-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tv-cert.der");
        let _ = fs::remove_file(&path);

        let pin = load_pin_from(&path).unwrap();
        assert_eq!(pin.der, PINNED_CERT);
        assert_eq!(pin.path, None);

        fs::write(&path, b"other").unwrap();
        let pin = load_pin_from(&path).unwrap();
        assert_eq!(pin.der, b"other");
        assert_eq!(pin.path.as_deref(), Some(path.as_path()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fetch_leaf_refused_is_unreachable() {
        // Bind then drop a listener to get a closed local port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = fetch_leaf("127.0.0.1", port, Duration::from_secs(1)).unwrap_err();
        assert!(crate::ssap::is_unreachable(&err), "{err:#}");
    }

    #[test]
    fn builds_without_default_provider() {
        build(Some(PINNED_CERT.to_vec())).unwrap();
        build(None).unwrap();
    }

    /// `cargo test -- --ignored live_`
    #[test]
    #[ignore = "needs the TV on the network"]
    fn live_fetch_leaf() {
        let leaf = fetch_leaf("192.168.8.5", 3001, Duration::from_secs(5)).unwrap();
        assert_eq!(leaf, PINNED_CERT);
    }

    /// Handshake with the real TV using a wrong pin; checks the error text survives
    /// tungstenite. Read-only (no register). `cargo test -- --ignored live_`
    #[tokio::test]
    #[ignore = "needs the TV on the network"]
    async fn live_pin_mismatch_error() {
        let cfg = build(Some(vec![0x30, 0x01, 0x00])).unwrap();
        let err = tokio_tungstenite::connect_async_tls_with_config(
            "wss://192.168.8.5:3001",
            None,
            true,
            Some(tokio_tungstenite::Connector::Rustls(cfg)),
        )
        .await
        .expect_err("handshake should fail");
        let text = format!("{err:#}");
        assert!(text.contains("tls = \"insecure\""), "{text}");
    }
}
