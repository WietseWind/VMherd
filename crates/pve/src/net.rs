//! TCP (+ TLS) connections to the cluster, shared by the REST client and the console websocket.
//!
//! [`connect`] returns a connection only after the TLS handshake and the certificate check
//! (pin or system trust, see [`crate::tls`]) succeeded, so neither hyper nor the websocket
//! upgrade can write a byte to a server whose certificate was refused.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use url::{Host, Url};

use crate::Error;
use crate::error::error_chain;
use crate::tls::{Tls, TlsStream};

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The cluster endpoint connections go to.
#[derive(Clone, Debug)]
pub(crate) struct Server {
    host: Host<String>,
    port: u16,
    /// `https` (TLS) or `http` (plain, for tests against a mock).
    secure: bool,
    /// `host:port`, for error messages.
    pub(crate) label: String,
}

impl Server {
    /// Host, port and scheme of `url` (`http` or `https`).
    pub(crate) fn from_url(url: &Url, label: &str) -> Result<Self, Error> {
        let secure = match url.scheme() {
            "https" => true,
            "http" => false,
            other => return Err(Error::Config(format!("unsupported URL scheme {other}"))),
        };
        let host = url.host().map(|h| h.to_owned()).ok_or_else(|| Error::Config("the URL has no host".to_owned()))?;
        let port = url.port_or_known_default().ok_or_else(|| Error::Config("the URL has no port".to_owned()))?;
        Ok(Self { host, port, secure, label: label.to_owned() })
    }

    fn network(&self, detail: String) -> Error {
        Error::Network { host: self.label.clone(), detail }
    }
}

/// TCP connect, then (for `https`) the TLS handshake with the certificate check.
pub(crate) async fn connect(server: &Server, tls: &Tls) -> Result<Transport, Error> {
    let tcp = connect_tcp(server).await?;
    if !server.secure {
        return Ok(Transport::Plain(tcp));
    }
    let stream = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tls.connect(tcp, &server.host, &server.label))
        .await
        .map_err(|_| server.network("TLS handshake timed out".to_owned()))??;
    Ok(Transport::Tls(Box::new(stream)))
}

/// hyper-util's TCP connector, as reqwest used it: tries every resolved address (the timeout is
/// split among them), the other address family in parallel after 300 ms (happy eyeballs), so a
/// dead IPv6 route does not use up the whole timeout.
async fn connect_tcp(server: &Server) -> Result<TcpStream, Error> {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(TCP_CONNECT_TIMEOUT));
    // `Host` displays IPv6 addresses in brackets; only host and port matter to the connector.
    let uri = Uri::try_from(format!("http://{}:{}", server.host, server.port))
        .map_err(|e| Error::Config(format!("invalid host: {e}")))?;
    // Name resolution is not covered by the connector's own timeout.
    let tcp = tokio::time::timeout(TCP_CONNECT_TIMEOUT, tower_service::Service::call(&mut connector, uri))
        .await
        .map_err(|_| server.network("TCP connect timed out".to_owned()))?
        .map_err(|e| server.network(error_chain(&e)))?;
    Ok(tcp.into_inner())
}

/// A connection to the cluster: plain TCP (`http`) or TLS with an accepted certificate.
#[derive(Debug)]
pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

impl AsyncRead for Transport {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_read(cx, buf),
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_write(cx, buf),
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_flush(cx),
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_shutdown(cx),
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_shutdown(cx),
        }
    }
}

impl Connection for Transport {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

/// hyper's connector for the REST pool: always the client's own cluster (whatever the URI
/// says; requests are built from the base URL anyway), connect timeout included.
#[derive(Clone, Debug)]
pub(crate) struct Connector {
    server: Arc<Server>,
    tls: Tls,
    timeout: Duration,
}

impl Connector {
    pub(crate) fn new(server: Arc<Server>, tls: Tls, timeout: Duration) -> Self {
        Self { server, tls, timeout }
    }
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<Transport>;
    /// A [`crate::Error`], found again in hyper's error chain by the client.
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let transport = tokio::time::timeout(this.timeout, connect(&this.server, &this.tls))
                .await
                .map_err(|_| this.server.network("connect timed out".to_owned()))??;
            Ok(TokioIo::new(transport))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_from_url() {
        let s = Server::from_url(&Url::parse("https://pve.example:8006/x").unwrap(), "pve.example:8006").unwrap();
        assert_eq!((s.host, s.port, s.secure), (Host::Domain("pve.example".to_owned()), 8006, true));
        let s = Server::from_url(&Url::parse("http://[::1]/").unwrap(), "[::1]:80").unwrap();
        assert_eq!((s.port, s.secure), (80, false));
        assert!(matches!(Server::from_url(&Url::parse("ftp://pve/").unwrap(), "pve"), Err(Error::Config(_))));
    }

    #[tokio::test]
    async fn tcp_connects_by_name_and_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        for host in ["localhost", "127.0.0.1"] {
            let server = Server::from_url(&Url::parse(&format!("http://{host}:{port}/")).unwrap(), host).unwrap();
            let tcp = connect_tcp(&server).await.unwrap();
            assert_eq!(tcp.peer_addr().unwrap().port(), port, "{host}");
            assert!(tcp.nodelay().unwrap());
        }
        if let Ok(v6) = tokio::net::TcpListener::bind("[::1]:0").await {
            let port = v6.local_addr().unwrap().port();
            tokio::spawn(async move { while v6.accept().await.is_ok() {} });
            let server = Server::from_url(&Url::parse(&format!("http://[::1]:{port}/")).unwrap(), "[::1]").unwrap();
            assert!(connect_tcp(&server).await.unwrap().peer_addr().unwrap().is_ipv6());
        }
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let server = Server::from_url(&Url::parse(&format!("http://{closed}/")).unwrap(), "closed").unwrap();
        assert!(matches!(connect_tcp(&server).await, Err(Error::Network { host, .. }) if host == "closed"));
    }
}
