//! TLS with leaf-certificate pinning (see crate docs), one backend per platform:
//!
//! * macOS: Secure Transport and the system trust store (Security.framework), so the app uses
//!   only the operating system's cryptography there.
//! * elsewhere: rustls (aws-lc-rs) with the platform verifier.
//!
//! Both expose the same [`Tls`] (one per [`crate::Client`], shared by the REST connections and
//! the console websocket) with `connect`, which returns a [`TlsStream`] only after the server
//! certificate passed the pin or the system trust check: nothing (request line, API token) can
//! be written to a connection whose certificate was not accepted.

use sha2::{Digest, Sha256};

#[cfg(target_os = "macos")]
mod apple;
#[cfg(not(target_os = "macos"))]
mod rustls_tls;

#[cfg(target_os = "macos")]
pub(crate) use apple::{Tls, TlsStream};
#[cfg(not(target_os = "macos"))]
pub(crate) use rustls_tls::{Tls, TlsStream};

/// SHA-256 of a certificate's DER encoding.
pub(crate) fn leaf_sha256(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// Logs a refused certificate (the fingerprint is not secret; the UI shows it).
fn log_rejection(problem: &crate::CertProblem, reason: &str) {
    tracing::debug!(
        fingerprint = %crate::format_fingerprint(&problem.sha256),
        changed = problem.changed,
        %reason,
        "server certificate rejected"
    );
}
