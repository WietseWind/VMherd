//! Certificate pinning and the console websocket against local TLS servers.

mod common;

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use pve::{CertProblem, Client, Error, Version, VmKind, VmRef, VncProxy};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;

use common::{Reply, endpoint, expected_auth, self_signed, serve};

const VERSION: &str = r#"{"version":"8.4.14","release":"8.4","repoid":"x"}"#;

async fn version_server() -> (String, [u8; 32], common::Log) {
    let (config, sha256) = self_signed();
    let (addr, log) = serve(Some(config), |req| match req.path() {
        "/api2/json/version" => Reply::data(VERSION),
        _ => Reply::json(404, r#"{"data":null}"#),
    })
    .await;
    (format!("https://127.0.0.1:{}", addr.port()), sha256, log)
}

#[tokio::test]
async fn untrusted_self_signed_cert_reports_its_fingerprint() {
    let (url, sha256, log) = version_server().await;
    let client = Client::new(&endpoint(&url, None)).unwrap();
    match client.version().await {
        Err(Error::UntrustedCert(problem)) => assert_eq!(problem, CertProblem { sha256, changed: false }),
        other => panic!("expected UntrustedCert, got {other:?}"),
    }
    assert!(log.lock().unwrap().is_empty(), "no request may be sent over an untrusted connection");
}

#[tokio::test]
async fn pinned_cert_is_accepted() {
    let (url, sha256, log) = version_server().await;
    let client = Client::new(&endpoint(&url, Some(sha256))).unwrap();
    let version = client.version().await.unwrap();
    assert_eq!(version, Version { version: "8.4.14".into(), release: "8.4".into(), repoid: "x".into() });
    let requests = log.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("authorization"), Some(expected_auth().as_str()));
}

#[tokio::test]
async fn other_pin_reports_a_changed_cert() {
    let (url, sha256, _log) = version_server().await;
    let client = Client::new(&endpoint(&url, Some([0xAB; 32]))).unwrap();
    match client.version().await {
        Err(Error::UntrustedCert(problem)) => assert_eq!(problem, CertProblem { sha256, changed: true }),
        other => panic!("expected UntrustedCert, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_rejections_are_all_reported() {
    let (url, sha256, _log) = version_server().await;
    let client = Client::new(&endpoint(&url, None)).unwrap();
    let results = futures_util::future::join_all((0..6).map(|_| client.version())).await;
    for result in results {
        assert!(matches!(result, Err(Error::UntrustedCert(ref p)) if p.sha256 == sha256 && !p.changed), "{result:?}");
    }
    // A later, unrelated failure must not be blamed on the certificate.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let other = Client::new(&endpoint(&format!("https://127.0.0.1:{}", closed.port()), None)).unwrap();
    assert!(matches!(other.version().await, Err(Error::Network { .. })));
    assert!(matches!(client.vms().await, Err(Error::UntrustedCert(_))));
}

/// One client: after a certificate rejection, a later transport failure is still a network
/// error, not the certificate's fault.
#[tokio::test]
async fn transport_failure_after_a_rejection_is_a_network_error() {
    let (config, sha256) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // First connection: a TLS handshake the client refuses.
        let (tcp, _) = listener.accept().await.unwrap();
        let _ = TlsAcceptor::from(config).accept(tcp).await;
        // Then every connection is aborted before TLS starts.
        while let Ok((tcp, _)) = listener.accept().await {
            drop(tcp);
        }
    });
    let client = Client::new(&endpoint(&format!("https://127.0.0.1:{port}"), Some([0xAB; 32]))).unwrap();
    match client.version().await {
        Err(Error::UntrustedCert(problem)) => assert_eq!(problem, CertProblem { sha256, changed: true }),
        other => panic!("expected UntrustedCert, got {other:?}"),
    }
    let later = client.version().await;
    assert!(matches!(later, Err(Error::Network { .. })), "{later:?}");
}

/// What the websocket server saw in the upgrade request.
#[derive(Debug, Default)]
struct Seen {
    path: String,
    query: String,
    auth: String,
    protocol: String,
    user_agent: String,
}

fn header(req: &Request, name: &str) -> String {
    req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
}

/// A TLS websocket server for one connection running a scripted exchange:
/// sends "RFB 003.008\n" split over three binary messages (with a ping and a text message in
/// between), collects 12 bytes, sends back "echo:" + those bytes in two messages, closes.
async fn console_server() -> (u16, [u8; 32], Arc<Mutex<Seen>>, JoinHandle<usize>) {
    let (config, sha256) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let record = seen.clone();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let Ok(tls) = TlsAcceptor::from(config).accept(tcp).await else {
            return 0; // the client refused the certificate
        };
        #[allow(clippy::result_large_err)] // the callback signature is tungstenite's
        let callback = move |req: &Request, mut resp: Response| {
            *record.lock().unwrap() = Seen {
                path: req.uri().path().to_owned(),
                query: req.uri().query().unwrap_or_default().to_owned(),
                auth: header(req, "authorization"),
                protocol: header(req, "sec-websocket-protocol"),
                user_agent: header(req, "user-agent"),
            };
            resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("binary"));
            Ok(resp)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(tls, callback).await.unwrap();
        for msg in [
            Message::Binary(Bytes::from_static(b"RF")),
            Message::Ping(Bytes::from_static(b"are you there")),
            Message::Binary(Bytes::from_static(b"B 003")),
            Message::text("ignored"),
            Message::Binary(Bytes::from_static(b".008\n")),
        ] {
            ws.send(msg).await.unwrap();
        }
        let (mut got, mut messages) = (Vec::new(), 0);
        while got.len() < 12 {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(data) => {
                    got.extend_from_slice(&data);
                    messages += 1;
                }
                Message::Pong(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        ws.send(Message::Binary([b"echo:".as_slice(), &got[..4]].concat().into())).await.unwrap();
        ws.send(Message::Binary(Bytes::copy_from_slice(&got[4..]))).await.unwrap();
        ws.close(None).await.unwrap();
        messages
    });
    (port, sha256, seen, task)
}

fn console_target() -> (VmRef, VncProxy) {
    let vm = VmRef { vmid: 9001, node: "pve1".into(), kind: VmKind::Qemu };
    let proxy = VncProxy { port: 5901, ticket: "PVEVNC:6720AB12::a+b/c=".into(), password: "pw123456".into() };
    (vm, proxy)
}

#[tokio::test]
async fn websocket_is_a_byte_stream_both_ways() {
    let (port, sha256, seen, server) = console_server().await;
    let client = Client::new(&endpoint(&format!("https://127.0.0.1:{port}"), Some(sha256))).unwrap();
    let (vm, proxy) = console_target();
    let mut stream = client.vnc_connect(&vm, &proxy).await.unwrap();

    let mut banner = [0u8; 12];
    stream.read_exact(&mut banner).await.unwrap();
    assert_eq!(&banner, b"RFB 003.008\n");

    stream.write_all(b"RFB 0").await.unwrap();
    stream.write_all(b"03.008\n").await.unwrap();
    stream.flush().await.unwrap();

    let mut echo = [0u8; 17];
    stream.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"echo:RFB 003.008\n");

    let mut rest = Vec::new();
    assert_eq!(stream.read_to_end(&mut rest).await.unwrap(), 0, "close frame => EOF");
    assert_eq!(server.await.unwrap(), 2, "one websocket message per write");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.path, "/api2/json/nodes/pve1/qemu/9001/vncwebsocket");
    assert_eq!(seen.query, "port=5901&vncticket=PVEVNC%3A6720AB12%3A%3Aa%2Bb%2Fc%3D");
    assert_eq!(seen.auth, expected_auth());
    assert_eq!(seen.protocol, "binary");
    assert!(seen.user_agent.starts_with("vmherd/"));
}

#[tokio::test]
async fn websocket_refuses_an_untrusted_cert() {
    let (port, sha256, _seen, server) = console_server().await;
    let client = Client::new(&endpoint(&format!("https://127.0.0.1:{port}"), None)).unwrap();
    let (vm, proxy) = console_target();
    match client.vnc_connect(&vm, &proxy).await {
        Err(Error::UntrustedCert(problem)) => assert_eq!(problem, CertProblem { sha256, changed: false }),
        other => panic!("expected UntrustedCert, got {other:?}"),
    }
    assert_eq!(server.await.unwrap(), 0, "the handshake must not complete");
}

/// Answers the upgrade with the `binary` sub-protocol, like pveproxy.
#[allow(clippy::result_large_err)] // the callback signature is tungstenite's
fn binary_protocol(_req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("binary"));
    Ok(resp)
}

/// Like pveproxy when a console ends (VM stopped, vncproxy gone): sends the RFB banner, then
/// shuts the socket down without a websocket close frame and, over TLS, without `close_notify`.
async fn vanishing_console_server(tls: bool) -> (pve::Endpoint, JoinHandle<()>) {
    async fn banner_then_vanish<S: AsyncRead + AsyncWrite + Unpin>(stream: S) {
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, binary_protocol).await.unwrap();
        ws.send(Message::Binary(Bytes::from_static(b"RFB 003.008\n"))).await.unwrap();
        // Dropping `ws` just closes the socket.
    }
    let (config, sha256) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        if tls {
            banner_then_vanish(TlsAcceptor::from(config).accept(tcp).await.unwrap()).await;
        } else {
            banner_then_vanish(tcp).await;
        }
    });
    let scheme = if tls { "https" } else { "http" };
    (endpoint(&format!("{scheme}://127.0.0.1:{port}"), Some(sha256)), task)
}

#[tokio::test]
async fn console_end_without_close_frame_is_eof() {
    for tls in [true, false] {
        let (endpoint, server) = vanishing_console_server(tls).await;
        let client = Client::new(&endpoint).unwrap();
        let (vm, proxy) = console_target();
        let mut stream = client.vnc_connect(&vm, &proxy).await.unwrap();
        let mut got = Vec::new();
        let read = stream.read_to_end(&mut got).await;
        assert!(read.is_ok(), "tls={tls}: {read:?}");
        assert_eq!(got, b"RFB 003.008\n", "tls={tls}");
        server.await.unwrap();
    }
}

/// What one connection to the [`recording_server`] delivered.
#[derive(Debug)]
struct Received {
    /// The TLS handshake completed on the server side.
    handshake: bool,
    /// Application bytes received after it (request line, headers, ...).
    bytes: Vec<u8>,
}

/// A TLS server recording, per connection, whether the handshake completed and every
/// application byte that arrived until the end of the request head; answers 403.
async fn recording_server() -> (String, [u8; 32], Arc<Mutex<Vec<Received>>>) {
    let (config, sha256) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Arc<Mutex<Vec<Received>>> = Arc::default();
    let record = log.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, record) = (TlsAcceptor::from(config.clone()), record.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    record.lock().unwrap().push(Received { handshake: false, bytes: Vec::new() });
                    return;
                };
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => bytes.extend_from_slice(&buf[..n]),
                    }
                }
                record.lock().unwrap().push(Received { handshake: true, bytes });
                let reply = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = tls.write_all(reply).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (format!("https://127.0.0.1:{port}"), sha256, log)
}

/// Waits until the server logged `n` connections.
async fn received(log: &Arc<Mutex<Vec<Received>>>, n: usize) -> Vec<Received> {
    for _ in 0..200 {
        if log.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    std::mem::take(&mut *log.lock().unwrap())
}

fn contains_ignore_case(haystack: &[u8], needle: &str) -> bool {
    haystack.to_ascii_lowercase().windows(needle.len()).any(|w| w == needle.to_ascii_lowercase().as_bytes())
}

/// The recorder itself: with the right pin, the request (token included) does arrive.
#[tokio::test]
async fn recorder_sees_requests_over_an_accepted_connection() {
    let (url, sha256, log) = recording_server().await;
    let client = Client::new(&endpoint(&url, Some(sha256))).unwrap();
    assert!(matches!(client.version().await, Err(Error::Api { status: 403, .. })));
    let (vm, proxy) = console_target();
    assert!(matches!(client.vnc_connect(&vm, &proxy).await, Err(Error::Api { status: 403, .. })));
    let seen = received(&log, 2).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    for connection in &seen {
        assert!(connection.handshake);
        assert!(contains_ignore_case(&connection.bytes, &format!("authorization: {}", expected_auth())));
    }
    assert!(seen.iter().any(|c| c.bytes.starts_with(b"GET /api2/json/version ")));
    assert!(seen.iter().any(|c| c.bytes.starts_with(b"GET /api2/json/nodes/pve1/qemu/9001/vncwebsocket?")));
}

/// A refused certificate (pin mismatch or untrusted) ends the connection before the client
/// writes a single application byte: no request line, no token, no websocket upgrade.
#[tokio::test]
async fn refused_certificates_get_no_request_bytes() {
    let (url, sha256, log) = recording_server().await;
    for (pin, changed) in [(Some([0xAB; 32]), true), (None, false)] {
        let client = Client::new(&endpoint(&url, pin)).unwrap();
        let expected = CertProblem { sha256, changed };
        match client.version().await {
            Err(Error::UntrustedCert(problem)) => assert_eq!(problem, expected),
            other => panic!("REST, pin {pin:?}: expected UntrustedCert, got {other:?}"),
        }
        match client.vnc_proxy(&console_target().0).await {
            Err(Error::UntrustedCert(problem)) => assert_eq!(problem, expected),
            other => panic!("POST, pin {pin:?}: expected UntrustedCert, got {other:?}"),
        }
        let (vm, proxy) = console_target();
        match client.vnc_connect(&vm, &proxy).await {
            Err(Error::UntrustedCert(problem)) => assert_eq!(problem, expected),
            other => panic!("websocket, pin {pin:?}: expected UntrustedCert, got {other:?}"),
        }
        let seen = received(&log, 3).await;
        assert!(!seen.is_empty(), "pin {pin:?}: the server saw no connection at all");
        for connection in &seen {
            assert!(!connection.handshake, "pin {pin:?}: handshake completed: {connection:?}");
            assert!(connection.bytes.is_empty(), "pin {pin:?}: application bytes sent: {connection:?}");
        }
    }
}
