//! Proxmox VE API client for VMherd.
//!
//! * API token auth (`Authorization: PVEAPIToken=<user@realm!name>=<secret>`).
//! * TLS with certificate pinning (trust on first use): a pinned SHA-256 fingerprint of the
//!   server's leaf certificate is accepted as is (like SSH host keys, no name/expiry checks);
//!   without a pin the OS trust store decides. When verification fails, the error carries the
//!   presented fingerprint so the UI can ask the user to trust (pin) it.
//! * `http://` base URLs are allowed (no TLS) for tests against a mock.
//! * The noVNC websocket (`vncwebsocket`) is exposed as a plain `AsyncRead + AsyncWrite`
//!   byte stream, ready for `rfb::run`.

mod api;
mod client;
mod error;
mod tls;
mod types;
mod ws;

pub use client::Client;
pub use error::{CertProblem, Error};
pub use types::{Endpoint, GuestIps, PowerAction, TaskStatus, Version, VmKind, VmRef, VmResource, VncProxy};
pub use ws::VncStream;

/// Check an API token ID (`user@realm!tokenname`) the way [`Client::new`] does. The message never
/// repeats the input (people paste `id=secret` into the ID field).
pub fn check_token_id(token_id: &str) -> Result<(), String> {
    client::validate_token_id(token_id).map_err(|e| e.to_string())
}

/// Hex SHA-256 fingerprint in the usual `AB:CD:...` form.
pub fn format_fingerprint(sha256: &[u8; 32]) -> String {
    sha256.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Parse `AB:CD:...` / `abcd...` (64 hex digits; `:` and whitespace, including non-breaking
/// spaces, are ignored). `None` for anything else; never panics.
pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let digits: Vec<u8> =
        s.chars().filter(|&c| c != ':' && !c.is_whitespace()).map(hex_digit).collect::<Option<_>>()?;
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = (high << 4) | low;
    }
    Some(out)
}

/// The value of one hex digit; `to_digit` accepts only ASCII `0-9a-fA-F` (no signs).
fn hex_digit(c: char) -> Option<u8> {
    c.to_digit(16).and_then(|d| u8::try_from(d).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_roundtrip() {
        let fp: [u8; 32] = core::array::from_fn(|i| i as u8 * 7);
        let s = format_fingerprint(&fp);
        assert_eq!(s.len(), 32 * 3 - 1);
        assert_eq!(parse_fingerprint(&s), Some(fp));
        assert_eq!(parse_fingerprint(&s.replace(':', "").to_lowercase()), Some(fp));
        assert_eq!(parse_fingerprint("abc"), None);
        assert_eq!(parse_fingerprint(&format!(" {s}\n")), Some(fp));
    }

    #[test]
    fn fingerprint_rejects_odd_input_without_panicking() {
        // 64 bytes, but a two-byte character where a hex pair would be sliced.
        let nbsp_inside = format!("A\u{a0}{}", "B".repeat(61));
        assert_eq!(nbsp_inside.len(), 64);
        assert_eq!(parse_fingerprint(&nbsp_inside), None);
        let signs = "+F".repeat(32);
        assert_eq!(parse_fingerprint(&signs), None);
        let fullwidth = "\u{ff21}".repeat(64); // fullwidth 'A'
        assert_eq!(parse_fingerprint(&fullwidth), None);
        assert_eq!(parse_fingerprint(&"g".repeat(64)), None);
        assert_eq!(parse_fingerprint(&"a".repeat(66)), None);
        assert_eq!(parse_fingerprint(""), None);
    }

    #[test]
    fn fingerprint_ignores_non_breaking_spaces() {
        let fp = [0xAB; 32];
        let pasted = format_fingerprint(&fp).replace(':', "\u{a0}");
        assert_eq!(parse_fingerprint(&pasted), Some(fp));
    }
}
