//! What a session talks to: a real Proxmox cluster, or the built-in demo cluster (no network).
//! Both answer with the `pve` types, so the rest of the app does not care which it is.

use std::sync::Arc;

use pve::{GuestIps, PowerAction, TaskStatus, VmKind, VmRef, VmResource};

use crate::theme;

#[derive(Clone)]
pub enum Backend {
    Pve(pve::Client),
    Demo(Arc<demo::Cluster>),
}

/// Title of the demo session (menu and window title).
pub const DEMO_NAME: &str = "Demo cluster";

impl Backend {
    /// A fresh demo cluster; its consoles are drawn with the app's own JetBrains Mono.
    pub fn demo() -> Result<Self, String> {
        let fonts = demo::Fonts { regular: theme::JETBRAINS_MONO, bold: theme::JETBRAINS_MONO_BOLD };
        Ok(Backend::Demo(demo::Cluster::new(fonts)?))
    }

    pub fn is_demo(&self) -> bool {
        matches!(self, Backend::Demo(_))
    }

    /// `host:port` shown in the top bar.
    pub fn host_label(&self) -> String {
        match self {
            Backend::Pve(client) => {
                let url = client.base_url();
                match (url.host_str(), url.port()) {
                    (Some(h), Some(p)) => format!("{h}:{p}"),
                    (Some(h), None) => h.to_owned(),
                    (None, _) => String::new(),
                }
            }
            Backend::Demo(_) => "demo · offline".into(),
        }
    }

    pub async fn vms(&self) -> Result<Vec<VmResource>, String> {
        match self {
            Backend::Pve(client) => client.vms().await.map_err(|e| e.to_string()),
            Backend::Demo(cluster) => Ok(cluster.guests().into_iter().map(resource).collect()),
        }
    }

    /// Returns the task's UPID.
    pub async fn power(&self, vm: &VmRef, action: PowerAction) -> Result<String, String> {
        match self {
            Backend::Pve(client) => client.power(vm, action).await.map_err(|e| e.to_string()),
            Backend::Demo(cluster) => {
                let action = match action {
                    PowerAction::Start => demo::Power::Start,
                    PowerAction::Shutdown => demo::Power::Shutdown,
                    PowerAction::Stop => demo::Power::Stop,
                };
                cluster.power(vm.vmid, action).await
            }
        }
    }

    pub async fn task_status(&self, node: &str, upid: &str) -> Result<TaskStatus, String> {
        match self {
            Backend::Pve(client) => client.task_status(node, upid).await.map_err(|e| e.to_string()),
            Backend::Demo(cluster) => match cluster.task_done(upid) {
                Some(true) => Ok(TaskStatus { status: "stopped".into(), exitstatus: Some("OK".into()) }),
                Some(false) => Ok(TaskStatus { status: "running".into(), exitstatus: None }),
                None => Err(format!("no such task: {upid}")),
            },
        }
    }

    pub async fn guest_ips(&self, vm: &VmRef) -> Result<GuestIps, String> {
        match self {
            Backend::Pve(client) => client.guest_ips(vm).await.map_err(|e| e.to_string()),
            Backend::Demo(cluster) => {
                let a = cluster.addresses(vm.vmid).await?;
                Ok(GuestIps { ipv4: a.ipv4, ipv6: a.ipv6 })
            }
        }
    }
}

/// A demo guest as `GET /cluster/resources` would list it.
fn resource(g: demo::Guest) -> VmResource {
    VmResource {
        vmid: g.vmid,
        name: Some(g.name.to_owned()),
        node: g.node.to_owned(),
        status: if g.running { "running" } else { "stopped" }.to_owned(),
        kind: match g.kind {
            demo::Kind::Qemu => VmKind::Qemu,
            demo::Kind::Lxc => VmKind::Lxc,
        },
        template: g.template,
        tags: (!g.tags.is_empty()).then(|| g.tags.to_owned()),
        uptime: g.running.then_some(g.uptime),
        lock: None,
        hastate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::matches;

    #[tokio::test]
    async fn demo_guests_as_resources() {
        let backend = Backend::demo().unwrap();
        let vms = backend.vms().await.unwrap();
        assert_eq!(vms.len(), 14);
        let web = vms.iter().find(|v| v.vmid == 101).unwrap();
        assert_eq!(web.name.as_deref(), Some("web-01"));
        assert_eq!((web.status.as_str(), web.kind, web.tags.as_deref()), ("running", VmKind::Qemu, Some("prod;web")));
        assert!(web.uptime.unwrap() > 0);
        let ct = vms.iter().find(|v| v.vmid == 201).unwrap();
        assert_eq!(ct.kind, VmKind::Lxc);
        let off = vms.iter().find(|v| v.vmid == 131).unwrap();
        assert_eq!((off.status.as_str(), off.uptime), ("stopped", None));
        assert!(vms.iter().find(|v| v.vmid == 9000).unwrap().template);

        // the picker filters from the design: "prod web" 3, "k8s" 4, "pve3" 4, "db, dns" 3
        let count = |f: &str| vms.iter().filter(|v| !v.template && matches(v, f)).count();
        assert_eq!((count("prod web"), count("k8s"), count("pve3"), count("db, dns")), (3, 4, 4, 3));
        assert_eq!(backend.host_label(), "demo · offline");

        let ips = backend.guest_ips(&web.vm_ref()).await.unwrap();
        assert_eq!(ips, GuestIps { ipv4: Some("192.0.2.11".into()), ipv6: Some("2001:db8:10::11".into()) });
        let upid = backend.power(&off.vm_ref(), PowerAction::Start).await.unwrap();
        assert!(!backend.task_status("pve3", &upid).await.unwrap().is_done());
    }
}
