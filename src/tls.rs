//! TLS client config: pinned-certificate (or insecure) `ServerCertVerifier`.
//!
//! The TV's certificate (`CN=LGE TV SSG`, issued by LG's private intermediate CA)
//! doesn't match its address, so normal WebPKI verification can't work. Instead we
//! pin the exact end-entity certificate, ignoring the name and the chain, and still
//! check the handshake signatures so the server must hold the matching private key.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::config::TlsMode;

/// The TV's leaf certificate (DER), fetched with
/// `openssl s_client -connect 192.168.11.232:3001 -showcerts`.
/// `CN=LGE TV SSG`, valid 2018-03-12 to 2034-08-15,
/// SHA-256 `11:C5:B1:C5:90:77:50:AB:B9:DA:2A:66:65:CC:CE:2B:B2:88:A5:83:F4:5A:33:39:E7:1F:87:BF:2F:80:85:52`.
pub const PINNED_CERT: &[u8] = include_bytes!("../certs/lg-c6.der");

/// Text of the error returned when the TV presents a different certificate.
pub const PIN_MISMATCH: &str = "TV certificate does not match the pinned certificate \
     (a TV firmware update may have changed it): re-pin certs/lg-c6.der and rebuild, \
     or set `tls = \"insecure\"` in ~/.config/lgtv-wake/config.toml";

/// Build a rustls `ClientConfig` (ring provider) that verifies the TV per `mode`.
pub fn client_config(mode: TlsMode) -> Result<Arc<ClientConfig>> {
    build(pin_for(mode))
}

fn pin_for(mode: TlsMode) -> Option<&'static [u8]> {
    match mode {
        TlsMode::Pinned => Some(PINNED_CERT),
        TlsMode::Insecure => None,
    }
}

/// `pinned: None` accepts any certificate.
fn build(pinned: Option<&'static [u8]>) -> Result<Arc<ClientConfig>> {
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

/// Accepts the pinned end-entity certificate (or any, if insecure); always checks signatures.
#[derive(Debug)]
struct TvVerifier {
    /// `None` accepts any certificate.
    pinned: Option<&'static [u8]>,
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
        match self.pinned {
            Some(pinned) if end_entity.as_ref() != pinned => {
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

    fn verify(mode: TlsMode, cert: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
        let v = TvVerifier {
            pinned: pin_for(mode),
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
        assert!(verify(TlsMode::Pinned, PINNED_CERT).is_ok());
        let mut other = PINNED_CERT.to_vec();
        *other.last_mut().unwrap() ^= 1;
        let err = verify(TlsMode::Pinned, &other).unwrap_err().to_string();
        assert!(err.contains("tls = \"insecure\""), "{err}");
        assert!(err.contains("re-pin"), "{err}");
    }

    #[test]
    fn insecure_accepts_any_cert() {
        assert!(verify(TlsMode::Insecure, b"not a certificate").is_ok());
    }

    #[test]
    fn builds_without_default_provider() {
        client_config(TlsMode::Pinned).unwrap();
        client_config(TlsMode::Insecure).unwrap();
    }

    /// Handshake with the real TV using a wrong pin; checks the error text survives
    /// tungstenite. Read-only (no register). `cargo test -- --ignored live_`
    #[tokio::test]
    #[ignore = "needs the TV on the network"]
    async fn live_pin_mismatch_error() {
        static WRONG: [u8; 3] = [0x30, 0x01, 0x00];
        let cfg = build(Some(&WRONG)).unwrap();
        let err = tokio_tungstenite::connect_async_tls_with_config(
            "wss://192.168.11.232:3001",
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
