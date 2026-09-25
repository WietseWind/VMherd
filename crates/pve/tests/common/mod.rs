//! Tiny local HTTP(S) servers for the integration tests.
#![allow(dead_code)] // not every test binary uses every helper

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub const TOKEN_ID: &str = "root@pam!grid-test";
pub const TOKEN_SECRET: &str = "8e2c1f0a-5b4d-4c3e-9f1a-0123456789ab";

/// `PVEAPIToken=...` as the client must send it.
pub fn expected_auth() -> String {
    format!("PVEAPIToken={TOKEN_ID}={TOKEN_SECRET}")
}

pub fn endpoint(url: &str, pin: Option<[u8; 32]>) -> pve::Endpoint {
    pve::Endpoint {
        url: url::Url::parse(url).unwrap(),
        token_id: TOKEN_ID.to_owned(),
        token_secret: TOKEN_SECRET.to_owned(),
        pinned_sha256: pin,
    }
}

/// A fresh self-signed certificate: the server config and the SHA-256 of the leaf DER.
pub fn self_signed() -> (Arc<rustls::ServerConfig>, [u8; 32]) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).unwrap();
    let der: CertificateDer<'static> = cert.der().clone();
    let sha256: [u8; 32] = Sha256::digest(der.as_ref()).into();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der], key)
        .unwrap();
    (Arc::new(config), sha256)
}

/// One parsed HTTP request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn path(&self) -> &str {
        self.target.split_once('?').map_or(self.target.as_str(), |(p, _)| p)
    }

    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }
}

/// What the handler answers.
pub struct Reply {
    pub status: u16,
    pub reason: String,
    pub body: String,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Self {
        let reason = if status == 200 { "OK" } else { "Error" };
        Self { status, reason: reason.to_owned(), body: body.to_owned() }
    }

    pub fn data(json: &str) -> Self {
        Self::json(200, &format!(r#"{{"data":{json}}}"#))
    }
}

pub type Log = Arc<Mutex<Vec<Request>>>;

/// Serves `handler` on 127.0.0.1 (TLS when `tls` is given), one request per connection.
/// Returns the address and the log of requests received.
pub async fn serve<F>(tls: Option<Arc<rustls::ServerConfig>>, handler: F) -> (SocketAddr, Log)
where
    F: Fn(&Request) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Arc::default();
    let handler = Arc::new(handler);
    let acceptor = tls.map(TlsAcceptor::from);
    let server_log = log.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (handler, log, acceptor) = (handler.clone(), server_log.clone(), acceptor.clone());
            tokio::spawn(async move {
                match acceptor {
                    // A client refusing our certificate aborts the handshake: nothing to serve.
                    Some(acceptor) => {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            answer(stream, &*handler, &log).await;
                        }
                    }
                    None => answer(tcp, &*handler, &log).await,
                }
            });
        }
    });
    (addr, log)
}

async fn answer<S, F>(stream: S, handler: &F, log: &Log)
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(&Request) -> Reply,
{
    let mut reader = BufReader::new(stream);
    let Some(request) = read_request(&mut reader).await else {
        return;
    };
    let reply = handler(&request);
    log.lock().unwrap().push(request);
    // Head and body in one write, so a client reading the head also gets the body.
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.reason,
        reply.body.len(),
        reply.body
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_request<S: AsyncRead + Unpin>(reader: &mut BufReader<S>) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.ok()?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let request = Request { method, target, headers, body: String::new() };
    let length: usize = request.header("content-length").map_or(Ok(0), str::parse).ok()?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    Some(Request { body: String::from_utf8(body).ok()?, ..request })
}
