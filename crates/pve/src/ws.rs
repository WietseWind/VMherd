//! Websocket (binary messages) as an `AsyncRead + AsyncWrite` byte stream.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Buf, Bytes};
use futures_util::{Sink, Stream};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::header::{
    AUTHORIZATION, CONNECTION, HOST, HeaderMap, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE, USER_AGENT,
};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_websockets::{Message, WebSocketStream};
use url::{Position, Url};

use crate::error::error_chain;
use crate::net::{self, Server, Transport};
use crate::tls::Tls;
use crate::{Error, api};

const UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest single websocket message produced by one `poll_write`.
const MAX_WRITE_CHUNK: usize = 64 * 1024;
/// Largest error body read from a refused upgrade.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// RFC 6455: appended to `Sec-WebSocket-Key` for `Sec-WebSocket-Accept`.
const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The console byte stream (websocket underneath). Dropping it closes the connection.
///
/// Reading returns EOF when the session ends normally, which with pveproxy is usually a plain
/// socket shutdown rather than a websocket close frame (see `is_end_of_session`).
///
/// Logging: nothing here logs the upgrade request (API token, `vncticket`) or frame payloads
/// (keystrokes); as a safeguard for dependencies this crate also compiles `log` trace records
/// out (`log` features `max_level_debug` / `release_max_level_debug`), so they cannot reach a
/// log sink whatever `RUST_LOG` says.
pub struct VncStream {
    pub(crate) inner: Pin<Box<dyn Duplex>>,
}

pub(crate) trait Duplex: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> Duplex for T {}

impl std::fmt::Debug for VncStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VncStream").finish_non_exhaustive()
    }
}

impl AsyncRead for VncStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for VncStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

/// Where and how to open the console websocket.
pub(crate) struct Target<'a> {
    /// The `http(s)://.../vncwebsocket?port=..&vncticket=..` URL. Contains the ticket: never log it.
    pub(crate) url: Url,
    pub(crate) auth: &'a HeaderValue,
    pub(crate) tls: &'a Tls,
    /// The cluster the URL points to.
    pub(crate) server: &'a Server,
}

/// TCP connect, TLS (for `https`, with the certificate check), websocket upgrade.
pub(crate) async fn connect(target: Target<'_>) -> Result<VncStream, Error> {
    let key = new_key()?;
    let request = upgrade_request(&target.url, target.auth, &key)?;
    let transport = net::connect(target.server, target.tls).await?;
    upgrade(transport, request, &key, &target.server.label).await
}

/// A fresh `Sec-WebSocket-Key` (16 random bytes from the operating system, base64).
fn new_key() -> Result<String, Error> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| Error::WebSocket(format!("no random bytes for the handshake: {e}")))?;
    Ok(BASE64.encode(nonce))
}

/// The `Sec-WebSocket-Accept` the server must answer for `key`.
fn accept_key(key: &str) -> String {
    let mut sha1 = sha1_smol::Sha1::new();
    sha1.update(key.as_bytes());
    sha1.update(ACCEPT_GUID.as_bytes());
    BASE64.encode(sha1.digest().bytes())
}

/// The GET upgrade request (origin form) with token, sub-protocol and user agent headers.
fn upgrade_request(url: &Url, auth: &HeaderValue, key: &str) -> Result<Request<Empty<Bytes>>, Error> {
    // `auth` is marked sensitive: http / hyper never print it.
    Request::get(&url[Position::BeforePath..Position::AfterQuery])
        .header(HOST, &url[Position::BeforeHost..Position::AfterPort])
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_VERSION, "13")
        .header(SEC_WEBSOCKET_KEY, key)
        .header(SEC_WEBSOCKET_PROTOCOL, "binary")
        .header(AUTHORIZATION, auth.clone())
        .header(USER_AGENT, HeaderValue::from_static(crate::client::USER_AGENT))
        .body(Empty::new())
        // The error text never contains the URL (which carries the ticket).
        .map_err(|e| Error::WebSocket(format!("cannot build the upgrade request: {e}")))
}

async fn upgrade(
    transport: Transport,
    request: Request<Empty<Bytes>>,
    key: &str,
    label: &str,
) -> Result<VncStream, Error> {
    let network =
        |e: &(dyn std::error::Error + 'static)| Error::Network { host: label.to_owned(), detail: error_chain(e) };
    let handshake = async {
        // Title-case header names on the wire, as browsers (and tungstenite before) send them.
        let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
            .title_case_headers(true)
            .handshake(TokioIo::new(transport))
            .await
            .map_err(|e| network(&e))?;
        // Drives the connection until the 101 hands the socket over to `hyper::upgrade::on`.
        tokio::spawn(async move {
            if let Err(e) = connection.with_upgrades().await {
                tracing::debug!(error = %e, "console upgrade connection ended");
            }
        });
        let response = sender.send_request(request).await.map_err(|e| network(&e))?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = response.status();
            let body = Limited::new(response.into_body(), MAX_ERROR_BODY).collect().await;
            return Err(upgrade_refused(status, &body.map(|b| b.to_bytes()).unwrap_or_default()));
        }
        check_switch(response.headers(), key)?;
        hyper::upgrade::on(response).await.map_err(|e| network(&e))
    };
    let upgraded = tokio::time::timeout(UPGRADE_TIMEOUT, handshake)
        .await
        .map_err(|_| Error::WebSocket("the websocket handshake timed out".to_owned()))??;
    tracing::debug!("console websocket open");
    let ws = tokio_websockets::ClientBuilder::new().take_over(TokioIo::new(upgraded));
    Ok(VncStream { inner: Box::pin(WsBytes::new(ws)) })
}

/// A non-101 answer to the upgrade: the message from the JSON body, else the status text.
fn upgrade_refused(status: StatusCode, body: &[u8]) -> Error {
    let message = api::error_message(body, status.canonical_reason().unwrap_or_default());
    Error::Api { status: status.as_u16(), message }
}

/// RFC 6455 4.1: the 101 must upgrade to `websocket`, prove it read our key, and pick the
/// `binary` sub-protocol we asked for.
fn check_switch(headers: &HeaderMap, key: &str) -> Result<(), Error> {
    let header = |name| headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok()).unwrap_or_default();
    let fail = |what: &str| Err(Error::WebSocket(format!("invalid upgrade response: {what}")));
    if !header(UPGRADE).eq_ignore_ascii_case("websocket") {
        return fail("no Upgrade: websocket");
    }
    if !header(CONNECTION).split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade")) {
        return fail("no Connection: Upgrade");
    }
    if header(SEC_WEBSOCKET_ACCEPT) != accept_key(key) {
        return fail("wrong Sec-WebSocket-Accept");
    }
    if header(SEC_WEBSOCKET_PROTOCOL) != "binary" {
        return fail("the server did not accept the binary sub-protocol");
    }
    Ok(())
}

/// Binary websocket messages as a byte stream.
///
/// Reading: binary payloads are concatenated, text is ignored, ping/pong are skipped
/// (tokio-websockets answers pings itself), a close frame or the end of the session is EOF.
/// Writing: each `poll_write` becomes one binary message.
struct WsBytes<S> {
    ws: WebSocketStream<S>,
    /// Rest of the last binary message not yet handed to the reader.
    pending: Bytes,
    eof: bool,
}

impl<S> WsBytes<S> {
    fn new(ws: WebSocketStream<S>) -> Self {
        Self { ws, pending: Bytes::new(), eof: false }
    }
}

type WsError = tokio_websockets::Error;

fn to_io(err: WsError) -> io::Error {
    match err {
        WsError::Io(e) => e,
        other => io::Error::other(other),
    }
}

fn is_closed(err: &WsError) -> bool {
    matches!(err, WsError::AlreadyClosed)
}

/// Whether a read error only means that the server ended the session.
///
/// pveproxy never sends a websocket close frame: when the VNC backend goes away (VM stop /
/// reboot, vncproxy exit, pveproxy restart) or after our own close it just shuts the socket
/// down. A plain TCP EOF ends the message stream (no error); rustls reports a TLS session
/// without `close_notify` as `UnexpectedEof` (Secure Transport as a plain EOF).
fn is_end_of_session(err: &WsError) -> bool {
    match err {
        WsError::Io(e) => e.kind() == io::ErrorKind::UnexpectedEof,
        other => is_closed(other),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsBytes<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if !this.pending.is_empty() {
                let n = this.pending.len().min(buf.remaining());
                buf.put_slice(&this.pending[..n]);
                this.pending.advance(n);
                return Poll::Ready(Ok(()));
            }
            if this.eof || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.ws).poll_next(cx)) {
                Some(Ok(message)) if message.is_binary() => this.pending = Bytes::from(message.into_payload()),
                Some(Ok(message)) if message.is_close() => this.eof = true,
                None => this.eof = true,
                Some(Ok(_)) => {} // text, ping, pong
                Some(Err(e)) if is_end_of_session(&e) => {
                    tracing::debug!(reason = %e, "console websocket ended");
                    this.eof = true;
                }
                Some(Err(e)) => return Poll::Ready(Err(to_io(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsBytes<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut ws = Pin::new(&mut self.ws);
        ready!(ws.as_mut().poll_ready(cx)).map_err(to_io)?;
        let chunk = &buf[..buf.len().min(MAX_WRITE_CHUNK)];
        ws.start_send(Message::binary(Bytes::copy_from_slice(chunk))).map_err(to_io)?;
        Poll::Ready(Ok(chunk.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.ws).poll_flush(cx).map_err(to_io)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match ready!(Pin::new(&mut self.ws).poll_close(cx)) {
            Err(e) if !is_closed(&e) => Poll::Ready(Err(to_io(e))),
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio_websockets::proto::ProtocolError;

    use super::*;

    #[test]
    fn upgrade_request_headers() {
        let url = Url::parse(
            "https://pve:8006/api2/json/nodes/pve2/qemu/101/vncwebsocket?port=5900&vncticket=PVEVNC%3Aab%2Bc",
        )
        .unwrap();
        let mut auth = HeaderValue::from_static("PVEAPIToken=root@pam!t=secret");
        auth.set_sensitive(true);
        let request = upgrade_request(&url, &auth, "dGhlIHNhbXBsZSBub25jZQ==").unwrap();
        assert_eq!(
            request.uri().to_string(),
            "/api2/json/nodes/pve2/qemu/101/vncwebsocket?port=5900&vncticket=PVEVNC%3Aab%2Bc"
        );
        let headers = request.headers();
        assert_eq!(headers[HOST], "pve:8006");
        assert_eq!(headers[AUTHORIZATION], "PVEAPIToken=root@pam!t=secret");
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert_eq!(headers[SEC_WEBSOCKET_PROTOCOL], "binary");
        assert_eq!(headers[SEC_WEBSOCKET_KEY], "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(
            (&headers[UPGRADE], &headers[CONNECTION], &headers[SEC_WEBSOCKET_VERSION]),
            (
                &HeaderValue::from_static("websocket"),
                &HeaderValue::from_static("Upgrade"),
                &HeaderValue::from_static("13")
            )
        );
        assert!(headers[USER_AGENT].to_str().unwrap().starts_with("vmherd/"));
        let default_port = upgrade_request(&Url::parse("https://[fd00::1]/x").unwrap(), &auth, "k").unwrap();
        assert_eq!(default_port.headers()[HOST], "[fd00::1]");
    }

    #[test]
    fn keys_are_random_and_accepted_per_rfc() {
        // RFC 6455, section 1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let (a, b) = (new_key().unwrap(), new_key().unwrap());
        assert_ne!(a, b);
        assert_eq!(BASE64.decode(&a).unwrap().len(), 16);
    }

    #[test]
    fn switch_response_is_checked() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let good = [
            (UPGRADE, "WebSocket"),
            (CONNECTION, "keep-alive, Upgrade"),
            (SEC_WEBSOCKET_ACCEPT, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            (SEC_WEBSOCKET_PROTOCOL, "binary"),
        ];
        let headers = |skip: Option<usize>, accept: &'static str| {
            let mut map = HeaderMap::new();
            for (i, (name, value)) in good.iter().enumerate() {
                if Some(i) != skip {
                    let value = if *name == SEC_WEBSOCKET_ACCEPT { accept } else { value };
                    map.insert(name.clone(), HeaderValue::from_static(value));
                }
            }
            map
        };
        assert!(check_switch(&headers(None, good[2].1), key).is_ok());
        for skip in 0..good.len() {
            assert!(matches!(check_switch(&headers(Some(skip), good[2].1), key), Err(Error::WebSocket(_))), "{skip}");
        }
        assert!(check_switch(&headers(None, "AAAAAAAAAAAAAAAAAAAAAAAAAAA="), key).is_err());
    }

    #[test]
    fn http_upgrade_failure_maps_to_api_error() {
        let body = br#"{"data":null,"message":"permission denied - invalid PVE ticket\n"}"#;
        match upgrade_refused(StatusCode::UNAUTHORIZED, body) {
            Error::Api { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "permission denied - invalid PVE ticket");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            upgrade_refused(StatusCode::FORBIDDEN, b""),
            Error::Api { status: 403, message } if message == "Forbidden"
        ));
    }

    #[test]
    fn socket_shutdown_ends_the_session() {
        let tls_eof = WsError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        ));
        for err in [tls_eof, WsError::AlreadyClosed] {
            assert!(is_end_of_session(&err), "{err}");
        }
        let reset = WsError::Io(io::ErrorKind::ConnectionReset.into());
        assert!(!is_end_of_session(&reset), "a reset stays an error");
        let bad = WsError::Protocol(ProtocolError::InvalidOpcode);
        assert!(!is_end_of_session(&bad));
    }

    #[test]
    fn trace_logs_are_compiled_out() {
        // A safeguard: no dependency may trace-log the upgrade request (token, ticket) or frames.
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug);
    }
}
