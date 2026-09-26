//! TCP (+ TLS) connections to the cluster, shared by the REST client and the console websocket.
//!
//! [`connect`] returns a connection only after the TLS handshake and the certificate check
//! (pin or system trust, see [`crate::tls`]) succeeded, so neither hyper nor the websocket
//! upgrade can write a byte to a server whose certificate was refused.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
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

async fn connect_tcp(server: &Server) -> Result<TcpStream, Error> {
    let port = server.port;
    let connecting = async {
        match &server.host {
            Host::Domain(domain) => TcpStream::connect((domain.as_str(), port)).await,
            Host::Ipv4(ip) => TcpStream::connect(SocketAddr::new(IpAddr::V4(*ip), port)).await,
            Host::Ipv6(ip) => TcpStream::connect(SocketAddr::new(IpAddr::V6(*ip), port)).await,
        }
    };
    let tcp = tokio::time::timeout(TCP_CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| server.network("TCP connect timed out".to_owned()))?
        .map_err(|e| server.network(error_chain(&e)))?;
    tcp.set_nodelay(true).map_err(|e| server.network(error_chain(&e)))?;
    Ok(tcp)
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
}
