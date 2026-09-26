//! "Type text": key strokes sent to several consoles in lock step, one character at a time.
//!
//! QEMU's PS/2 keyboard queue holds only a few bytes; bursts drop characters. So every console
//! gets character i, then there is a pause, then character i+1.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use rfb::ClientInput;
use tokio::sync::mpsc::UnboundedSender;

use crate::keys::Stroke;

pub struct Typing {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    total: usize,
    task: tokio::task::JoinHandle<()>,
}

impl Typing {
    pub fn start(
        rt: &tokio::runtime::Handle,
        jobs: Vec<(UnboundedSender<ClientInput>, Vec<Stroke>)>,
        delay: Duration,
        ctx: egui::Context,
    ) -> Self {
        let total = jobs.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let task = rt.spawn({
            let (stop, done) = (Arc::clone(&stop), Arc::clone(&done));
            async move {
                for i in 0..total {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    for (tx, strokes) in &jobs {
                        if let Some(s) = strokes.get(i) {
                            send_stroke(tx, *s);
                        }
                    }
                    done.store(i + 1, Ordering::Relaxed);
                    if i % 4 == 0 {
                        ctx.request_repaint();
                    }
                    tokio::time::sleep(delay).await;
                }
                ctx.request_repaint();
            }
        });
        Self { stop, done, total, task }
    }

    /// (characters typed, total)
    pub fn progress(&self) -> (usize, usize) {
        (self.done.load(Ordering::Relaxed), self.total)
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn was_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

impl Drop for Typing {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn send_stroke(tx: &UnboundedSender<ClientInput>, s: Stroke) {
    let key = |keysym, qnum, down| ClientInput::Key { keysym, qnum, down };
    if s.shift {
        let _ = tx.send(key(rfb::keysym::SHIFT_L, 0x2a, true));
    }
    let _ = tx.send(key(s.keysym, s.qnum, true));
    let _ = tx.send(key(s.keysym, s.qnum, false));
    if s.shift {
        let _ = tx.send(key(rfb::keysym::SHIFT_L, 0x2a, false));
    }
}

/// Whether the text uses an address placeholder (they need a guest-agent lookup first).
pub fn needs_ips(text: &str) -> bool {
    text.contains("{{ipv4}}") || text.contains("{{ipv6}}")
}

/// Per-VM placeholders of the text box: `{{vmid}}` `{{name}}` `{{node}}` `{{ipv4}}` `{{ipv6}}`.
pub fn fill_template(text: &str, vmid: u32, name: &str, node: &str, ips: Option<&pve::GuestIps>) -> String {
    let mut out = text.replace("{{vmid}}", &vmid.to_string()).replace("{{name}}", name).replace("{{node}}", node);
    if let Some(ips) = ips {
        out = out
            .replace("{{ipv4}}", ips.ipv4.as_deref().unwrap_or(""))
            .replace("{{ipv6}}", ips.ipv6.as_deref().unwrap_or(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full pipeline against `tools/mock_pve.py`: API -> vncproxy -> websocket -> RFB (QEMU extended
    /// key events) -> the mock decodes the scancodes back to text.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs `python3 tools/mock_pve.py` listening on 127.0.0.1:18006"]
    async fn types_into_mock_consoles() {
        use crate::console::{ConnState, Console};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let endpoint = pve::Endpoint {
            url: "http://127.0.0.1:18006".parse().unwrap(),
            token_id: "mock@pve!test".into(),
            token_secret: "unused".into(),
            pinned_sha256: None,
        };
        let client = pve::Client::new(&endpoint).unwrap();
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Handle::current();
        let consoles: Vec<(u32, Console)> = [9001, 9002]
            .into_iter()
            .map(|vmid| {
                (
                    vmid,
                    Console::open(
                        &rt,
                        crate::backend::Backend::Pve(client.clone()),
                        pve::VmRef { vmid, node: "pve1".into(), kind: pve::VmKind::Qemu },
                        ctx.clone(),
                    ),
                )
            })
            .collect();
        for _ in 0..100 {
            if consoles.iter().all(|(_, c)| c.state() == ConnState::Live) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(consoles.iter().all(|(_, c)| c.state() == ConnState::Live), "consoles did not connect");

        let marker = format!("e2e-{}", std::process::id());
        let text = format!("echo {marker} \"Q|~\" ${{HOME}}/{{a,b}}; 1+1=2\n");
        let (strokes, skipped) = crate::keys::text_strokes(&text);
        assert_eq!(skipped, 0);
        let jobs = consoles.iter().map(|(_, c)| (c.sender(), strokes.clone())).collect();
        let typing = Typing::start(&rt, jobs, Duration::from_millis(5), ctx);
        while !typing.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(typing.progress(), (strokes.len(), strokes.len()));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut tcp = tokio::net::TcpStream::connect("127.0.0.1:18006").await.unwrap();
        tcp.write_all(b"GET /mock/typed HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        tcp.read_to_string(&mut resp).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        for (vmid, _) in &consoles {
            let typed = body["typed"][vmid.to_string()].as_str().unwrap_or_default();
            assert!(typed.ends_with(&text), "VM {vmid} received {typed:?}");
        }
    }

    /// The same pipeline against the built-in demo cluster (no Python, no network): RFB with QEMU
    /// extended key events into two simulated VMs, whose shells echo and run what was typed.
    #[tokio::test(flavor = "multi_thread")]
    async fn types_into_demo_consoles() {
        use crate::backend::Backend;
        use crate::console::{ConnState, Console};

        let backend = Backend::demo().unwrap();
        let Backend::Demo(cluster) = &backend else { unreachable!() };
        let ctx = egui::Context::default();
        let rt = tokio::runtime::Handle::current();
        let consoles: Vec<(u32, Console)> = [(101, "pve1"), (102, "pve2")]
            .into_iter()
            .map(|(vmid, node)| {
                let vm = pve::VmRef { vmid, node: node.into(), kind: pve::VmKind::Qemu };
                (vmid, Console::open(&rt, backend.clone(), vm, ctx.clone()))
            })
            .collect();
        for _ in 0..100 {
            if consoles.iter().all(|(_, c)| c.state() == ConnState::Live) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(consoles.iter().all(|(_, c)| c.state() == ConnState::Live), "consoles did not connect");
        tokio::time::sleep(Duration::from_millis(200)).await; // the extended key announcement

        let marker = format!("e2e-{}", std::process::id());
        let text = format!("echo {marker} \"Q|~\" ${{HOME}}/{{a,b}}; 1+1=2\n");
        let (strokes, skipped) = crate::keys::text_strokes(&text);
        assert_eq!(skipped, 0);
        let jobs = consoles.iter().map(|(_, c)| (c.sender(), strokes.clone())).collect();
        let typing = Typing::start(&rt, jobs, Duration::from_millis(2), ctx);
        while !typing.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(typing.progress(), (strokes.len(), strokes.len()));
        for (vmid, name) in [(101, "web-01"), (102, "web-02")] {
            let want = format!(
                "root@{name}:~# {}\n{marker} Q|~ ${{HOME}}/{{a,b}}\n-bash: 1+1=2: command not found\nroot@{name}:~#",
                text.trim_end()
            );
            let mut screen = String::new();
            for _ in 0..100 {
                screen = cluster.screen_text(vmid).unwrap();
                if screen.ends_with(&want) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(screen.ends_with(&want), "VM {vmid} shows\n{screen}");
        }
    }

    #[test]
    fn templates() {
        let ips = pve::GuestIps { ipv4: Some("192.0.2.10".into()), ipv6: Some("2001:db8::10".into()) };
        assert_eq!(
            fill_template(
                "hostnamectl set-hostname {{name}} # {{vmid}} {{node}} {{ipv4}} [{{ipv6}}] ${name}",
                100,
                "web-1",
                "pve1",
                Some(&ips)
            ),
            "hostnamectl set-hostname web-1 # 100 pve1 192.0.2.10 [2001:db8::10] ${name}"
        );
        assert!(needs_ips("ping {{ipv4}}") && needs_ips("{{ipv6}}") && !needs_ips("{{name}}"));
        assert_eq!(fill_template("{{ipv4}}", 1, "a", "n", None), "{{ipv4}}");
    }
}
