//! The demo guests and their address plan.
//!
//! Only reserved documentation values: IPv4 from RFC 5737, IPv6 from RFC 3849 (plus link-local),
//! MAC addresses from the RFC 7042 / RFC 9542 documentation block, names under `.example`.

use crate::Kind;

/// Which documentation network a guest lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Net {
    Prod,
    Staging,
    Infra,
}

impl Net {
    fn v4(self) -> &'static str {
        match self {
            Net::Prod => "192.0.2",
            Net::Staging => "198.51.100",
            Net::Infra => "203.0.113",
        }
    }

    fn v6(self) -> &'static str {
        match self {
            Net::Prod => "2001:db8:10",
            Net::Staging => "2001:db8:20",
            Net::Infra => "2001:db8:30",
        }
    }
}

/// One guest as it is when the demo starts.
#[derive(Debug)]
pub(crate) struct Spec {
    pub vmid: u32,
    pub name: &'static str,
    pub node: &'static str,
    pub kind: Kind,
    pub running: bool,
    pub template: bool,
    pub tags: &'static str,
    pub net: Net,
    /// Last address byte (also the last MAC byte).
    pub host: u8,
    /// Uptime when the demo starts, seconds.
    pub uptime: u64,
}

const DAY: u64 = 86_400;
const HOUR: u64 = 3_600;

#[rustfmt::skip]
pub(crate) const GUESTS: [Spec; 14] = [
    guest(101, "web-01", "pve1", Kind::Qemu, true, "prod;web", Net::Prod, 11, 23 * DAY + 4 * HOUR),
    guest(102, "web-02", "pve2", Kind::Qemu, true, "prod;web", Net::Prod, 12, 23 * DAY + 3 * HOUR),
    guest(103, "web-03", "pve3", Kind::Qemu, true, "prod;web", Net::Prod, 13, 9 * DAY + 17 * HOUR),
    guest(111, "db-01", "pve1", Kind::Qemu, true, "prod;db", Net::Prod, 21, 41 * DAY + 6 * HOUR),
    guest(112, "db-02", "pve2", Kind::Qemu, true, "prod;db", Net::Prod, 22, 41 * DAY + 5 * HOUR),
    guest(121, "k8s-cp-1", "pve1", Kind::Qemu, true, "k8s", Net::Prod, 31, 12 * DAY + 2 * HOUR),
    guest(122, "k8s-worker-1", "pve1", Kind::Qemu, true, "k8s", Net::Prod, 41, 12 * DAY + HOUR),
    guest(123, "k8s-worker-2", "pve2", Kind::Qemu, true, "k8s", Net::Prod, 42, 12 * DAY + HOUR),
    guest(124, "k8s-worker-3", "pve3", Kind::Qemu, true, "k8s", Net::Prod, 43, 5 * DAY + 20 * HOUR),
    guest(131, "ci-runner-1", "pve3", Kind::Qemu, false, "ci;staging", Net::Staging, 31, 0),
    guest(132, "staging-app-1", "pve2", Kind::Qemu, false, "staging;web", Net::Staging, 21, 0),
    guest(201, "dns-01", "pve2", Kind::Lxc, true, "infra;dns", Net::Infra, 53, 97 * DAY + 11 * HOUR),
    guest(202, "monitor-01", "pve3", Kind::Lxc, true, "infra;monitoring", Net::Infra, 60, 64 * DAY + 8 * HOUR),
    Spec {
        vmid: 9000, name: "tpl-base-12", node: "pve1", kind: Kind::Qemu, running: false, template: true,
        tags: "", net: Net::Staging, host: 250, uptime: 0,
    },
];

/// The grid shown when the demo starts: the production web, database and k8s VMs.
pub const DEFAULT_GRID: [u32; 9] = [101, 102, 103, 111, 112, 121, 122, 123, 124];

#[allow(clippy::too_many_arguments)]
const fn guest(
    vmid: u32,
    name: &'static str,
    node: &'static str,
    kind: Kind,
    running: bool,
    tags: &'static str,
    net: Net,
    host: u8,
    uptime: u64,
) -> Spec {
    Spec { vmid, name, node, kind, running, template: false, tags, net, host, uptime }
}

impl Spec {
    pub fn ipv4(&self) -> String {
        format!("{}.{}", self.net.v4(), self.host)
    }

    pub fn gateway(&self) -> String {
        format!("{}.1", self.net.v4())
    }

    pub fn ipv6(&self) -> String {
        format!("{}::{}", self.net.v6(), self.host)
    }

    fn mac_byte(&self) -> u8 {
        // unique per guest: its position in the table
        GUESTS.iter().position(|g| g.vmid == self.vmid).map_or(0xff, |i| i as u8 + 1)
    }

    pub fn mac(&self) -> String {
        format!("00:00:5e:00:53:{:02x}", self.mac_byte())
    }

    /// EUI-64 link-local address of [`Spec::mac`].
    pub fn link_local(&self) -> String {
        format!("fe80::200:5eff:fe00:53{:02x}", self.mac_byte())
    }

    pub fn fqdn(&self) -> String {
        format!("{}.lab.example", self.name)
    }

    pub fn iface(&self) -> &'static str {
        match self.kind {
            Kind::Qemu => "ens18",
            Kind::Lxc => "eth0",
        }
    }

    /// A small number that differs per guest: timing jitter and output variety, no randomness.
    pub fn seed(&self) -> u32 {
        self.vmid.wrapping_mul(2_654_435_761) >> 7
    }
}

pub(crate) fn spec(vmid: u32) -> Option<&'static Spec> {
    GUESTS.iter().find(|g| g.vmid == vmid)
}
