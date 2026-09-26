use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::error_chain;
use crate::net::{Connector, Server};
use crate::tls::Tls;
use crate::{
    Endpoint, Error, GuestIps, PowerAction, TaskStatus, Version, VmKind, VmRef, VmResource, VncProxy, VncStream, api,
    ws,
};

/// `User-Agent` of every request.
pub(crate) const USER_AGENT: &str = concat!("vmherd/", env!("CARGO_PKG_VERSION"));
/// TCP connect + TLS handshake (with the certificate check) of a REST connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A whole REST exchange: connect, request, response and body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Cheap to clone (shared connection pool).
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    /// `scheme://host:port/`, no credentials / path / query.
    base: Url,
    /// `host:port`, for error messages.
    host: String,
    /// `PVEAPIToken=...`, marked sensitive.
    auth: HeaderValue,
    /// Where every connection goes (REST and websocket alike).
    server: Arc<Server>,
    http: HttpClient<Connector, Full<Bytes>>,
    tls: Tls,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("base", &self.inner.base.as_str()).finish_non_exhaustive()
    }
}

impl Client {
    /// Validates the endpoint (scheme http/https, host present, token id `user@realm!name`,
    /// non-empty secret) and builds the HTTP client. No network traffic.
    pub fn new(endpoint: &Endpoint) -> Result<Self, Error> {
        let base = normalize_base_url(&endpoint.url)?;
        let host = host_label(&base);
        let auth = auth_header(&endpoint.token_id, &endpoint.token_secret)?;
        let server = Arc::new(Server::from_url(&base, &host)?);
        let tls = Tls::new(endpoint.pinned_sha256)?;
        let http = build_http(&server, &tls);
        Ok(Self { inner: Arc::new(Inner { base, host, auth, server, http, tls }) })
    }

    /// `https://host:port` as configured.
    pub fn base_url(&self) -> &url::Url {
        &self.inner.base
    }

    /// `GET /version` (also the connectivity / auth / certificate check).
    pub async fn version(&self) -> Result<Version, Error> {
        self.get(self.api_url(&["version"])?).await
    }

    /// `GET /cluster/resources?type=vm` (templates included; filter on `template`).
    pub async fn vms(&self) -> Result<Vec<VmResource>, Error> {
        let mut url = self.api_url(&["cluster", "resources"])?;
        url.query_pairs_mut().append_pair("type", "vm");
        api::parse_resources(self.get(url).await?)
    }

    /// `POST /nodes/{node}/{qemu|lxc}/{vmid}/vncproxy` with `websocket=1` (and
    /// `generate-password=1` for qemu; retried without it if the server rejects the parameter).
    pub async fn vnc_proxy(&self, vm: &VmRef) -> Result<VncProxy, Error> {
        let url = self.api_url(&guest_segments(vm, &["vncproxy"])?)?;
        tracing::debug!(node = %vm.node, vmid = vm.vmid, kind = vm.kind.as_str(), "requesting a VNC proxy");
        let plain = [("websocket", "1")];
        let data: api::VncProxyData = if vm.kind == VmKind::Qemu {
            let with_password = [("websocket", "1"), ("generate-password", "1")];
            match self.post(url.clone(), &with_password).await {
                Err(Error::Api { status: 400, message }) if message.contains("generate-password") => {
                    tracing::debug!("server rejected generate-password, retrying without it");
                    self.post(url, &plain).await?
                }
                other => other?,
            }
        } else {
            self.post(url, &plain).await?
        };
        Ok(data.into())
    }

    /// Open `GET /nodes/{node}/{kind}/{vmid}/vncwebsocket?port=..&vncticket=..` as a websocket
    /// (`Sec-WebSocket-Protocol: binary`, token header, pinned TLS) and return it as a byte stream.
    pub async fn vnc_connect(&self, vm: &VmRef, proxy: &VncProxy) -> Result<VncStream, Error> {
        let mut url = self.api_url(&guest_segments(vm, &["vncwebsocket"])?)?;
        url.query_pairs_mut().append_pair("port", &proxy.port.to_string()).append_pair("vncticket", &proxy.ticket);
        tracing::debug!(node = %vm.node, vmid = vm.vmid, port = proxy.port, "opening the console websocket");
        ws::connect(ws::Target { url, auth: &self.inner.auth, tls: &self.inner.tls, server: &self.inner.server }).await
    }

    /// `vnc_proxy` + `vnc_connect`; returns the stream and the VNC password.
    pub async fn open_console(&self, vm: &VmRef) -> Result<(VncStream, String), Error> {
        let proxy = self.vnc_proxy(vm).await?;
        let stream = self.vnc_connect(vm, &proxy).await?;
        Ok((stream, proxy.password))
    }

    /// `POST /nodes/{node}/{kind}/{vmid}/status/{start|shutdown|stop}`; returns the task UPID.
    pub async fn power(&self, vm: &VmRef, action: PowerAction) -> Result<String, Error> {
        let url = self.api_url(&guest_segments(vm, &["status", action.as_str()])?)?;
        tracing::debug!(node = %vm.node, vmid = vm.vmid, action = action.as_str(), "power action");
        self.post(url, &[]).await
    }

    /// The guest's first global IPv4 / IPv6 address: from the QEMU guest agent
    /// (`agent/network-get-interfaces`, needs a running agent and `VM.Monitor`) or, for
    /// containers, `lxc/{vmid}/interfaces`.
    pub async fn guest_ips(&self, vm: &VmRef) -> Result<GuestIps, Error> {
        match vm.kind {
            VmKind::Qemu => {
                let url = self.api_url(&guest_segments(vm, &["agent", "network-get-interfaces"])?)?;
                let data: api::AgentInterfaces = self.get(url).await?;
                Ok(api::pick_ips(
                    data.result.into_iter().map(|i| (i.name, i.addresses.into_iter().map(|a| a.address).collect())),
                ))
            }
            VmKind::Lxc => {
                let url = self.api_url(&guest_segments(vm, &["interfaces"])?)?;
                let data: Vec<api::LxcInterface> = self.get(url).await?;
                Ok(api::pick_ips(data.into_iter().map(|i| {
                    let addrs = [i.inet, i.inet6].into_iter().flatten().collect();
                    (i.name, addrs)
                })))
            }
        }
    }

    /// `GET /nodes/{node}/tasks/{upid}/status`
    pub async fn task_status(&self, node: &str, upid: &str) -> Result<TaskStatus, Error> {
        validate_node(node)?;
        if upid.trim().is_empty() {
            return Err(Error::Config("empty task id (UPID)".to_owned()));
        }
        self.get(self.api_url(&["nodes", node, "tasks", upid, "status"])?).await
    }

    /// `<base>/api2/json/<segments...>`, each segment percent-encoded.
    fn api_url<S: AsRef<str>>(&self, segments: &[S]) -> Result<Url, Error> {
        api_url(&self.inner.base, segments)
    }

    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, Error> {
        tracing::debug!(path = url.path(), "GET");
        self.execute(self.request(Method::GET, &url, None)?).await
    }

    async fn post<T: DeserializeOwned>(&self, url: Url, form: &[(&str, &str)]) -> Result<T, Error> {
        tracing::debug!(path = url.path(), "POST");
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
        self.execute(self.request(Method::POST, &url, Some(body))?).await
    }

    /// A request with the token and user agent headers (and a form body, if any).
    fn request(&self, method: Method, url: &Url, form: Option<String>) -> Result<Request<Full<Bytes>>, Error> {
        let mut request = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header(AUTHORIZATION, self.inner.auth.clone())
            .header(hyper::header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        if form.is_some() {
            request = request.header(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"));
        }
        // The error text never contains the URL or the headers.
        request
            .body(Full::new(Bytes::from(form.unwrap_or_default())))
            .map_err(|e| Error::Config(format!("cannot build the request: {e}")))
    }

    /// Sends the request and decodes `data`, mapping HTTP / transport failures.
    async fn execute<T: DeserializeOwned>(&self, request: Request<Full<Bytes>>) -> Result<T, Error> {
        let exchange = async {
            let response = self.inner.http.request(request).await.map_err(|e| self.transport_error(&e))?;
            let (head, body) = response.into_parts();
            Ok::<_, Error>((head, body.collect().await.map(|b| b.to_bytes())))
        };
        let (head, body) = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| Error::Network { host: self.inner.host.clone(), detail: "request timed out".to_owned() })??;
        let status = head.status;
        if !status.is_success() {
            let reason = reason_phrase(&head);
            let body = body.unwrap_or_default();
            let message = api::error_message(&body, &reason);
            tracing::debug!(status = status.as_u16(), %message, "API error");
            return Err(Error::Api { status: status.as_u16(), message });
        }
        let body = body.map_err(|e| self.transport_error(&e))?;
        api::decode_data(&body)
    }

    /// A refused certificate or connect failure as the connector reported it (it travels as
    /// the source of hyper's error, so this is exact even with concurrent connects), else a
    /// network error with the whole chain.
    fn transport_error(&self, err: &(dyn StdError + 'static)) -> Error {
        let mut next = Some(err);
        while let Some(e) = next {
            match e.downcast_ref::<Error>() {
                Some(Error::UntrustedCert(problem)) => return Error::UntrustedCert(problem.clone()),
                Some(Error::Network { host, detail }) => {
                    return Error::Network { host: host.clone(), detail: detail.clone() };
                }
                Some(other) => return Error::Network { host: self.inner.host.clone(), detail: other.to_string() },
                None => next = e.source(),
            }
        }
        Error::Network { host: self.inner.host.clone(), detail: error_chain(err) }
    }
}

/// Checks scheme / host / credentials and strips everything after the port.
fn normalize_base_url(url: &Url) -> Result<Url, Error> {
    if !matches!(url.scheme(), "https" | "http") {
        return Err(Error::Config(format!("the URL must start with https:// (got {}://)", url.scheme())));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(Error::Config("the URL has no host".to_owned()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Config("the URL must not contain a user name or password".to_owned()));
    }
    let mut base = url.clone();
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    Ok(base)
}

/// `host:port` (the default port of the scheme when none is given).
fn host_label(base: &Url) -> String {
    let host = base.host_str().unwrap_or_default();
    match base.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// `PVEAPIToken=<id>=<secret>` as a sensitive header value (never logged by hyper / http).
fn auth_header(token_id: &str, secret: &str) -> Result<HeaderValue, Error> {
    let token_id = token_id.trim();
    let secret = secret.trim();
    validate_token_id(token_id)?;
    if secret.is_empty() {
        return Err(Error::Config("the API token secret is empty".to_owned()));
    }
    let mut value = HeaderValue::try_from(format!("PVEAPIToken={token_id}={secret}")).map_err(|_| {
        Error::Config("the API token contains characters that are not allowed in an HTTP header".to_owned())
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// `user@realm!tokenname` like Proxmox: user without whitespace / `:` / `/`, realm and token
/// name start with a letter followed by letters, digits, `.`, `-`, `_`.
pub(crate) fn validate_token_id(token_id: &str) -> Result<(), Error> {
    let valid = token_id.rsplit_once('!').is_some_and(|(user_id, token_name)| {
        is_identifier(token_name)
            && user_id.rsplit_once('@').is_some_and(|(user, realm)| {
                !user.is_empty()
                    && !user.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '/'))
                    && is_identifier(realm)
            })
    });
    if valid { Ok(()) } else { Err(Error::Config(token_id_error(token_id))) }
}

/// Why a token ID is invalid. Never echoes the input: users paste `id=secret`,
/// `PVEAPIToken=id=secret` or the bare secret into the ID field.
fn token_id_error(token_id: &str) -> String {
    const SHAPE: &str = "the API token ID must look like user@realm!tokenname";
    let hint = if token_id.contains('=') {
        "enter only the ID here and the secret in the secret field"
    } else if !token_id.contains('!') {
        "the \"!tokenname\" part is missing"
    } else if !token_id.contains('@') {
        "the \"@realm\" part is missing"
    } else {
        "names start with a letter, then letters, digits, '.', '-' or '_'"
    };
    format!("{SHAPE} ({hint})")
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Node names as Proxmox allows them (also keeps them from escaping their path segment).
fn validate_node(node: &str) -> Result<(), Error> {
    let mut chars = node.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    if valid { Ok(()) } else { Err(Error::Config(format!("invalid node name {node:?}"))) }
}

/// `nodes/{node}/{kind}/{vmid}/<rest...>` for a guest.
fn guest_segments(vm: &VmRef, rest: &[&str]) -> Result<Vec<String>, Error> {
    validate_node(&vm.node)?;
    let head = ["nodes".to_owned(), vm.node.clone(), vm.kind.as_str().to_owned(), vm.vmid.to_string()];
    Ok(head.into_iter().chain(rest.iter().map(|s| (*s).to_owned())).collect())
}

fn api_url<S: AsRef<str>>(base: &Url, segments: &[S]) -> Result<Url, Error> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| Error::Config("the URL cannot have a path".to_owned()))?
        .clear()
        .extend(["api2", "json"])
        .extend(segments.iter().map(AsRef::as_ref));
    Ok(url)
}

/// The pooled HTTP/1.1 client over [`Connector`]: every connection goes straight to the
/// cluster (no proxy, so the token is never handed to one) and is used only after its TLS
/// certificate was accepted. hyper follows no redirects.
fn build_http(server: &Arc<Server>, tls: &Tls) -> HttpClient<Connector, Full<Bytes>> {
    HttpClient::builder(TokioExecutor::new()).pool_timer(TokioTimer::new()).build(Connector::new(
        server.clone(),
        tls.clone(),
        CONNECT_TIMEOUT,
    ))
}

/// The reason phrase as sent (Proxmox puts error texts there), else the canonical one.
fn reason_phrase(head: &hyper::http::response::Parts) -> String {
    head.extensions
        .get::<hyper::ext::ReasonPhrase>()
        .and_then(|reason| std::str::from_utf8(reason.as_bytes()).ok())
        .or_else(|| head.status.canonical_reason())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str, token_id: &str, secret: &str) -> Endpoint {
        Endpoint {
            url: Url::parse(url).unwrap(),
            token_id: token_id.to_owned(),
            token_secret: secret.to_owned(),
            pinned_sha256: None,
        }
    }

    fn config_err(result: Result<Client, Error>) -> String {
        match result {
            Err(Error::Config(msg)) => msg,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    const SECRET: &str = "0b5f3c4e-1111-2222-3333-444455556666";

    #[test]
    fn valid_endpoint_is_normalized() {
        let c = Client::new(&endpoint("https://pve.example:8006/some/path?x=1#y", "root@pam!grid", SECRET)).unwrap();
        assert_eq!(c.base_url().as_str(), "https://pve.example:8006/");
        assert_eq!(c.inner.host, "pve.example:8006");
        assert!(c.inner.auth.is_sensitive());
        assert!(!format!("{c:?}").contains(SECRET));
        let plain = Client::new(&endpoint("http://127.0.0.1:18006", "u@pve!t", SECRET)).unwrap();
        assert_eq!(plain.inner.host, "127.0.0.1:18006");
        let v6 = Client::new(&endpoint("https://[fd00::1]", "u@pve!t", SECRET)).unwrap();
        assert_eq!(v6.inner.host, "[fd00::1]:443");
    }

    #[test]
    fn endpoint_validation() {
        assert!(config_err(Client::new(&endpoint("ftp://pve:8006", "root@pam!t", SECRET))).contains("https://"));
        assert!(config_err(Client::new(&endpoint("https://u:p@pve:8006", "root@pam!t", SECRET))).contains("password"));
        assert!(config_err(Client::new(&endpoint("https://pve:8006", "root@pam!t", " "))).contains("secret"));
        let bad_secret = config_err(Client::new(&endpoint("https://pve:8006", "root@pam!t", "a\nb")));
        assert!(!bad_secret.contains("a\nb"), "the secret must not leak into errors");
        for id in ["root@pam", "root!t", "@pam!t", "root@pam!", "root@!t", "ro ot@pam!t", "root@pam!1t", "root@pam!t t"]
        {
            assert!(
                config_err(Client::new(&endpoint("https://pve:8006", id, SECRET))).contains("user@realm!tokenname"),
                "{id}"
            );
        }
        for id in ["root@pam!vmherd", "svc.grid@pve!grid_1", " root@pam!t "] {
            assert!(Client::new(&endpoint("https://pve:8006", id, SECRET)).is_ok(), "{id}");
        }
    }

    #[test]
    fn token_id_errors_never_echo_the_input() {
        let pasted = [
            format!("root@pam!grid={SECRET}"),
            format!("PVEAPIToken=root@pam!grid={SECRET}"),
            SECRET.to_owned(),
            format!("root@pam!{SECRET}"),
            format!("{SECRET}@pam!grid!"),
        ];
        for id in &pasted {
            let msg = config_err(Client::new(&endpoint("https://pve:8006", id, SECRET)));
            assert!(msg.contains("user@realm!tokenname"), "{msg}");
            for part in SECRET.split('-') {
                assert!(!msg.contains(part), "{msg}");
            }
        }
        let with_secret = config_err(Client::new(&endpoint("https://pve:8006", &pasted[0], SECRET)));
        assert!(with_secret.contains("secret field"), "{with_secret}");
        assert!(config_err(Client::new(&endpoint("https://pve:8006", "root@pam", SECRET))).contains("!tokenname"));
        assert!(config_err(Client::new(&endpoint("https://pve:8006", "root!t", SECRET))).contains("@realm"));
    }

    #[test]
    fn auth_header_format() {
        let value = auth_header("root@pam!grid", &format!(" {SECRET}\n")).unwrap();
        assert_eq!(value.to_str().unwrap(), format!("PVEAPIToken=root@pam!grid={SECRET}"));
    }

    #[test]
    fn node_validation() {
        for ok in ["pve2", "pve-1", "node.example", "1node"] {
            assert!(validate_node(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-pve", ".pve", "pve/../x", "pve 1", "pve%2F", "pve?x", "pve_1"] {
            assert!(matches!(validate_node(bad), Err(Error::Config(_))), "{bad}");
        }
    }

    #[test]
    fn paths_are_built_from_segments() {
        let base = Url::parse("https://pve:8006/").unwrap();
        let vm = VmRef { vmid: 101, node: "pve2".into(), kind: VmKind::Qemu };
        let url = api_url(&base, &guest_segments(&vm, &["status", "start"]).unwrap()).unwrap();
        assert_eq!(url.as_str(), "https://pve:8006/api2/json/nodes/pve2/qemu/101/status/start");
        let lxc = VmRef { vmid: 200, node: "pve3".into(), kind: VmKind::Lxc };
        let url = api_url(&base, &guest_segments(&lxc, &["vncproxy"]).unwrap()).unwrap();
        assert_eq!(url.path(), "/api2/json/nodes/pve3/lxc/200/vncproxy");
        let bad = VmRef { vmid: 1, node: "../x".into(), kind: VmKind::Qemu };
        assert!(matches!(guest_segments(&bad, &[]), Err(Error::Config(_))));
    }

    #[test]
    fn upid_is_one_encoded_segment() {
        let base = Url::parse("https://pve:8006/").unwrap();
        let upid = "UPID:pve2:00ABCDEF:0123/4567:6710AB00:qmstart:101:root@pam!grid:";
        let url = api_url(&base, &["nodes", "pve2", "tasks", upid, "status"]).unwrap();
        let segments: Vec<_> = url.path_segments().unwrap().collect();
        assert_eq!(segments.len(), 7);
        assert_eq!(segments[5], "UPID:pve2:00ABCDEF:0123%2F4567:6710AB00:qmstart:101:root@pam!grid:");
    }

    #[tokio::test]
    async fn task_status_validates_locally() {
        let c = Client::new(&endpoint("http://127.0.0.1:9", "root@pam!t", SECRET)).unwrap();
        assert!(matches!(c.task_status("bad/node", "UPID:x").await, Err(Error::Config(_))));
        assert!(matches!(c.task_status("pve2", " ").await, Err(Error::Config(_))));
    }
}
