/// A certificate the client refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertProblem {
    /// SHA-256 of the presented leaf certificate (DER).
    pub sha256: [u8; 32],
    /// `true`: a fingerprint was pinned and the server now presents a different certificate.
    pub changed: bool,
}

/// Everything that can go wrong talking to a cluster.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The TLS certificate was refused; the UI may offer to pin [`CertProblem::sha256`].
    #[error("{}", if .0.changed { "the server certificate CHANGED since it was trusted" } else { "the server certificate is not trusted" })]
    UntrustedCert(CertProblem),
    /// Non-2xx from the API; `message` from the JSON body (`message` / `errors`) or the HTTP reason.
    #[error("Proxmox {status}: {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Human readable reason as reported by the server.
        message: String,
    },
    /// Transport failure (DNS, TCP, TLS, timeout, connection reset, ...).
    #[error("cannot reach {host}: {detail}")]
    Network {
        /// `host:port` of the cluster.
        host: String,
        /// The full error chain.
        detail: String,
    },
    /// The console websocket failed (handshake or protocol) after TCP/TLS were established.
    #[error("console websocket: {0}")]
    WebSocket(String),
    /// The server answered with JSON of an unexpected shape.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// Invalid endpoint or argument, detected locally (nothing was sent).
    #[error("{0}")]
    Config(String),
}

/// `outer: inner: innermost`, skipping messages already contained in the text so far
/// (hyper / reqwest like to repeat their inner error).
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let msg = e.to_string();
        if !msg.is_empty() && !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = e.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{msg}")]
    struct Layer {
        msg: &'static str,
        #[source]
        inner: Option<Box<Layer>>,
    }

    fn layer(msg: &'static str, inner: Option<Layer>) -> Layer {
        Layer { msg, inner: inner.map(Box::new) }
    }

    #[test]
    fn chain_joins_and_dedupes() {
        let err = layer(
            "error sending request",
            Some(layer(
                "client error (Connect)",
                Some(layer("Connect", Some(layer("Connection refused (os error 61)", None)))),
            )),
        );
        assert_eq!(
            error_chain(&err),
            "error sending request: client error (Connect): Connection refused (os error 61)"
        );
    }

    #[test]
    fn display_texts() {
        let problem = CertProblem { sha256: [0; 32], changed: true };
        assert!(Error::UntrustedCert(problem).to_string().contains("CHANGED"));
        let api = Error::Api { status: 401, message: "invalid token value!".into() };
        assert_eq!(api.to_string(), "Proxmox 401: invalid token value!");
    }
}
