//! macOS backend: Secure Transport and the system trust store (Security.framework).
//!
//! Secure Transport is told to stop as soon as the server's certificate chain has arrived
//! (`break_on_server_auth`, which also switches its own trust evaluation off). The chain is
//! checked right there, before the client sends its key exchange: a pinned leaf must match
//! exactly (no name / expiry / chain checks, like the rustls backend); without a pin the system
//! trust store evaluates the chain for the host name, like `rustls-platform-verifier` does on
//! macOS. Only a certificate that passes lets the handshake continue, and only a finished
//! handshake yields a [`TlsStream`], so no request (API token) can reach an unchecked server.
//! The handshake signature is verified by Secure Transport in any case.
//!
//! TLS 1.2 (Secure Transport's maximum), ECDHE with AES-GCM only, no session resumption, no
//! renegotiation (Secure Transport's default: off, and no server identity change).
//!
//! Secure Transport drives blocking `Read + Write` streams, so it runs over in-memory
//! [`Buffers`] which the async code fills from and drains to the socket.

use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, BytesMut};
#[cfg(test)]
use security_framework::certificate::SecCertificate;
use security_framework::cipher_suite::CipherSuite;
use security_framework::policy::SecPolicy;
use security_framework::secure_transport::{
    HandshakeError, SslConnectionType, SslContext, SslProtocol, SslProtocolSide, SslStream,
};
use security_framework::trust::SecTrust;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use url::Host;

use super::{leaf_sha256, log_rejection};
use crate::error::error_chain;
use crate::{CertProblem, Error};

/// The cipher suites offered: forward secrecy and AEAD only (rustls' TLS 1.2 set, less
/// ChaCha20, which Secure Transport does not have).
const CIPHERS: [CipherSuite; 4] = [
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
];
/// Largest chunk read from the socket at once (a bit more than one TLS record).
const READ_CHUNK: usize = 17 * 1024;

/// The TLS setup shared by all connections of one client.
#[derive(Clone, Debug)]
pub(crate) struct Tls {
    pin: Option<[u8; 32]>,
    /// Tests only: trust these roots instead of the system's (nothing is added to a keychain).
    #[cfg(test)]
    anchors: Vec<SecCertificate>,
}

impl Tls {
    /// The setup for an optional pinned leaf fingerprint.
    #[allow(clippy::unnecessary_wraps)] // the rustls backend can fail here
    pub(crate) fn new(pin: Option<[u8; 32]>) -> Result<Self, Error> {
        Ok(Self {
            pin,
            #[cfg(test)]
            anchors: Vec::new(),
        })
    }

    /// The TLS handshake over `tcp` to `host`; `label` (`host:port`) is for error messages.
    /// A refused certificate is [`Error::UntrustedCert`] with the presented fingerprint.
    pub(crate) async fn connect(
        &self,
        mut tcp: TcpStream,
        host: &Host<String>,
        label: &str,
    ) -> Result<TlsStream, Error> {
        let network = |detail: String| Error::Network { host: label.to_owned(), detail };
        let io_error = |e: io::Error| network(error_chain(&e));
        let name = server_name(host);
        let context = new_context(host, &name).map_err(|e| network(format!("TLS setup failed: {e}")))?;
        let mut verified = false;
        let mut state = context.handshake(Buffers::default());
        loop {
            match state {
                Ok(mut ssl) => {
                    if !verified {
                        // Cannot happen with `break_on_server_auth`; never hand out such a stream.
                        return Err(network("the TLS handshake skipped the certificate check".to_owned()));
                    }
                    send_all(&mut tcp, ssl.get_mut()).await.map_err(io_error)?;
                    return Ok(TlsStream { tcp, ssl, closing: false });
                }
                Err(HandshakeError::Interrupted(mut mid)) => {
                    send_all(&mut tcp, mid.get_mut()).await.map_err(io_error)?;
                    if mid.server_auth_completed() {
                        self.check(mid.context(), &name, label)?;
                        verified = true;
                    } else if mid.would_block() {
                        poll_fn(|cx| poll_receive(&mut tcp, mid.get_mut(), cx)).await.map_err(io_error)?;
                    } else {
                        return Err(network(format!("TLS handshake failed: {}", mid.error())));
                    }
                    state = mid.handshake();
                }
                Err(HandshakeError::Failure(e)) => return Err(network(format!("TLS handshake failed: {e}"))),
            }
        }
    }

    /// Accepts the server's certificate, or refuses it with its fingerprint.
    fn check(&self, context: &SslContext, name: &str, label: &str) -> Result<(), Error> {
        let trust = context.peer_trust2().ok().flatten();
        // Index 0 is always the leaf; the call is deprecated only in favour of a macOS 12 API.
        #[allow(deprecated)]
        let leaf = trust.as_ref().and_then(|trust| trust.certificate_at_index(0));
        let (Some(mut trust), Some(leaf)) = (trust, leaf) else {
            return Err(Error::Network { host: label.to_owned(), detail: "the server sent no certificate".to_owned() });
        };
        let sha256 = leaf_sha256(&leaf.to_der());
        let refuse = |changed: bool, reason: String| {
            let problem = CertProblem { sha256, changed };
            log_rejection(&problem, &reason);
            Error::UntrustedCert(problem)
        };
        match self.pin {
            Some(pin) if pin == sha256 => Ok(()),
            Some(_) => Err(refuse(true, "does not match the pinned fingerprint".to_owned())),
            None => self.system_trust(&mut trust, name).map_err(|reason| refuse(false, reason)),
        }
    }

    /// The system trust store's verdict on the presented chain for `name`.
    fn system_trust(&self, trust: &mut SecTrust, name: &str) -> Result<(), String> {
        trust.set_policy(&SecPolicy::create_ssl(SslProtocolSide::SERVER, Some(name))).map_err(|e| e.to_string())?;
        #[cfg(test)]
        if !self.anchors.is_empty() {
            trust.set_anchor_certificates(&self.anchors).map_err(|e| e.to_string())?;
            trust.set_trust_anchor_certificates_only(true).map_err(|e| e.to_string())?;
        }
        trust.evaluate_with_error().map_err(|e| e.to_string())
    }
}

/// The name the certificate must be valid for: the domain (without a trailing dot) or the IP
/// address, as `rustls-platform-verifier` passes it to the same system policy.
fn server_name(host: &Host<String>) -> String {
    match host {
        Host::Domain(domain) => domain.trim_end_matches('.').to_owned(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    }
}

/// A client context: SNI for domain names (not for IP addresses, like rustls), TLS 1.2+,
/// [`CIPHERS`], and a stop after the server's certificate. No peer ID, so no resumption.
fn new_context(host: &Host<String>, name: &str) -> security_framework::base::Result<SslContext> {
    let mut context = SslContext::new(SslProtocolSide::CLIENT, SslConnectionType::STREAM)?;
    if matches!(host, Host::Domain(_)) {
        context.set_peer_domain_name(name)?;
    }
    context.set_protocol_version_min(SslProtocol::TLS12)?;
    let supported = context.supported_ciphers()?;
    let ciphers: Vec<CipherSuite> = CIPHERS.into_iter().filter(|c| supported.contains(c)).collect();
    context.set_enabled_ciphers(&ciphers)?;
    context.set_break_on_server_auth(true)?;
    Ok(context)
}

/// The in-memory transport under Secure Transport.
#[derive(Debug, Default)]
struct Buffers {
    /// Received from the socket, not yet consumed.
    incoming: BytesMut,
    /// The socket reached end of file.
    eof: bool,
    /// Produced by Secure Transport, not yet written to the socket.
    outgoing: BytesMut,
}

impl Read for Buffers {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.incoming.is_empty() {
            return if self.eof { Ok(0) } else { Err(io::ErrorKind::WouldBlock.into()) };
        }
        let n = buf.len().min(self.incoming.len());
        buf[..n].copy_from_slice(&self.incoming[..n]);
        self.incoming.advance(n);
        Ok(n)
    }
}

impl Write for Buffers {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.outgoing.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes the queued TLS records to the socket.
fn poll_send(tcp: &mut TcpStream, buffers: &mut Buffers, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    while !buffers.outgoing.is_empty() {
        let n = ready!(Pin::new(&mut *tcp).poll_write(cx, &buffers.outgoing))?;
        if n == 0 {
            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
        }
        buffers.outgoing.advance(n);
    }
    Poll::Ready(Ok(()))
}

/// Writes what the socket takes right now, without waiting and without registering a waker.
fn try_send(tcp: &TcpStream, buffers: &mut Buffers) -> io::Result<()> {
    while !buffers.outgoing.is_empty() {
        match tcp.try_write(&buffers.outgoing) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buffers.outgoing.advance(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads the next chunk from the socket into the buffers (or notes end of file).
fn poll_receive(tcp: &mut TcpStream, buffers: &mut Buffers, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    let mut chunk = [0u8; READ_CHUNK];
    let mut buf = ReadBuf::new(&mut chunk);
    ready!(Pin::new(tcp).poll_read(cx, &mut buf))?;
    match buf.filled() {
        [] => buffers.eof = true,
        data => buffers.incoming.extend_from_slice(data),
    }
    Poll::Ready(Ok(()))
}

async fn send_all(tcp: &mut TcpStream, buffers: &mut Buffers) -> io::Result<()> {
    poll_fn(|cx| poll_send(tcp, buffers, cx)).await
}

/// A TLS connection whose server certificate was accepted.
#[derive(Debug)]
pub(crate) struct TlsStream {
    tcp: TcpStream,
    ssl: SslStream<Buffers>,
    /// `close_notify` is queued.
    closing: bool,
}

impl AsyncRead for TlsStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            // Queued records (alerts, or a writer's backlog) go out if the socket takes them now.
            // Never wait for that here: the reader may run on another task than the writer, and
            // registering its waker for writability would replace the writer's, which then never
            // wakes. The writer's own `poll_write` / `poll_flush` waits for the socket.
            try_send(&this.tcp, this.ssl.get_mut())?;
            let room = buf.remaining().min(READ_CHUNK);
            match this.ssl.read(buf.initialize_unfilled_to(room)) {
                // 0 is end of file: `close_notify`, or the socket closed without one (which
                // pveproxy does when a console ends).
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(poll_receive(&mut this.tcp, this.ssl.get_mut(), cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for TlsStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // New data only once the earlier records left: at most one write is buffered.
        ready!(poll_send(&mut this.tcp, this.ssl.get_mut(), cx))?;
        let n = this.ssl.write(buf)?;
        // Start sending now; what the socket does not take yet goes with the next write or flush.
        if let Poll::Ready(Err(e)) = poll_send(&mut this.tcp, this.ssl.get_mut(), cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(poll_send(&mut this.tcp, this.ssl.get_mut(), cx))?;
        Pin::new(&mut this.tcp).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.closing {
            this.closing = true;
            // Fails only if the session is already closed; the socket is shut down either way.
            // Unlike rustls this is no half close: reads return EOF from here on.
            if let Err(e) = this.ssl.close() {
                tracing::debug!(error = %e, "TLS close_notify not sent");
            }
        }
        ready!(poll_send(&mut this.tcp, this.ssl.get_mut(), cx))?;
        Pin::new(&mut this.tcp).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    //! The system trust path against a local CA (given to the evaluation as its only root, so
    //! the machine's keychains stay untouched). The pinning / untrusted paths are covered by the
    //! integration tests in `tests/tls.rs` for both backends.

    use std::sync::Arc;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;

    struct Pki {
        ca: CertificateDer<'static>,
        server: Arc<rustls::ServerConfig>,
        leaf_sha256: [u8; 32],
    }

    /// A CA and a leaf it signed for `names`, served with the CA as the chain.
    fn new_pki(names: &[&str]) -> Pki {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "VMherd test CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);

        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut leaf_params =
            rcgen::CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>()).unwrap();
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone(), ca.der().clone()], key)
            .unwrap();
        Pki { ca: ca.der().clone(), server: Arc::new(server), leaf_sha256: leaf_sha256(leaf.der()) }
    }

    /// Serves one TLS connection that echoes one line; returns the port.
    async fn echo_server(config: Arc<rustls::ServerConfig>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = TlsAcceptor::from(config).accept(tcp).await {
                let mut buf = [0u8; 5];
                if tls.read_exact(&mut buf).await.is_ok() {
                    let _ = tls.write_all(&buf).await;
                    let _ = tls.shutdown().await;
                }
            }
        });
        port
    }

    fn trusting(pki: &Pki) -> Tls {
        Tls { pin: None, anchors: vec![SecCertificate::from_der(&pki.ca).unwrap()] }
    }

    async fn connect(tls: &Tls, port: u16, host: Host<String>) -> Result<TlsStream, Error> {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tls.connect(tcp, &host, "test").await
    }

    #[tokio::test]
    async fn trusted_chain_connects_and_carries_data() {
        let pki = new_pki(&["localhost", "127.0.0.1"]);
        for host in [Host::Ipv4([127, 0, 0, 1].into()), Host::Domain("localhost".to_owned())] {
            let port = echo_server(pki.server.clone()).await;
            let mut stream = connect(&trusting(&pki), port, host.clone()).await.unwrap();
            stream.write_all(b"hello").await.unwrap();
            stream.flush().await.unwrap();
            let mut back = Vec::new();
            stream.read_to_end(&mut back).await.unwrap();
            assert_eq!(back, b"hello", "{host}");
        }
    }

    #[tokio::test]
    async fn wrong_host_name_is_untrusted_with_the_leaf_fingerprint() {
        let pki = new_pki(&["pve.example"]);
        let port = echo_server(pki.server.clone()).await;
        match connect(&trusting(&pki), port, Host::Ipv4([127, 0, 0, 1].into())).await {
            Err(Error::UntrustedCert(problem)) => {
                assert_eq!(problem, CertProblem { sha256: pki.leaf_sha256, changed: false });
            }
            other => panic!("expected UntrustedCert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_root_is_untrusted() {
        let pki = new_pki(&["localhost"]);
        let port = echo_server(pki.server.clone()).await;
        // The real system store: a fresh private CA is not in it.
        let tls = Tls::new(None).unwrap();
        match connect(&tls, port, Host::Domain("localhost".to_owned())).await {
            Err(Error::UntrustedCert(problem)) => assert!(!problem.changed && problem.sha256 == pki.leaf_sha256),
            other => panic!("expected UntrustedCert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pin_overrides_the_trust_store() {
        let pki = new_pki(&["pve.example"]);
        // Wrong name and unknown root, but pinned: accepted.
        let port = echo_server(pki.server.clone()).await;
        let pinned = Tls::new(Some(pki.leaf_sha256)).unwrap();
        assert!(connect(&pinned, port, Host::Domain("localhost".to_owned())).await.is_ok());
        // Trusted chain, but another pin: refused as changed.
        let trusted = new_pki(&["localhost"]);
        let port = echo_server(trusted.server.clone()).await;
        let other = Tls { pin: Some([1; 32]), ..trusting(&trusted) };
        assert!(matches!(
            connect(&other, port, Host::Domain("localhost".to_owned())).await,
            Err(Error::UntrustedCert(CertProblem { changed: true, .. }))
        ));
    }

    /// Megabytes each way with the reader and the writer on different tasks: backpressure on
    /// the socket must not lose either side's wakeup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_data_with_reader_and_writer_on_separate_tasks() {
        const LEN: usize = 8 * 1024 * 1024;
        let pki = new_pki(&["localhost"]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = pki.server.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = TlsAcceptor::from(server).accept(tcp).await.unwrap();
            let (mut read, mut write) = tokio::io::split(tls);
            // Ends with an error when the client goes away.
            let _ = tokio::io::copy(&mut read, &mut write).await;
        });
        let stream = connect(&trusting(&pki), port, Host::Domain("localhost".to_owned())).await.unwrap();
        let (mut read, mut write) = tokio::io::split(stream);
        let data: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
        let expected = data.clone();
        let writer = tokio::spawn(async move {
            for chunk in data.chunks(10_000) {
                write.write_all(chunk).await.unwrap();
            }
            write.flush().await.unwrap();
            write
        });
        let reader = tokio::spawn(async move {
            let mut back = vec![0; LEN];
            read.read_exact(&mut back).await.unwrap();
            back
        });
        let both = async { (writer.await.unwrap(), reader.await.unwrap()) };
        let (_write, back) = tokio::time::timeout(std::time::Duration::from_secs(30), both).await.expect("stalled");
        assert!(back == expected, "the echo differs");
    }

    #[test]
    fn server_names() {
        assert_eq!(server_name(&Host::Domain("pve.example.".to_owned())), "pve.example");
        assert_eq!(server_name(&Host::Ipv6("fd00::1".parse().unwrap())), "fd00::1");
    }

    #[test]
    fn buffers_block_until_data_or_eof() {
        let mut buffers = Buffers::default();
        let mut buf = [0u8; 4];
        assert_eq!(buffers.read(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        buffers.incoming.extend_from_slice(b"abcdef");
        assert_eq!(buffers.read(&mut buf).unwrap(), 4);
        assert_eq!(buffers.read(&mut buf).unwrap(), 2);
        buffers.eof = true;
        assert_eq!(buffers.read(&mut buf).unwrap(), 0);
        assert_eq!(buffers.write(b"xy").unwrap(), 2);
        assert_eq!(&buffers.outgoing[..], b"xy");
    }
}
