//! Websocket (binary messages) as an `AsyncRead + AsyncWrite` byte stream.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf, Bytes};
use futures_util::{Sink, Stream};
use reqwest::header::HeaderValue;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL, USER_AGENT};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use url::{Host, Url};

use crate::error::error_chain;
use crate::tls::Tls;
use crate::{Error, api};

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest single websocket message produced by one `poll_write`.
const MAX_WRITE_CHUNK: usize = 64 * 1024;

/// The console byte stream (websocket underneath). Dropping it closes the connection.
///
/// Reading returns EOF when the session ends normally, which with pveproxy is usually a plain
/// socket shutdown rather than a websocket close frame (see `is_end_of_session`).
///
/// Logging: tungstenite logs the raw upgrade request (API token, `vncticket`) and every frame
/// payload (keystrokes) through the `log` crate at trace level. This crate compiles `log`
/// trace records out (`log` features `max_level_debug` / `release_max_level_debug`), so they
/// cannot reach a log sink whatever `RUST_LOG` says.
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
    /// `host:port`, for error messages.
    pub(crate) host: &'a str,
}

/// TCP connect, TLS (for `https`), websocket upgrade.
pub(crate) async fn connect(target: Target<'_>) -> Result<VncStream, Error> {
    let secure = match target.url.scheme() {
        "https" => true,
        "http" => false,
        other => return Err(Error::Config(format!("unsupported URL scheme {other}"))),
    };
    let (host, port) = host_and_port(&target.url)?;
    let request = upgrade_request(&target.url, secure, target.auth)?;
    let tcp = connect_tcp(&host, port, target.host).await?;
    if secure {
        let tls = tls_handshake(tcp, &host, &target).await?;
        upgrade(tls, request, target.host).await
    } else {
        upgrade(tcp, request, target.host).await
    }
}

fn host_and_port(url: &Url) -> Result<(Host<String>, u16), Error> {
    let host = url.host().map(|h| h.to_owned()).ok_or_else(|| Error::Config("the URL has no host".to_owned()))?;
    let port = url.port_or_known_default().ok_or_else(|| Error::Config("the URL has no port".to_owned()))?;
    Ok((host, port))
}

/// The GET upgrade request for `ws(s)://...` with token, sub-protocol and user agent headers.
fn upgrade_request(url: &Url, secure: bool, auth: &HeaderValue) -> Result<Request, Error> {
    let mut ws_url = url.clone();
    ws_url
        .set_scheme(if secure { "wss" } else { "ws" })
        .map_err(|()| Error::Config("cannot build the websocket URL".to_owned()))?;
    // tungstenite's error texts do not include the URL (which carries the ticket).
    let mut request = ws_url.as_str().into_client_request().map_err(|e| Error::WebSocket(e.to_string()))?;
    let headers = request.headers_mut();
    // `auth` is marked sensitive, but that does not help here: tungstenite serializes the
    // request itself and trace-logs it verbatim (token and ticket). Those `log` records are
    // compiled out, see `VncStream`.
    headers.insert(AUTHORIZATION, auth.clone());
    headers.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("binary"));
    headers.insert(USER_AGENT, HeaderValue::from_static(crate::client::USER_AGENT));
    Ok(request)
}

async fn connect_tcp(host: &Host<String>, port: u16, label: &str) -> Result<TcpStream, Error> {
    let network = |detail: String| Error::Network { host: label.to_owned(), detail };
    let connecting = async {
        match host {
            Host::Domain(domain) => TcpStream::connect((domain.as_str(), port)).await,
            Host::Ipv4(ip) => TcpStream::connect(SocketAddr::new(IpAddr::V4(*ip), port)).await,
            Host::Ipv6(ip) => TcpStream::connect(SocketAddr::new(IpAddr::V6(*ip), port)).await,
        }
    };
    let tcp = tokio::time::timeout(TCP_CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| network("TCP connect timed out".to_owned()))?
        .map_err(|e| network(error_chain(&e)))?;
    tcp.set_nodelay(true).map_err(|e| network(error_chain(&e)))?;
    Ok(tcp)
}

async fn tls_handshake(
    tcp: TcpStream,
    host: &Host<String>,
    target: &Target<'_>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, Error> {
    let network = |detail: String| Error::Network { host: target.host.to_owned(), detail };
    let server_name = match host {
        Host::Domain(domain) => ServerName::try_from(domain.clone())
            .map_err(|e| Error::Config(format!("invalid host name {domain:?}: {e}")))?,
        Host::Ipv4(ip) => ServerName::IpAddress(IpAddr::V4(*ip).into()),
        Host::Ipv6(ip) => ServerName::IpAddress(IpAddr::V6(*ip).into()),
    };
    let connector = tokio_rustls::TlsConnector::from(target.tls.config.clone());
    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp)).await {
        Err(_) => Err(network("TLS handshake timed out".to_owned())),
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(match target.tls.rejection_for(&e) {
            Some(problem) => Error::UntrustedCert(problem),
            None => network(error_chain(&e)),
        }),
    }
}

async fn upgrade<S>(stream: S, request: Request, label: &str) -> Result<VncStream, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Write every message right away: console traffic is small and latency sensitive.
    let config = WebSocketConfig::default().write_buffer_size(0);
    let handshake = tokio_tungstenite::client_async_with_config(request, stream, Some(config));
    let (ws, _response) = tokio::time::timeout(UPGRADE_TIMEOUT, handshake)
        .await
        .map_err(|_| Error::WebSocket("the websocket handshake timed out".to_owned()))?
        .map_err(|e| upgrade_error(e, label))?;
    tracing::debug!("console websocket open");
    Ok(VncStream { inner: Box::pin(WsBytes::new(ws)) })
}

fn upgrade_error(err: tungstenite::Error, label: &str) -> Error {
    match err {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            let body = response.body().as_deref().unwrap_or_default();
            let message = api::error_message(body, status.canonical_reason().unwrap_or_default());
            Error::Api { status: status.as_u16(), message }
        }
        tungstenite::Error::Io(e) => Error::Network { host: label.to_owned(), detail: error_chain(&e) },
        other => Error::WebSocket(other.to_string()),
    }
}

/// Binary websocket messages as a byte stream.
///
/// Reading: binary payloads are concatenated, text is ignored, ping/pong are skipped
/// (tungstenite answers pings itself), a close frame or the end of the session is EOF.
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

fn to_io(err: tungstenite::Error) -> io::Error {
    match err {
        tungstenite::Error::Io(e) => e,
        other => io::Error::other(other),
    }
}

fn is_closed(err: &tungstenite::Error) -> bool {
    matches!(err, tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed)
}

/// Whether a read error only means that the server ended the session.
///
/// pveproxy never sends a websocket close frame: when the VNC backend goes away (VM stop /
/// reboot, vncproxy exit, pveproxy restart) or after our own close it just shuts the socket
/// down. tungstenite reports that as `ResetWithoutClosingHandshake` (TCP EOF without a close
/// handshake), rustls as `UnexpectedEof` (TCP EOF without a TLS `close_notify`).
fn is_end_of_session(err: &tungstenite::Error) -> bool {
    match err {
        tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        tungstenite::Error::Io(e) => e.kind() == io::ErrorKind::UnexpectedEof,
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
                Some(Ok(Message::Binary(data))) => this.pending = data,
                Some(Ok(Message::Close(_))) | None => this.eof = true,
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
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
        ws.start_send(Message::Binary(Bytes::copy_from_slice(chunk))).map_err(to_io)?;
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
    use super::*;

    #[test]
    fn upgrade_request_headers() {
        let url = Url::parse(
            "https://pve:8006/api2/json/nodes/pve2/qemu/101/vncwebsocket?port=5900&vncticket=PVEVNC%3Aab%2Bc",
        )
        .unwrap();
        let mut auth = HeaderValue::from_static("PVEAPIToken=root@pam!t=secret");
        auth.set_sensitive(true);
        let request = upgrade_request(&url, true, &auth).unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://pve:8006/api2/json/nodes/pve2/qemu/101/vncwebsocket?port=5900&vncticket=PVEVNC%3Aab%2Bc"
        );
        let headers = request.headers();
        assert_eq!(headers[AUTHORIZATION], "PVEAPIToken=root@pam!t=secret");
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert_eq!(headers[SEC_WEBSOCKET_PROTOCOL], "binary");
        assert!(headers[USER_AGENT].to_str().unwrap().starts_with("vmherd/"));
        let plain = upgrade_request(&Url::parse("http://127.0.0.1:18006/x").unwrap(), false, &auth).unwrap();
        assert_eq!(plain.uri().scheme_str(), Some("ws"));
    }

    #[test]
    fn http_upgrade_failure_maps_to_api_error() {
        let response = tungstenite::http::Response::builder()
            .status(401)
            .body(Some(br#"{"data":null,"message":"permission denied - invalid PVE ticket\n"}"#.to_vec()))
            .unwrap();
        match upgrade_error(tungstenite::Error::Http(Box::new(response)), "pve:8006") {
            Error::Api { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "permission denied - invalid PVE ticket");
            }
            other => panic!("unexpected {other:?}"),
        }
        let empty = tungstenite::http::Response::builder().status(403).body(None).unwrap();
        assert!(matches!(
            upgrade_error(tungstenite::Error::Http(Box::new(empty)), "pve:8006"),
            Error::Api { status: 403, message } if message == "Forbidden"
        ));
    }

    #[test]
    fn socket_shutdown_ends_the_session() {
        let reset = tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake);
        let tls_eof = tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed connection without sending TLS close_notify",
        ));
        for err in [reset, tls_eof, tungstenite::Error::ConnectionClosed] {
            assert!(is_end_of_session(&err), "{err}");
        }
        let reset = tungstenite::Error::Io(io::ErrorKind::ConnectionReset.into());
        assert!(!is_end_of_session(&reset), "a reset stays an error");
        let bad = tungstenite::Error::Protocol(ProtocolError::ReceivedAfterClosing);
        assert!(!is_end_of_session(&bad));
    }

    #[test]
    fn trace_logs_are_compiled_out() {
        // tungstenite trace-logs the upgrade request (token, ticket) and frame payloads.
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug);
    }
}
