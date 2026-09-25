//! Pure helpers for Proxmox API response bodies (no I/O).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

use std::net::IpAddr;

use crate::{Error, GuestIps, VmResource, VncProxy};

/// `agent/network-get-interfaces` data: `{"result": [{"name": .., "ip-addresses": [..]}]}`
#[derive(Deserialize)]
pub(crate) struct AgentInterfaces {
    pub result: Vec<AgentInterface>,
}

#[derive(Deserialize)]
pub(crate) struct AgentInterface {
    pub name: String,
    #[serde(default, rename = "ip-addresses")]
    pub addresses: Vec<AgentAddress>,
}

#[derive(Deserialize)]
pub(crate) struct AgentAddress {
    #[serde(rename = "ip-address")]
    pub address: String,
}

/// `lxc/{vmid}/interfaces` entry (`inet` / `inet6` with a prefix, e.g. `10.0.0.2/24`)
#[derive(Deserialize)]
pub(crate) struct LxcInterface {
    pub name: String,
    #[serde(default)]
    pub inet: Option<String>,
    #[serde(default)]
    pub inet6: Option<String>,
}

/// Interfaces that are not the guest's own network (loopback, container / VM bridges).
fn virtual_interface(name: &str) -> bool {
    const PREFIXES: [&str; 12] =
        ["lo", "docker", "br-", "veth", "virbr", "lxc", "cni", "flannel", "cali", "vxlan", "tun", "wg"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// First global IPv4 and IPv6 (no loopback, link-local or unspecified), preferring real interfaces.
pub(crate) fn pick_ips(interfaces: impl Iterator<Item = (String, Vec<String>)>) -> GuestIps {
    let mut all: Vec<(bool, IpAddr)> = Vec::new();
    for (name, addrs) in interfaces {
        for a in addrs {
            let ip = a.split('/').next().unwrap_or("").trim();
            if let Ok(ip) = ip.parse::<IpAddr>() {
                all.push((virtual_interface(&name), ip));
            }
        }
    }
    let global = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()),
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80),
    };
    let pick = |v6: bool| {
        let usable = |virt: bool| {
            all.iter().find(|(vi, ip)| *vi == virt && ip.is_ipv6() == v6 && global(ip)).map(|(_, ip)| ip.to_string())
        };
        usable(false).or_else(|| usable(true))
    };
    GuestIps { ipv4: pick(false), ipv6: pick(true) }
}

/// Every successful response: `{"data": ...}`.
#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

/// Decodes the `data` member of a successful response.
pub(crate) fn decode_data<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice::<Envelope<T>>(body).map(|envelope| envelope.data).map_err(|e| Error::Decode(e.to_string()))
}

/// Error body: `{"data":null,"message":"...\n","errors":{"param":"reason"}}` (all optional).
#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The text for [`Error::Api`]: the body's `message` plus its `errors` as `key: value` pairs,
/// or else the HTTP reason phrase (Proxmox puts e.g. `invalid token value!` only there).
pub(crate) fn error_message(body: &[u8], reason: &str) -> String {
    let parsed = serde_json::from_slice::<ErrorBody>(body).ok();
    let message = parsed.as_ref().and_then(|b| b.message.as_deref()).map(str::trim).unwrap_or("");
    let errors = parsed.as_ref().and_then(|b| b.errors.as_ref()).map(format_errors).unwrap_or_default();
    match (message.is_empty(), errors.is_empty()) {
        (false, false) => format!("{message} ({errors})"),
        (false, true) => message.to_owned(),
        (true, false) => errors,
        (true, true) if !reason.trim().is_empty() => reason.trim().to_owned(),
        (true, true) => "no details given".to_owned(),
    }
}

fn format_errors(errors: &serde_json::Map<String, serde_json::Value>) -> String {
    errors
        .iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(s) => format!("{key}: {}", s.trim()),
            other => format!("{key}: {other}"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `data` of `POST .../vncproxy`.
#[derive(Deserialize)]
pub(crate) struct VncProxyData {
    #[serde(deserialize_with = "port_from_int_or_string")]
    port: u16,
    ticket: String,
    /// Only with `generate-password=1` (qemu).
    #[serde(default)]
    password: Option<String>,
}

impl From<VncProxyData> for VncProxy {
    fn from(data: VncProxyData) -> Self {
        let password = data.password.filter(|p| !p.is_empty()).unwrap_or_else(|| data.ticket.clone());
        VncProxy { port: data.port, ticket: data.ticket, password }
    }
}

/// Proxmox sends the port as a number, some versions as a string.
fn port_from_int_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Port {
        Int(u64),
        Str(String),
    }
    let port = match Port::deserialize(d)? {
        Port::Int(n) => u16::try_from(n).ok(),
        Port::Str(s) => s.trim().parse::<u16>().ok(),
    };
    port.ok_or_else(|| serde::de::Error::custom("port is not a valid TCP port number"))
}

/// `GET /cluster/resources?type=vm` entries; entries of an unexpected shape are skipped (and
/// logged) instead of failing the whole list, unless nothing at all could be parsed.
pub(crate) fn parse_resources(entries: Vec<serde_json::Value>) -> Result<Vec<VmResource>, Error> {
    let total = entries.len();
    let mut first_error = None;
    let vms: Vec<VmResource> = entries
        .into_iter()
        .filter_map(|entry| match serde_json::from_value::<VmResource>(entry) {
            Ok(vm) => Some(vm),
            Err(e) => {
                tracing::warn!(error = %e, "skipping a cluster resource of unexpected shape");
                first_error.get_or_insert(e);
                None
            }
        })
        .collect();
    match first_error {
        Some(e) if vms.is_empty() && total > 0 => Err(Error::Decode(e.to_string())),
        _ => Ok(vms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_agent_ips() {
        let body = br#"{"data":{"result":[
            {"name":"lo","ip-addresses":[{"ip-address-type":"ipv4","ip-address":"127.0.0.1","prefix":8},{"ip-address-type":"ipv6","ip-address":"::1","prefix":128}]},
            {"name":"docker0","ip-addresses":[{"ip-address-type":"ipv4","ip-address":"172.17.0.1","prefix":16}]},
            {"name":"ens18","hardware-address":"bc:24:11:00:00:01","ip-addresses":[
                {"ip-address-type":"ipv4","ip-address":"192.0.2.10","prefix":25},
                {"ip-address-type":"ipv6","ip-address":"fe80::1","prefix":64},
                {"ip-address-type":"ipv6","ip-address":"2001:db8::10","prefix":64}]}]}}"#;
        let data: AgentInterfaces = decode_data(body).unwrap();
        let ips =
            pick_ips(data.result.into_iter().map(|i| (i.name, i.addresses.into_iter().map(|a| a.address).collect())));
        assert_eq!(ips, GuestIps { ipv4: Some("192.0.2.10".into()), ipv6: Some("2001:db8::10".into()) });
    }

    #[test]
    fn lxc_ips_and_fallbacks() {
        let body = br#"{"data":[{"name":"lo","inet":"127.0.0.1/8","inet6":"::1/128"},{"name":"eth0","inet":"10.0.0.2/24","inet6":"fe80::2/64","hwaddr":"x"}]}"#;
        let data: Vec<LxcInterface> = decode_data(body).unwrap();
        let ips = pick_ips(data.into_iter().map(|i| (i.name, [i.inet, i.inet6].into_iter().flatten().collect())));
        assert_eq!(ips, GuestIps { ipv4: Some("10.0.0.2".into()), ipv6: None });
        // only a bridge address: better than nothing
        let ips = pick_ips([("docker0".to_owned(), vec!["172.17.0.1".to_owned()])].into_iter());
        assert_eq!(ips.ipv4.as_deref(), Some("172.17.0.1"));
    }
    use crate::Version;

    #[test]
    fn message_from_500_body() {
        let body = br#"{"data":null,"message":"Configuration file 'nodes/pve2/qemu-server/1.conf' does not exist\n"}"#;
        assert_eq!(
            error_message(body, "Internal Server Error"),
            "Configuration file 'nodes/pve2/qemu-server/1.conf' does not exist"
        );
    }

    #[test]
    fn message_and_errors_from_400_body() {
        let body = br#"{"errors":{"type":"value 'x' does not have a value in the enumeration 'vm, storage, node, sdn'"},"message":"Parameter verification failed.\n","data":null}"#;
        assert_eq!(
            error_message(body, "Parameter verification failed."),
            "Parameter verification failed. (type: value 'x' does not have a value in the enumeration 'vm, storage, node, sdn')"
        );
    }

    #[test]
    fn several_errors_are_joined() {
        let body = br#"{"errors":{"b":"second\n","a":1},"data":null}"#;
        assert_eq!(error_message(body, "Bad Request"), "a: 1; b: second");
    }

    #[test]
    fn empty_401_body_uses_the_reason_phrase() {
        assert_eq!(error_message(b"", "invalid token value!"), "invalid token value!");
        assert_eq!(error_message(b"", "  "), "no details given");
        assert_eq!(error_message(b"<html>proxy error</html>", "Bad Gateway"), "Bad Gateway");
        assert_eq!(error_message(br#"{"data":null,"message":"  \n"}"#, "Forbidden"), "Forbidden");
    }

    #[test]
    fn envelope_decoding() {
        let v: Version = decode_data(br#"{"data":{"version":"8.4.14","release":"8.4","repoid":"x"}}"#).unwrap();
        assert_eq!(v.version, "8.4.14");
        let upid: String = decode_data(br#"{"data":"UPID:pve2:1:2:3:qmstart:101:root@pam:"}"#).unwrap();
        assert!(upid.starts_with("UPID:"));
        assert!(matches!(decode_data::<Version>(br#"{"data":null}"#), Err(Error::Decode(_))));
        assert!(matches!(decode_data::<Version>(b"not json"), Err(Error::Decode(_))));
    }

    fn proxy(json: &str) -> Result<VncProxy, Error> {
        decode_data::<VncProxyData>(json.as_bytes()).map(VncProxy::from)
    }

    #[test]
    fn vnc_proxy_with_int_port_and_password() {
        let p = proxy(r#"{"data":{"port":5900,"ticket":"PVEVNC:abc::sig","password":"Xy12Ab34","upid":"UPID:x","cert":"-----","user":"root@pam!t"}}"#).unwrap();
        assert_eq!(p, VncProxy { port: 5900, ticket: "PVEVNC:abc::sig".into(), password: "Xy12Ab34".into() });
    }

    #[test]
    fn vnc_proxy_with_string_port_without_password() {
        let p = proxy(r#"{"data":{"port":"5901","ticket":"PVEVNC:def::sig","upid":"UPID:y"}}"#).unwrap();
        assert_eq!(p.port, 5901);
        assert_eq!(p.password, "PVEVNC:def::sig", "the ticket doubles as the password");
    }

    #[test]
    fn vnc_proxy_rejects_bad_ports() {
        assert!(matches!(proxy(r#"{"data":{"port":70000,"ticket":"t"}}"#), Err(Error::Decode(_))));
        assert!(matches!(proxy(r#"{"data":{"port":"x","ticket":"t"}}"#), Err(Error::Decode(_))));
        assert!(matches!(proxy(r#"{"data":{"ticket":"t"}}"#), Err(Error::Decode(_))));
    }

    #[test]
    fn resources_skip_odd_entries() {
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"vmid":1,"node":"a","status":"running","type":"qemu"},{"vmid":"weird"}]"#)
                .unwrap();
        let vms = parse_resources(entries).unwrap();
        assert_eq!(vms.len(), 1);
        assert!(parse_resources(Vec::new()).unwrap().is_empty());
        let only_bad = vec![serde_json::json!({"id": "storage/x"})];
        assert!(matches!(parse_resources(only_bad), Err(Error::Decode(_))));
    }
}
