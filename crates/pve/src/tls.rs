//! rustls configuration with leaf-certificate pinning (see crate docs).
//!
//! One [`Tls`] per [`crate::Client`]: a single `rustls::ClientConfig` (aws-lc-rs, safe default
//! protocol versions, no client auth) with a [`PinningVerifier`], shared by the HTTP client and
//! the console websocket.

use std::error::Error as StdError;
use std::sync::{Arc, Mutex, PoisonError};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, OtherError, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::{CertProblem, Error};

/// The TLS setup shared by all connections of one client.
#[derive(Clone, Debug)]
pub(crate) struct Tls {
    /// Without ALPN (reqwest adds its own to its copy; the websocket needs plain HTTP/1.1).
    pub(crate) config: Arc<ClientConfig>,
    verifier: Arc<PinningVerifier>,
}

impl Tls {
    /// Builds the client config for an optional pinned leaf fingerprint.
    pub(crate) fn new(pin: Option<[u8; 32]>) -> Result<Self, Error> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Arc::new(PinningVerifier::new(pin, Arc::clone(&provider))?);
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Config(format!("TLS setup failed: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        Ok(Self { config: Arc::new(config), verifier })
    }

    /// The certificate rejection behind a failed connection, if that is what happened.
    ///
    /// Decided by `err` alone: the rejection travels inside the error chain, which is exact
    /// even with concurrent handshakes. The verifier's record is shared by every connection of
    /// the client (and may be left over from a cancelled or background handshake), so it is
    /// only used when the chain itself shows a certificate failure without its details. It is
    /// cleared either way.
    pub(crate) fn rejection_for(&self, err: &(dyn StdError + 'static)) -> Option<CertProblem> {
        let recorded = self.verifier.take_rejection();
        find_rejection(err).or_else(|| recorded.filter(|_| has_cert_failure(err)))
    }
}

/// Accepts exactly the pinned leaf certificate (SSH style: no name / expiry / chain checks), or
/// without a pin whatever the OS trust store accepts. Handshake signatures are always verified.
#[derive(Debug)]
pub(crate) struct PinningVerifier {
    pin: Option<[u8; 32]>,
    /// The OS trust store; only consulted when nothing is pinned.
    platform: Option<rustls_platform_verifier::Verifier>,
    algorithms: WebPkiSupportedAlgorithms,
    rejection: Mutex<Option<CertProblem>>,
}

impl PinningVerifier {
    /// A verifier for `pin` (or the OS trust store when `None`).
    pub(crate) fn new(pin: Option<[u8; 32]>, provider: Arc<CryptoProvider>) -> Result<Self, Error> {
        let algorithms = provider.signature_verification_algorithms;
        let platform = match pin {
            Some(_) => None,
            None => Some(
                rustls_platform_verifier::Verifier::new(provider)
                    .map_err(|e| Error::Config(format!("cannot use the OS certificate store: {e}")))?,
            ),
        };
        Ok(Self { pin, platform, algorithms, rejection: Mutex::new(None) })
    }

    /// The most recent rejection (if any), clearing it.
    pub(crate) fn take_rejection(&self) -> Option<CertProblem> {
        self.rejection.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn reject(&self, problem: CertProblem, reason: String) -> rustls::Error {
        tracing::debug!(
            fingerprint = %crate::format_fingerprint(&problem.sha256),
            changed = problem.changed,
            %reason,
            "server certificate rejected"
        );
        *self.rejection.lock().unwrap_or_else(PoisonError::into_inner) = Some(problem.clone());
        rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(Rejected { problem, reason }))))
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let sha256 = leaf_sha256(end_entity);
        match (self.pin, &self.platform) {
            (Some(pin), _) if pin == sha256 => Ok(ServerCertVerified::assertion()),
            (Some(_), _) => {
                Err(self
                    .reject(CertProblem { sha256, changed: true }, "does not match the pinned fingerprint".to_owned()))
            }
            (None, Some(platform)) => platform
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .map_err(|e| self.reject(CertProblem { sha256, changed: false }, e.to_string())),
            (None, None) => {
                Err(self.reject(CertProblem { sha256, changed: false }, "no trust store configured".to_owned()))
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Carried inside the `rustls::Error` so callers can find the rejected fingerprint.
#[derive(Debug, thiserror::Error)]
#[error("server certificate rejected: {reason}")]
struct Rejected {
    problem: CertProblem,
    reason: String,
}

/// SHA-256 of a certificate's DER encoding.
pub(crate) fn leaf_sha256(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

/// The first `Some` that `visit` returns for an error of the chain. Also descends into the
/// error an `io::Error` wraps (tokio-rustls puts the `rustls::Error` there), which
/// `io::Error::source()` skips, hence the explicit `get_ref`.
fn search_chain<T>(
    err: &(dyn StdError + 'static),
    visit: &impl Fn(&(dyn StdError + 'static)) -> Option<T>,
) -> Option<T> {
    let mut next = Some(err);
    while let Some(e) = next {
        if let Some(found) = visit(e) {
            return Some(found);
        }
        let wrapped = e.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref);
        if let Some(found) = wrapped.and_then(|inner| search_chain(inner, visit)) {
            return Some(found);
        }
        next = e.source();
    }
    None
}

/// The [`Rejected`] somewhere in an error chain.
fn find_rejection(err: &(dyn StdError + 'static)) -> Option<CertProblem> {
    search_chain(err, &as_rejection)
}

/// Whether an error chain contains a rustls certificate failure of any kind.
fn has_cert_failure(err: &(dyn StdError + 'static)) -> bool {
    search_chain(err, &|e| {
        matches!(e.downcast_ref::<rustls::Error>(), Some(rustls::Error::InvalidCertificate(_))).then_some(())
    })
    .is_some()
}

fn as_rejection(err: &(dyn StdError + 'static)) -> Option<CertProblem> {
    if let Some(rejected) = err.downcast_ref::<Rejected>() {
        return Some(rejected.problem.clone());
    }
    match err.downcast_ref::<rustls::Error>()? {
        rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(inner))) => {
            inner.downcast_ref::<Rejected>().map(|r| r.problem.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier(pin: Option<[u8; 32]>) -> PinningVerifier {
        PinningVerifier::new(pin, Arc::new(rustls::crypto::aws_lc_rs::default_provider())).unwrap()
    }

    fn verify(v: &PinningVerifier, der: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
        let cert = CertificateDer::from(der.to_vec());
        let name = ServerName::try_from("pve.example").unwrap();
        v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
    }

    #[test]
    fn pinned_leaf_is_accepted_without_other_checks() {
        let der = b"not even a real certificate";
        let v = verifier(Some(leaf_sha256(&CertificateDer::from(der.to_vec()))));
        assert!(verify(&v, der).is_ok());
        assert_eq!(v.take_rejection(), None);
    }

    #[test]
    fn other_leaf_is_reported_as_changed() {
        let v = verifier(Some([7; 32]));
        let err = verify(&v, b"another certificate").unwrap_err();
        let expected =
            CertProblem { sha256: leaf_sha256(&CertificateDer::from(b"another certificate".to_vec())), changed: true };
        assert_eq!(find_rejection(&err), Some(expected.clone()));
        assert_eq!(v.take_rejection(), Some(expected));
        assert_eq!(v.take_rejection(), None, "taking clears the record");
    }

    #[test]
    fn rejection_is_found_through_io_error_wrapping() {
        let v = verifier(Some([7; 32]));
        let tls_err = verify(&v, b"leaf").unwrap_err();
        // tokio-rustls wraps the rustls error like this; io::Error::source() would skip it.
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, tls_err);

        #[derive(Debug, thiserror::Error)]
        #[error("client error (Connect)")]
        struct Outer(#[source] std::io::Error);

        let problem = find_rejection(&Outer(io)).unwrap();
        assert!(problem.changed);
        assert_eq!(problem.sha256, leaf_sha256(&CertificateDer::from(b"leaf".to_vec())));
    }

    #[test]
    fn unrelated_errors_have_no_rejection() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(find_rejection(&io), None);
        let tls = rustls::Error::InvalidCertificate(CertificateError::Expired);
        assert_eq!(find_rejection(&tls), None);
    }

    fn tls_with_leftover_rejection() -> Tls {
        let tls = Tls::new(Some([7; 32])).unwrap();
        // A handshake that was rejected, but whose future was cancelled (or that ran as a
        // background pool connect) before anyone looked at its error.
        assert!(verify(&tls.verifier, b"some other node").is_err());
        tls
    }

    #[test]
    fn leftover_rejection_is_not_blamed_for_unrelated_errors() {
        let tls = tls_with_leftover_rejection();
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(tls.rejection_for(&refused), None);
        assert_eq!(tls.verifier.take_rejection(), None, "the record is cleared");
    }

    #[test]
    fn rejection_for_prefers_the_error_chain() {
        let tls = tls_with_leftover_rejection();
        let own = verify(&tls.verifier, b"this connection").unwrap_err();
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, own);
        let problem = tls.rejection_for(&io).unwrap();
        assert_eq!(problem.sha256, leaf_sha256(&CertificateDer::from(b"this connection".to_vec())));
    }

    #[test]
    fn record_only_backs_up_a_certificate_failure() {
        let tls = tls_with_leftover_rejection();
        // A certificate failure whose details did not survive the error wrapping.
        let bare = rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer);
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, bare);
        assert!(tls.rejection_for(&io).is_some_and(|p| p.changed));
        assert_eq!(tls.rejection_for(&io), None, "the record is used only once");
    }

    #[test]
    fn schemes_come_from_the_provider() {
        assert!(!verifier(None).supported_verify_schemes().is_empty());
    }
}
