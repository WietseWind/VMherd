use serde::{Deserialize, Deserializer, Serialize};

/// How to reach one cluster.
#[derive(Clone)]
pub struct Endpoint {
    /// `https://host:8006` (any path is ignored; the API lives under `/api2/json`).
    pub url: url::Url,
    /// `user@realm!tokenname`
    pub token_id: String,
    pub token_secret: String,
    /// Trusted leaf certificate (SHA-256 of the DER), set after the user accepted it.
    pub pinned_sha256: Option<[u8; 32]>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url.as_str())
            .field("token_id", &self.token_id)
            .field("token_secret", &"<redacted>")
            .field("pinned_sha256", &self.pinned_sha256.map(|p| crate::format_fingerprint(&p)))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmKind {
    Qemu,
    Lxc,
}

impl VmKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VmKind::Qemu => "qemu",
            VmKind::Lxc => "lxc",
        }
    }
}

/// Enough to address a guest in the API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VmRef {
    pub vmid: u32,
    pub node: String,
    pub kind: VmKind,
}

/// One entry of `GET /cluster/resources?type=vm`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct VmResource {
    pub vmid: u32,
    #[serde(default)]
    pub name: Option<String>,
    pub node: String,
    /// `running`, `stopped`, ... (`unknown` when the node is offline)
    pub status: String,
    #[serde(rename = "type")]
    pub kind: VmKind,
    #[serde(default, deserialize_with = "bool_from_int")]
    pub template: bool,
    /// `;`-separated
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub uptime: Option<u64>,
    #[serde(default)]
    pub lock: Option<String>,
    #[serde(default)]
    pub hastate: Option<String>,
}

impl VmResource {
    pub fn vm_ref(&self) -> VmRef {
        VmRef { vmid: self.vmid, node: self.node.clone(), kind: self.kind }
    }
}

fn bool_from_int<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrBool {
        Int(i64),
        Bool(bool),
    }
    Ok(match Option::<IntOrBool>::deserialize(d)? {
        Some(IntOrBool::Int(i)) => i != 0,
        Some(IntOrBool::Bool(b)) => b,
        None => false,
    })
}

/// `GET /version`
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Version {
    pub version: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub repoid: String,
}

/// Result of `POST .../vncproxy` (websocket=1): connect to `vncwebsocket` with `port` + `ticket`
/// within ~10 s, then authenticate VNC with `password`.
#[derive(Clone, PartialEq, Eq)]
pub struct VncProxy {
    pub port: u16,
    pub ticket: String,
    /// The generated one-time password (qemu, `generate-password=1`), else the ticket.
    pub password: String,
}

impl std::fmt::Debug for VncProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VncProxy").field("port", &self.port).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Start,
    /// ACPI shutdown (graceful)
    Shutdown,
    /// Hard power off
    Stop,
}

impl PowerAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerAction::Start => "start",
            PowerAction::Shutdown => "shutdown",
            PowerAction::Stop => "stop",
        }
    }
}

/// A guest's addresses, for the `{{ipv4}}` / `{{ipv6}}` placeholders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuestIps {
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

/// `GET /nodes/{node}/tasks/{upid}/status`
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TaskStatus {
    /// `running` or `stopped`
    pub status: String,
    /// `OK` or the error, once stopped
    #[serde(default)]
    pub exitstatus: Option<String>,
}

impl TaskStatus {
    pub fn is_done(&self) -> bool {
        self.status == "stopped"
    }

    pub fn is_ok(&self) -> bool {
        self.exitstatus.as_deref() == Some("OK")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resources() {
        let json = r#"[{"vmid":100,"name":"lab.web-1","node":"pve1","status":"running","type":"qemu","template":0,"tags":"lab","uptime":3600,"cpu":0.1},
                       {"vmid":200,"node":"pve3","status":"stopped","type":"lxc","template":1}]"#;
        let v: Vec<VmResource> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].vmid, 100);
        assert_eq!(v[0].kind, VmKind::Qemu);
        assert!(!v[0].template);
        assert_eq!(v[1].kind, VmKind::Lxc);
        assert!(v[1].template);
        assert_eq!(v[1].name, None);
    }
}
