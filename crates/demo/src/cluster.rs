//! The simulated cluster: per guest a power state, an uptime clock, a guest agent, a screen and
//! a shell; power tasks; slow steps (boot, shutdown, `apt update`) played back on tokio.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::DuplexStream;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::data::{GUESTS, Spec, spec};
use crate::glyphs::Glyphs;
use crate::shell::{self, Env, Shell, Step};
use crate::term::Term;
use crate::{Addresses, Fonts, Guest, Kind, Power};

/// Room in each console pipe; the RFB client reads concurrently, so this only paces writes.
const PIPE: usize = 256 * 1024;
const CTRL_L: u32 = 0xffe3;
const CTRL_R: u32 = 0xffe4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Dropping the cluster stops all of its consoles and running steps.
pub struct Cluster {
    vms: BTreeMap<u32, Arc<Vm>>,
    glyphs: Arc<Glyphs>,
    /// UPID -> when the task is done
    tasks: Mutex<HashMap<String, Instant>>,
    consoles: Mutex<Vec<AbortHandle>>,
    /// Varies latencies and task ids from call to call (deterministically).
    counter: AtomicU32,
}

pub(crate) struct Vm {
    pub spec: &'static Spec,
    state: Mutex<Machine>,
    /// Pinged after every visible change; consoles send an update if the client wants one.
    changed: watch::Sender<()>,
}

struct Script {
    handle: AbortHandle,
    /// Only a command's output (`apt update`) stops on ^C; boot and shutdown do not.
    interruptible: bool,
}

pub(crate) struct Machine {
    running: bool,
    /// Bumped at every power on and off: a console belongs to one power cycle.
    epoch: u64,
    up_base: u64,
    up_since: Instant,
    agent: bool,
    pub term: Term,
    shell: Shell,
    ctrl: bool,
    script: Option<Script>,
    scripts: u64,
    /// Keys typed while steps play, fed to the shell afterwards (like a tty's input buffer).
    queue: VecDeque<(u32, bool)>,
}

impl Cluster {
    pub fn new(fonts: Fonts) -> Result<Arc<Self>, String> {
        let glyphs = Arc::new(Glyphs::new(fonts.regular, fonts.bold)?);
        let now = Instant::now();
        let vms = GUESTS
            .iter()
            .map(|spec| {
                let mut term = Term::default();
                if spec.running {
                    term.write(&shell::banner(spec));
                    term.write(&shell::prompt(spec));
                }
                let machine = Machine {
                    running: spec.running,
                    epoch: 0,
                    // plus some minutes, so uptimes do not all end in :00
                    up_base: if spec.running { spec.uptime + u64::from(spec.seed() % 3541) } else { 0 },
                    up_since: now,
                    agent: spec.running,
                    term,
                    shell: Shell::new(spec),
                    ctrl: false,
                    script: None,
                    scripts: 0,
                    queue: VecDeque::new(),
                };
                (spec.vmid, Arc::new(Vm { spec, state: Mutex::new(machine), changed: watch::channel(()).0 }))
            })
            .collect();
        Ok(Arc::new(Self {
            vms,
            glyphs,
            tasks: Mutex::new(HashMap::new()),
            consoles: Mutex::new(Vec::new()),
            counter: AtomicU32::new(0),
        }))
    }

    fn vm(&self, vmid: u32) -> Result<&Arc<Vm>, String> {
        self.vms.get(&vmid).ok_or_else(|| format!("VM {vmid} does not exist"))
    }

    /// Like a real API call: 80-400 ms, varying per VM and per call.
    async fn latency(&self, vmid: u32) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let seed = spec(vmid).map_or(0, Spec::seed);
        let ms = 80 + (seed ^ n.wrapping_mul(7919)) % 321;
        tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
    }

    pub fn guests(&self) -> Vec<Guest> {
        self.vms
            .values()
            .map(|vm| {
                let m = vm.lock();
                let s = vm.spec;
                Guest {
                    vmid: s.vmid,
                    name: s.name,
                    node: s.node,
                    kind: s.kind,
                    running: m.running,
                    template: s.template,
                    tags: s.tags,
                    uptime: if m.running { m.uptime() } else { 0 },
                }
            })
            .collect()
    }

    /// Start a power action; returns the task's UPID (see [`Cluster::task_done`]).
    pub async fn power(&self, vmid: u32, action: Power) -> Result<String, String> {
        self.latency(vmid).await;
        let vm = self.vm(vmid)?;
        if vm.spec.template {
            return Err(format!("VM {vmid} is a template"));
        }
        let now = Instant::now();
        let (verb, done) = {
            let mut m = vm.lock();
            match action {
                Power::Start => {
                    if !m.running {
                        vm.power_on(&mut m);
                    }
                    ("start", now + Duration::from_secs(1))
                }
                Power::Shutdown => {
                    let steps = shell::shutdown(vm.spec, false);
                    let took = waits(&steps);
                    if m.running {
                        vm.start_script(&mut m, steps);
                    }
                    ("shutdown", now + took)
                }
                Power::Stop => {
                    let took = Duration::from_millis(500);
                    if m.running {
                        vm.start_script(&mut m, vec![Step::Wait(took), Step::PowerOff]);
                    }
                    ("stop", now + took)
                }
            }
        };
        vm.notify();
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let kind = match vm.spec.kind {
            Kind::Qemu => "qm",
            Kind::Lxc => "vz",
        };
        let upid = format!(
            "UPID:{}:{:08X}:{:08X}:{:08X}:{kind}{verb}:{vmid}:root@pam:",
            vm.spec.node,
            0x1000 + vmid * 7 + n,
            0x0040_0000 + n * 31,
            0x6700_0000 + n
        );
        lock(&self.tasks).insert(upid.clone(), done);
        Ok(upid)
    }

    /// `Some(true)` once the task has finished (always successfully), `None` for an unknown UPID.
    pub fn task_done(&self, upid: &str) -> Option<bool> {
        lock(&self.tasks).get(upid).map(|done| Instant::now() >= *done)
    }

    /// The guest agent's answer: fails while the VM is off or still booting.
    pub async fn addresses(&self, vmid: u32) -> Result<Addresses, String> {
        self.latency(vmid).await;
        let vm = self.vm(vmid)?;
        let m = vm.lock();
        if !m.running {
            return Err(format!("VM {vmid} is not running"));
        }
        if !m.agent {
            return Err(match vm.spec.kind {
                Kind::Qemu => "QEMU guest agent is not running".into(),
                Kind::Lxc => format!("CT {vmid} has no network interfaces yet"),
            });
        }
        Ok(Addresses { ipv4: Some(vm.spec.ipv4()), ipv6: Some(vm.spec.ipv6()) })
    }

    /// A new RFB connection to the guest's screen. It closes when the guest powers off.
    pub async fn open_console(&self, vmid: u32) -> Result<DuplexStream, String> {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let vm = self.vm(vmid)?;
        let epoch = {
            let m = vm.lock();
            if !m.running {
                return Err(format!("VM {vmid} not running"));
            }
            m.epoch
        };
        let (client, server) = tokio::io::duplex(PIPE);
        let task = tokio::spawn(crate::rfb_server::serve(server, Arc::clone(vm), Arc::clone(&self.glyphs), epoch));
        let mut consoles = lock(&self.consoles);
        consoles.retain(|h| !h.is_finished());
        consoles.push(task.abort_handle());
        Ok(client)
    }

    /// The guest's screen as text (for tests and scripted screenshots).
    pub fn screen_text(&self, vmid: u32) -> Option<String> {
        Some(self.vms.get(&vmid)?.lock().term.text())
    }

    /// Whether the guest is still playing output (booting, a command running, queued keys).
    pub fn busy(&self, vmid: u32) -> bool {
        self.vms.get(&vmid).is_some_and(|vm| {
            let m = vm.lock();
            m.script.is_some() || !m.queue.is_empty()
        })
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for h in lock(&self.consoles).drain(..) {
            h.abort();
        }
        for vm in self.vms.values() {
            let mut m = vm.lock();
            m.scripts += 1; // a step list between its lock and the abort must not go on
            m.queue.clear();
            if let Some(s) = m.script.take() {
                s.handle.abort();
            }
        }
    }
}

/// Total pause of a list of steps.
fn waits(steps: &[Step]) -> Duration {
    steps.iter().map(|s| if let Step::Wait(d) = s { *d } else { Duration::ZERO }).sum()
}

impl Vm {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Machine> {
        lock(&self.state)
    }

    fn notify(&self) {
        self.changed.send_replace(());
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Still the power cycle a console was opened in.
    pub(crate) fn alive(&self, epoch: u64) -> bool {
        let m = self.lock();
        m.running && m.epoch == epoch
    }

    /// A key from a console (X11 keysym).
    pub(crate) fn key(self: &Arc<Self>, keysym: u32, down: bool) {
        {
            let mut m = self.lock();
            if matches!(keysym, CTRL_L | CTRL_R) {
                m.ctrl = down;
                return;
            }
            if !down || !m.running {
                return;
            }
            let ctrl = m.ctrl;
            let is_ctrl_c = ctrl && matches!(keysym, 0x63 | 0x43);
            match m.script.as_ref().map(|s| s.interruptible) {
                Some(true) if is_ctrl_c => {
                    if let Some(s) = m.script.take() {
                        s.handle.abort();
                    }
                    m.scripts += 1; // a step list waiting for the lock must not go on
                    m.queue.clear();
                    m.shell.reset_line();
                    m.term.write("^C\n");
                    m.term.write(&shell::prompt(self.spec));
                }
                Some(_) => {
                    if m.queue.len() < 4096 {
                        m.queue.push_back((keysym, ctrl));
                    }
                    return;
                }
                None => self.feed(&mut m, keysym, ctrl),
            }
        }
        self.notify();
    }

    fn feed(self: &Arc<Self>, m: &mut Machine, keysym: u32, ctrl: bool) {
        let env = m.env();
        let steps = m.shell.key(keysym, ctrl, &mut m.term, &env);
        if steps.iter().any(|s| matches!(s, Step::Wait(_))) {
            self.start_script(m, steps);
        } else {
            for step in steps {
                m.apply(step, self.spec);
            }
        }
    }

    fn power_on(self: &Arc<Self>, m: &mut Machine) {
        m.running = true;
        m.epoch += 1;
        m.up_base = 0;
        m.up_since = Instant::now();
        m.agent = false;
        m.term.clear();
        m.shell.reset_line();
        self.start_script(m, shell::boot(self.spec));
    }

    /// Play `steps` on the runtime, replacing whatever was playing.
    fn start_script(self: &Arc<Self>, m: &mut Machine, steps: Vec<Step>) {
        if let Some(old) = m.script.take() {
            old.handle.abort();
        }
        m.scripts += 1;
        let id = m.scripts;
        let interruptible = steps.iter().all(|s| matches!(s, Step::Out(_) | Step::Wait(_) | Step::Prompt));
        let vm = Arc::clone(self);
        let task = tokio::spawn(async move {
            let mut steps = steps.into_iter();
            loop {
                let wait = {
                    let mut m = vm.lock();
                    if m.scripts != id {
                        return; // replaced (the abort may arrive after the lock was released)
                    }
                    let mut wait = None;
                    for step in steps.by_ref() {
                        if let Step::Wait(d) = step {
                            wait = Some(d);
                            break;
                        }
                        m.apply(step, vm.spec);
                    }
                    if wait.is_none() {
                        m.script = None;
                        vm.drain_queue(&mut m);
                    }
                    wait
                };
                vm.notify();
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => return,
                }
            }
        });
        m.script = Some(Script { handle: task.abort_handle(), interruptible });
    }

    /// Feed the keys typed meanwhile, until one of them starts slow steps again.
    fn drain_queue(self: &Arc<Self>, m: &mut Machine) {
        while m.script.is_none() && m.running {
            let Some((keysym, ctrl)) = m.queue.pop_front() else { break };
            self.feed(m, keysym, ctrl);
        }
    }
}

impl Machine {
    fn uptime(&self) -> u64 {
        self.up_base + self.up_since.elapsed().as_secs()
    }

    fn env(&self) -> Env {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Env { uptime: self.uptime(), now }
    }

    fn apply(&mut self, step: Step, spec: &Spec) {
        match step {
            Step::Out(text) => self.term.write(&text),
            Step::Wait(_) => {}
            Step::Clear => self.term.clear(),
            Step::Prompt => self.term.write(&shell::prompt(spec)),
            Step::AgentDown => self.agent = false,
            Step::AgentUp => self.agent = true,
            Step::PowerOff => {
                self.running = false;
                self.epoch += 1;
                self.agent = false;
                self.ctrl = false;
                self.queue.clear();
                self.term.clear();
                self.shell.reset_line();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster() -> Arc<Cluster> {
        Cluster::new(Fonts {
            regular: include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
            bold: include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
        })
        .unwrap()
    }

    fn guest(c: &Cluster, vmid: u32) -> Guest {
        c.guests().into_iter().find(|g| g.vmid == vmid).unwrap()
    }

    fn type_text(c: &Cluster, vmid: u32, text: &str) {
        let vm = c.vm(vmid).unwrap();
        for ch in text.chars() {
            let keysym = if ch == '\n' { 0xff0d } else { ch as u32 };
            vm.key(keysym, true);
            vm.key(keysym, false);
        }
    }

    #[test]
    fn the_guest_table() {
        let c = cluster();
        let guests = c.guests();
        assert_eq!(guests.len(), 14);
        assert_eq!(guests.iter().filter(|g| !g.template).count(), 13);
        assert_eq!(guests.iter().filter(|g| g.running).count(), 11);
        let web = guest(&c, 101);
        assert_eq!((web.name, web.node, web.tags, web.kind), ("web-01", "pve1", "prod;web", Kind::Qemu));
        assert!(web.uptime > 20 * 86_400);
        assert!(!guest(&c, 131).running && guest(&c, 9000).template);
        assert!(c.screen_text(101).unwrap().ends_with("root@web-01:~#"));
    }

    #[tokio::test(start_paused = true)]
    async fn power_cycle_tasks_and_the_guest_agent() {
        let c = cluster();
        assert_eq!(c.addresses(131).await, Err("VM 131 is not running".into()));
        assert!(c.open_console(131).await.is_err());

        let upid = c.power(131, Power::Start).await.unwrap();
        assert!(upid.starts_with("UPID:pve3:") && upid.contains(":qmstart:131:root@pam:"), "{upid}");
        assert!(guest(&c, 131).running, "running at once");
        assert_eq!(c.task_done(&upid), Some(false));
        assert!(c.addresses(131).await.is_err(), "no agent while booting");
        assert!(c.busy(131));
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert_eq!(c.task_done(&upid), Some(true));
        assert!(!c.busy(131));
        assert!(c.screen_text(131).unwrap().ends_with("root@ci-runner-1:~#"), "{:?}", c.screen_text(131));
        let ips = c.addresses(131).await.unwrap();
        assert_eq!(ips.ipv4.as_deref(), Some("198.51.100.31"));
        assert_eq!(ips.ipv6.as_deref(), Some("2001:db8:20::31"));

        let console = c.open_console(131).await.unwrap();
        let upid = c.power(131, Power::Shutdown).await.unwrap();
        assert!(guest(&c, 131).running, "still shutting down");
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!guest(&c, 131).running);
        assert_eq!(c.task_done(&upid), Some(true));
        assert_eq!(c.task_done("UPID:nope"), None);
        // the console's server side is gone: reading ends
        let mut console = console;
        let mut buf = Vec::new();
        let read = tokio::io::AsyncReadExt::read_to_end(&mut console, &mut buf);
        tokio::time::timeout(Duration::from_secs(5), read).await.unwrap().unwrap();

        c.power(101, Power::Stop).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!guest(&c, 101).running);
        assert_eq!(guest(&c, 101).uptime, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn keys_typed_during_output_wait_and_ctrl_c_interrupts() {
        let c = cluster();
        type_text(&c, 102, "apt update\nhostname\n");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(c.busy(102));
        assert!(!c.screen_text(102).unwrap().contains("# hostname"), "queued, not echoed yet");
        tokio::time::sleep(Duration::from_secs(8)).await;
        let text = c.screen_text(102).unwrap();
        assert!(text.contains("Reading package lists... Done") && text.ends_with("# hostname\nweb-02\nroot@web-02:~#"));

        type_text(&c, 103, "apt update\n");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let vm = c.vm(103).unwrap();
        assert!(c.busy(103));
        vm.key(CTRL_L, true);
        type_text(&c, 103, "c");
        vm.key(CTRL_L, false);
        assert!(!c.busy(103));
        assert!(c.screen_text(103).unwrap().ends_with("^C\nroot@web-03:~#"), "{:?}", c.screen_text(103));

        type_text(&c, 111, "poweroff\n");
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!guest(&c, 111).running);
    }
}
