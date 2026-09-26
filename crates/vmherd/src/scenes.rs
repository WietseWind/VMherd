//! Store screenshots, only in builds with `--features store-shots` (never in releases):
//! `vmherd --demo --scene NAME --out FILE.png` drives the demo into one scene, waits until its
//! consoles have settled, saves the window (see `handle_screenshot`) and quits.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pve::PowerAction;

use super::{App, Screenshot, Target};
use crate::backend::Backend;
use crate::{bar, tile};

pub const NAMES: [&str; 8] = ["grid", "broadcast", "placeholders", "picker", "solo", "power", "maximized", "home"];

/// Save whatever is there after this long.
const TIMEOUT: Duration = Duration::from_secs(60);
/// 12 consoles: everything but a stopped VM and the template (131 is started first).
const GRID: [u32; 12] = [101, 102, 103, 111, 112, 121, 122, 123, 124, 131, 201, 202];
/// The stopped 132 top right (toasts cover the bottom right), 131 boots bottom middle.
const POWER_GRID: [u32; 9] = [101, 102, 132, 111, 112, 121, 122, 131, 103];

pub struct Scene {
    name: String,
    out: PathBuf,
    step: u32,
    /// When the current step began.
    since: Instant,
    started: Instant,
    /// Toasts were cleared last frame (they must be gone from the frame that is saved).
    cleared: bool,
    /// Last time the window was asked to come to the front.
    raised: Option<Instant>,
}

/// What a scene step wants.
enum Go {
    Wait,
    Next,
    Shoot,
}

impl Scene {
    pub fn new(name: &str, out: PathBuf) -> Self {
        let now = Instant::now();
        Self { name: name.to_owned(), out, step: 0, since: now, started: now, cleared: false, raised: None }
    }
}

impl App {
    pub(super) fn scene_wants_demo(&self) -> bool {
        self.scene.as_ref().is_none_or(|s| s.name != "home")
    }

    fn cluster(&self) -> Option<Arc<demo::Cluster>> {
        match &self.session.as_ref()?.backend {
            Backend::Demo(c) => Some(Arc::clone(c)),
            Backend::Pve(_) => None,
        }
    }

    fn loaded(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.loaded)
    }

    /// Every console of a running VM is live, no guest is busy (booting, output playing) and
    /// nothing is being typed.
    fn settled(&self) -> bool {
        let Some(c) = self.cluster() else { return false };
        self.loaded()
            && self.typing.is_none()
            && !self.tiles.is_empty()
            && self.tiles.iter().all(|t| (t.vm.status != "running" || t.is_live()) && !c.busy(t.vmid()))
    }

    fn all_live(&self) -> bool {
        self.tiles.iter().all(|t| t.is_live())
    }

    fn type_all(&mut self, text: &str) {
        self.type_text(Target::Broadcast, |_| Some(text.to_owned()));
    }

    /// Power on without the toasts of the power buttons.
    fn start_quietly(&self, vmid: u32) {
        if let Some(c) = self.cluster() {
            self.rt.spawn(async move {
                let _ = c.power(vmid, demo::Power::Start).await;
            });
        }
    }

    pub(super) fn drive_scene(&mut self, ctx: &egui::Context) {
        let Some(scene) = &mut self.scene else { return };
        if self.screenshot.is_some() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(50));
        // focused widgets (the ON AIR box) only look focused in the frontmost window
        if !ctx.input(|i| i.focused) && scene.raised.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            scene.raised = Some(Instant::now());
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let (name, step, waited) = (scene.name.clone(), scene.step, scene.since.elapsed());
        let go = if scene.started.elapsed() > TIMEOUT {
            eprintln!("scene {name}: still not settled after {TIMEOUT:?}, saving anyway");
            Go::Shoot
        } else {
            // the VM list follows power changes quickly
            if let Some(s) = &mut self.session {
                s.next_poll = s.next_poll.min(Instant::now() + Duration::from_millis(500));
            }
            self.scene_step(ctx, &name, step, waited)
        };
        let Some(scene) = &mut self.scene else { return };
        match go {
            Go::Wait => {}
            Go::Next => {
                scene.step += 1;
                scene.since = Instant::now();
            }
            Go::Shoot if !scene.cleared && scene.name != "power" => {
                scene.cleared = true;
                self.toasts.clear();
            }
            Go::Shoot => {
                self.screenshot = Some(Screenshot { path: scene.out.clone(), at: 0.0, requested: false });
            }
        }
    }

    fn scene_step(&mut self, ctx: &egui::Context, name: &str, step: u32, waited: Duration) -> Go {
        let secs = waited.as_secs_f32();
        let cond = |c: bool| if c { Go::Next } else { Go::Wait };
        match (name, step) {
            ("home", _) => {
                if secs > 1.5 {
                    Go::Shoot
                } else {
                    Go::Wait
                }
            }
            (_, 0) if !self.loaded() => Go::Wait,

            ("grid", 0) => {
                self.set_grid(GRID.to_vec(), true);
                self.start_quietly(131);
                Go::Next
            }
            ("grid", 1) | ("picker", 1) | ("solo", 1) => {
                if self.all_live() && self.settled() && secs > 1.0 {
                    self.type_all(if name == "grid" { "uptime && ip -br a\n" } else { "uptime\n" });
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("grid", 2) => cond(self.settled()),
            ("grid", _) => cond(secs > 1.5).then_shoot(),

            ("broadcast", 0) => {
                if let Some(t) = self.tiles.iter_mut().find(|t| t.vmid() == 112) {
                    t.sync = false;
                }
                Go::Next
            }
            ("broadcast", 1) => {
                if self.all_live() && self.settled() && secs > 1.0 {
                    ctx.memory_mut(|m| m.request_focus(bar::capture_id()));
                    self.type_all("apt update\n");
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("broadcast", _) => {
                ctx.memory_mut(|m| m.request_focus(bar::capture_id())); // ON AIR
                cond(self.typing.is_none() && secs > 1.9).then_shoot()
            }

            ("placeholders", 0) => Go::Next,
            ("placeholders", 1) => {
                if self.all_live() && self.settled() && secs > 1.0 {
                    self.bar.text = "echo {{name}} {{ipv4}}".into();
                    self.lookup_ips_then_type(Target::Broadcast, "echo {{name}} {{ipv4}}\n".into());
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("placeholders", 2) => {
                let typed =
                    self.cluster().and_then(|c| c.screen_text(101)).is_some_and(|t| t.contains("web-01 192.0.2.11"));
                cond(typed && self.settled())
            }
            ("placeholders", _) => cond(secs > 1.2).then_shoot(),

            ("picker", 0) => {
                self.cfg.prefs.filter = "prod".into();
                self.cfg.prefs.only_running = true;
                Go::Next
            }
            ("picker", 2) => {
                if self.settled() {
                    self.picker.show();
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("picker", _) => cond(secs > 1.2).then_shoot(),

            ("solo", 0) => Go::Next,
            ("solo", 2) => {
                if self.settled() {
                    ctx.memory_mut(|m| m.request_focus(tile::screen_id(101)));
                    self.type_text(Target::Solo(101), |_| Some("ip -br a\n".into()));
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("solo", 3) => {
                ctx.memory_mut(|m| m.request_focus(tile::screen_id(101)));
                cond(self.settled() && secs > 0.5)
            }
            ("solo", _) => {
                ctx.memory_mut(|m| m.request_focus(tile::screen_id(101)));
                cond(secs > 1.0).then_shoot()
            }

            ("power", 0) => {
                self.set_grid(POWER_GRID.to_vec(), true);
                Go::Next
            }
            ("power", 1) => {
                if self.settled() && secs > 1.0 {
                    self.type_all("uptime\n");
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("power", 2) => {
                if self.settled() {
                    self.toasts.clear(); // only the power toasts
                    self.power(131, PowerAction::Start);
                    Go::Next
                } else {
                    Go::Wait
                }
            }
            ("power", _) => {
                if let Some(t) = self.tiles.iter_mut().find(|t| t.vmid() == 122) {
                    t.arm(PowerAction::Stop); // as after the first of the two clicks
                }
                cond(secs > 2.2).then_shoot()
            }

            ("maximized", 0) => {
                let mut ids = demo::DEFAULT_GRID.to_vec();
                ids.push(131);
                self.set_grid(ids, true);
                self.start_quietly(131);
                self.maxed = Some(131);
                Go::Next
            }
            ("maximized", 1) => {
                // near the end of the boot log, before the screen clears for the login banner
                let live = self.tiles.iter().any(|t| t.vmid() == 131 && t.is_live());
                let text = self.cluster().and_then(|c| c.screen_text(131)).unwrap_or_default();
                cond(live && text.contains("Started QEMU Guest Agent."))
            }
            ("maximized", _) => cond(secs > 0.15).then_shoot(),

            _ => Go::Shoot,
        }
    }
}

impl Go {
    /// A final step: shoot once its condition holds.
    fn then_shoot(self) -> Go {
        match self {
            Go::Next => Go::Shoot,
            other => other,
        }
    }
}
