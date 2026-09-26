//! Application state and the frame loop.

use std::collections::{BTreeMap, HashMap};
#[cfg(not(feature = "mas"))]
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{
    Align, Color32, Event, Frame, Id, Key, Layout, Modal, Rect, RichText, Sense, Stroke, Ui, ViewportCommand, pos2,
    vec2,
};
use pve::{PowerAction, TaskStatus, VmResource};
use rfb::ClientInput;
use uuid::Uuid;

use crate::backend::{self, Backend};
use crate::bar::{self, Bar};
use crate::clusters::{self, ClusterAction, ClustersUi};
use crate::config::{Bookmark, Config};
use crate::input::{EscHold, EscTick, KeyRouter, Routed};
use crate::keys;
use crate::picker::Picker;
use crate::secrets;
use crate::theme;
use crate::tile::{self, Tile, TileAction, TileView};
use crate::toast::Toasts;
use crate::typing::{self, Typing};
use crate::widgets::{self, PlateStyle};

#[cfg(feature = "store-shots")]
#[path = "scenes.rs"]
mod scenes;
#[cfg(feature = "store-shots")]
pub use scenes::NAMES as SCENES;

const POLL: Duration = Duration::from_secs(3);
const MANY_CONSOLES: usize = 24;
pub const AUTHOR: &str = "By The Integrators BV (NL), Wietse Wind";
pub const REPO_URL: &str = "https://github.com/WietseWind/VMherd";
pub const README_URL: &str = "https://github.com/WietseWind/VMherd#readme";
/// Website, privacy policy (App Review guideline 5.1.1(i): reachable from inside the app) and
/// support, shown in About and under the clusters.
pub const SITE_LINKS: &[(&str, &str)] = &[
    ("vmherd.app", "https://vmherd.app"),
    ("Privacy policy", "https://vmherd.app/privacy/"),
    ("Support", "https://vmherd.app/support/"),
];
/// Apple's standard license agreement, which covers the App Store version.
#[cfg(feature = "mas")]
const APPLE_EULA_URL: &str = "https://www.apple.com/legal/internet-services/itunes/dev/stdeula/";
/// Holding Esc this long leaves broadcast / solo mode (a shorter press is sent to the consoles).
const ESC_HOLD: Duration = Duration::from_secs(1);

/// Command line wishes.
#[derive(Default)]
pub struct Startup {
    pub cluster: Option<String>,
    pub vmids: Vec<u32>,
    /// `--screenshot FILE` after this many seconds (not in the Mac App Store build).
    #[cfg(not(feature = "mas"))]
    pub screenshot: Option<(PathBuf, f64)>,
    /// Start in the demo cluster; settings are neither loaded nor saved.
    pub demo: bool,
    /// `--scene NAME --out FILE` (store-shots builds only).
    #[cfg(feature = "store-shots")]
    pub scene: Option<(String, PathBuf)>,
}

enum ConnectError {
    Untrusted(pve::CertProblem),
    Other(String),
}

enum Msg {
    Connected {
        epoch: u64,
        id: Uuid,
        url: String,
        result: Result<(pve::Client, pve::Version), ConnectError>,
    },
    Vms {
        epoch: u64,
        result: Result<Vec<VmResource>, String>,
    },
    Power {
        epoch: u64,
        vmid: u32,
        node: String,
        label: String,
        action: PowerAction,
        result: Result<String, String>,
    },
    Task {
        epoch: u64,
        vmid: u32,
        label: String,
        action: PowerAction,
        result: Result<TaskStatus, String>,
    },
    /// Guest addresses looked up for typing `text` (which uses {{ipv4}} / {{ipv6}}).
    Ips {
        epoch: u64,
        target: Target,
        text: String,
        results: Vec<(u32, String, Result<pve::GuestIps, String>)>,
    },
}

struct Session {
    id: Uuid,
    name: String,
    backend: Backend,
    host: String,
    vms: BTreeMap<u32, VmResource>,
    loaded: bool,
    error: Option<String>,
    next_poll: Instant,
    polling: bool,
    /// power / task-watch tasks, aborted on disconnect
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Session {
    fn track(&mut self, handle: tokio::task::AbortHandle) {
        self.tasks.retain(|h| !h.is_finished());
        self.tasks.push(handle);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    None,
    Broadcast,
    Solo(u32),
}

enum Confirm {
    Paste { text: String, target: Target },
    OpenMany { ids: Vec<u32>, new: usize },
}

struct Trust {
    id: Uuid,
    url: String,
    problem: pve::CertProblem,
}

#[cfg(not(feature = "mas"))]
struct Screenshot {
    path: PathBuf,
    at: f64,
    requested: bool,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    ctx: egui::Context,
    cfg: Config,
    save_at: Option<Instant>,
    epoch: u64,
    session: Option<Session>,
    status: clusters::Status,
    clusters: ClustersUi,
    tiles: Vec<Tile>,
    picker: Picker,
    bar: Bar,
    router: KeyRouter,
    /// which tiles got each held key's press, so the release goes to exactly those
    sent: HashMap<(u32, u32), Vec<u32>>,
    target: Target,
    typing: Option<Typing>,
    /// short Esc = to the consoles, long Esc = leave broadcast / solo
    esc: EscHold,
    esc_progress: Option<f32>,
    show_about: bool,
    maxed: Option<u32>,
    toasts: Toasts,
    confirm: Option<Confirm>,
    trust: Option<Trust>,
    /// VMIDs from the command line, for this bookmark only
    startup_grid: Option<(Uuid, Vec<u32>)>,
    #[cfg(not(feature = "mas"))]
    screenshot: Option<Screenshot>,
    /// false after `--demo`: nothing is read from or written to the settings file
    persist: bool,
    /// The real settings while the demo (started from the clusters screen) runs with defaults.
    stash: Option<Config>,
    #[cfg(feature = "store-shots")]
    scene: Option<scenes::Scene>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup) -> anyhow::Result<Self> {
        theme::install(&cc.egui_ctx);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("vmherd-net")
            .enable_all()
            .build()?;
        let (tx, rx) = mpsc::channel();
        let (cfg, warning) = if startup.demo { (Config::default(), None) } else { Config::load() };
        let mut app = Self {
            rt,
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            cfg,
            save_at: None,
            epoch: 0,
            session: None,
            status: clusters::Status::default(),
            clusters: ClustersUi::default(),
            tiles: Vec::new(),
            picker: Picker::default(),
            bar: Bar::default(),
            router: KeyRouter::default(),
            sent: HashMap::new(),
            target: Target::None,
            typing: None,
            esc: EscHold::new(ESC_HOLD),
            esc_progress: None,
            show_about: false,
            maxed: None,
            toasts: Toasts::default(),
            confirm: None,
            trust: None,
            startup_grid: None,
            #[cfg(not(feature = "mas"))]
            screenshot: startup.screenshot.map(|(path, at)| Screenshot { path, at, requested: false }),
            persist: !startup.demo,
            stash: None,
            #[cfg(feature = "store-shots")]
            scene: startup.scene.map(|(name, out)| scenes::Scene::new(&name, out)),
        };
        if let Some(w) = warning {
            app.toasts.error(w);
        }
        if startup.demo {
            if app.scene_wants_demo() {
                app.connect_demo(startup.vmids);
            }
            return Ok(app);
        }
        let wanted = match &startup.cluster {
            Some(name) => {
                let found = app.cfg.bookmarks.iter().find(|b| b.name.eq_ignore_ascii_case(name)).map(|b| b.id);
                if found.is_none() {
                    app.toasts.error(format!("No cluster bookmark named \"{name}\""));
                }
                found
            }
            None if !startup.vmids.is_empty() => app.cfg.prefs.last_cluster,
            None => None,
        };
        if let Some(id) = wanted {
            if !startup.vmids.is_empty() {
                app.startup_grid = Some((id, startup.vmids.clone()));
            }
            app.connect(id);
        }
        Ok(app)
    }

    // ---------- persistence ----------

    fn save_soon(&mut self) {
        self.save_at = Some(Instant::now() + Duration::from_millis(400));
    }

    fn save_now(&mut self) {
        self.save_at = None;
        if !self.persist || self.in_demo() {
            return; // the demo never touches the settings file
        }
        if let Err(e) = self.cfg.save() {
            self.toasts.error(format!("Cannot save settings: {e:#}"));
        }
    }

    fn in_demo(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.backend.is_demo())
    }

    fn bookmark_mut(&mut self) -> Option<&mut Bookmark> {
        let id = self.session.as_ref()?.id;
        self.cfg.bookmark_mut(id)
    }

    // ---------- connection ----------

    fn connect(&mut self, id: Uuid) {
        let Some(b) = self.cfg.bookmark(id).cloned() else { return };
        self.disconnect();
        self.clusters.close_form();
        if self.startup_grid.as_ref().is_some_and(|(sid, _)| *sid != id) {
            self.startup_grid = None;
        }
        let url = b.url.clone();
        self.epoch += 1;
        let epoch = self.epoch;
        self.status = clusters::Status { connecting: Some(id), error: None };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = async {
                let bookmark = b.clone();
                let secret = tokio::task::spawn_blocking(move || secrets::get(&bookmark))
                    .await
                    .map_err(|e| ConnectError::Other(e.to_string()))?
                    .map_err(ConnectError::Other)?;
                let url = url::Url::parse(&b.url).map_err(|e| ConnectError::Other(format!("Invalid API URL: {e}")))?;
                let endpoint = pve::Endpoint {
                    url,
                    token_id: b.token_id.clone(),
                    token_secret: secret,
                    pinned_sha256: match b.pinned_sha256.as_deref() {
                        None => None,
                        // never fall back to the OS trust store because a stored pin is unreadable
                        Some(p) => Some(pve::parse_fingerprint(p).ok_or_else(|| {
                            ConnectError::Other(format!(
                                "The saved certificate pin \"{p}\" is invalid. Edit the cluster and forget it."
                            ))
                        })?),
                    },
                };
                let client = pve::Client::new(&endpoint).map_err(|e| ConnectError::Other(e.to_string()))?;
                match client.version().await {
                    Ok(v) => Ok((client, v)),
                    Err(pve::Error::UntrustedCert(p)) => Err(ConnectError::Untrusted(p)),
                    Err(e) => Err(ConnectError::Other(e.to_string())),
                }
            }
            .await;
            let _ = tx.send(Msg::Connected { epoch, id, url, result });
            ctx.request_repaint();
        });
    }

    fn disconnect(&mut self) {
        self.release_held();
        if let Some(s) = &self.session {
            for h in &s.tasks {
                h.abort();
            }
        }
        self.sent.clear();
        self.tiles.clear();
        self.typing = None;
        self.maxed = None;
        self.session = None;
        self.picker.open = false;
        self.confirm = None;
        if let Some(cfg) = self.stash.take() {
            self.cfg = cfg; // leaving the demo: back to the real settings
            self.save_at = None; // asked for in the demo; the real settings were saved before it
        }
        self.ctx.send_viewport_cmd(ViewportCommand::Title("VMherd".into()));
    }

    /// The built-in demo cluster: simulated VMs, no network, nothing saved. `grid`: the VMIDs to
    /// show (none = the production web, database and k8s VMs).
    fn connect_demo(&mut self, grid: Vec<u32>) {
        let backend = match Backend::demo() {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Cannot start the demo: {e}"));
                return;
            }
        };
        if self.save_at.is_some() {
            self.save_now();
        }
        self.disconnect();
        self.clusters.close_form();
        self.epoch += 1;
        self.status = clusters::Status::default();
        self.trust = None;
        if self.persist {
            // the demo runs with default settings; the real ones come back on leaving
            self.stash = Some(std::mem::take(&mut self.cfg));
        }
        let grid = if grid.is_empty() { demo::DEFAULT_GRID.to_vec() } else { grid };
        self.startup_grid = Some((Uuid::nil(), grid));
        self.ctx.send_viewport_cmd(ViewportCommand::Title(format!("VMherd — {}", backend::DEMO_NAME)));
        self.session = Some(Session {
            id: Uuid::nil(),
            name: backend::DEMO_NAME.into(),
            host: backend.host_label(),
            backend,
            vms: BTreeMap::new(),
            loaded: false,
            error: None,
            next_poll: Instant::now(),
            polling: false,
            tasks: Vec::new(),
        });
        self.toasts.info("13 simulated VMs — press Enter to type into all of them");
        self.poll_vms();
    }

    #[cfg(not(feature = "store-shots"))]
    fn scene_wants_demo(&self) -> bool {
        true
    }

    /// A connect for `id` that is still running no longer matters (bookmark edited or deleted).
    fn cancel_connect(&mut self, id: Uuid) {
        if self.status.connecting == Some(id) {
            self.epoch += 1;
            self.status.connecting = None;
        }
        if self.trust.as_ref().is_some_and(|t| t.id == id) {
            self.trust = None;
        }
        if self.status.error.as_ref().is_some_and(|(e, _)| *e == id) {
            self.status.error = None;
        }
    }

    /// Add or update a bookmark from the form: only the fields the form edits change; the grid,
    /// sync list and pin stay (the pin is dropped when asked or when the URL changes).
    fn save_bookmark(&mut self, edited: Bookmark, secret: Option<String>, forget_pin: bool) -> Result<(), String> {
        let id = edited.id;
        if !self.persist {
            return Err(
                "VMherd was started with --demo, so nothing is saved. Start it without --demo to add clusters.".into(),
            );
        }
        if let Some(secret) = &secret {
            secrets::set(id, &edited.secret, secret)?;
        }
        match self.cfg.bookmark_mut(id) {
            Some(b) => {
                // the secret moved elsewhere: forget what VMherd saved in the old place
                let old = std::mem::replace(&mut b.secret, edited.secret.clone());
                if std::mem::discriminant(&old) != std::mem::discriminant(&edited.secret) {
                    secrets::delete(id, &old)?;
                }
                if forget_pin || b.url != edited.url {
                    b.pinned_sha256 = None;
                }
                b.name = edited.name;
                b.url = edited.url;
                b.token_id = edited.token_id;
            }
            None => self.cfg.bookmarks.push(edited),
        }
        self.cancel_connect(id);
        self.save_now();
        Ok(())
    }

    fn poll_vms(&mut self) {
        let Some(s) = &mut self.session else { return };
        if s.polling || Instant::now() < s.next_poll {
            return;
        }
        s.polling = true;
        let (backend, tx, ctx, epoch) = (s.backend.clone(), self.tx.clone(), self.ctx.clone(), self.epoch);
        self.rt.spawn(async move {
            let result = backend.vms().await;
            let _ = tx.send(Msg::Vms { epoch, result });
            ctx.request_repaint();
        });
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Connected { epoch, id, url, result } if epoch == self.epoch => {
                self.status.connecting = None;
                match result {
                    Ok((client, version)) => {
                        let Some(b) = self.cfg.bookmark(id) else { return };
                        let backend = Backend::Pve(client);
                        let host = backend.host_label();
                        tracing::info!("connected to {} (Proxmox {})", b.name, version.version);
                        self.ctx.send_viewport_cmd(ViewportCommand::Title(format!("VMherd — {}", b.name)));
                        self.session = Some(Session {
                            id,
                            name: b.name.clone(),
                            backend,
                            host,
                            vms: BTreeMap::new(),
                            loaded: false,
                            error: None,
                            next_poll: Instant::now(),
                            polling: false,
                            tasks: Vec::new(),
                        });
                        self.cfg.prefs.last_cluster = Some(id);
                        self.save_soon();
                        self.poll_vms();
                    }
                    Err(ConnectError::Untrusted(problem)) => self.trust = Some(Trust { id, url, problem }),
                    Err(ConnectError::Other(e)) => self.status.error = Some((id, widgets::sentence(&e))),
                }
            }
            Msg::Vms { epoch, result } if epoch == self.epoch => {
                let Some(s) = &mut self.session else { return };
                s.polling = false;
                s.next_poll = Instant::now() + POLL;
                match result {
                    Ok(list) => {
                        s.vms = list.into_iter().map(|v| (v.vmid, v)).collect();
                        s.error = None;
                        let first = !s.loaded;
                        s.loaded = true;
                        for t in &mut self.tiles {
                            let vm = s
                                .vms
                                .get(&t.vmid())
                                .cloned()
                                .unwrap_or_else(|| VmResource { status: "missing".into(), ..t.vm.clone() });
                            t.set_vm(vm);
                        }
                        if first {
                            let session_id = s.id;
                            let grid = match self.startup_grid.take() {
                                Some((id, ids)) if id == session_id => ids,
                                _ => self.bookmark_mut().map(|b| b.grid.clone()).unwrap_or_default(),
                            };
                            self.set_grid(grid, true);
                            if self.tiles.is_empty() {
                                self.picker.show();
                            }
                        }
                    }
                    Err(e) => s.error = Some(e),
                }
            }
            Msg::Power { epoch, vmid, node, label, action, result } if epoch == self.epoch => match result {
                Ok(upid) => {
                    self.toasts.info(format!("{label}: {} sent", power_label(action)));
                    self.watch_task(vmid, node, label, action, upid);
                }
                Err(e) => self.toasts.error(format!("{label}: {} failed: {e}", power_label(action))),
            },
            Msg::Task { epoch, vmid, label, action, result } if epoch == self.epoch => {
                match result {
                    Ok(t) if t.is_ok() => self.toasts.info(format!("{label}: {} OK", power_label(action))),
                    Ok(t) => self.toasts.error(format!(
                        "{label}: {} failed: {}",
                        power_label(action),
                        t.exitstatus.unwrap_or_default()
                    )),
                    Err(e) => self.toasts.error(format!("{label}: {} status unknown: {e}", power_label(action))),
                }
                if let Some(t) = self.tiles.iter_mut().find(|t| t.vmid() == vmid) {
                    t.retry_now();
                }
                if let Some(s) = &mut self.session {
                    s.next_poll = Instant::now();
                }
            }
            Msg::Ips { epoch, target, text, results } if epoch == self.epoch => {
                let (need4, need6) = (text.contains("{{ipv4}}"), text.contains("{{ipv6}}"));
                let missing: Vec<String> = results
                    .iter()
                    .filter_map(|(_, label, r)| match r {
                        Err(e) => Some(format!("{label} ({e})")),
                        Ok(ips) if need4 && ips.ipv4.is_none() => Some(format!("{label} (no IPv4)")),
                        Ok(ips) if need6 && ips.ipv6.is_none() => Some(format!("{label} (no IPv6)")),
                        Ok(_) => None,
                    })
                    .collect();
                if !missing.is_empty() {
                    let more =
                        if missing.len() > 3 { format!(" and {} more", missing.len() - 3) } else { String::new() };
                    self.toasts.error(format!(
                        "Nothing typed: no address for {}{more}",
                        missing.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                    ));
                    return;
                }
                let ips: HashMap<u32, pve::GuestIps> =
                    results.into_iter().filter_map(|(id, _, r)| Some((id, r.ok()?))).collect();
                self.type_text(target, |vm| {
                    let ips = ips.get(&vm.vmid)?; // a console that joined after the lookup is skipped
                    Some(typing::fill_template(&text, vm.vmid, vm.name.as_deref().unwrap_or(""), &vm.node, Some(ips)))
                });
            }
            _ => {} // a reply for an older connection
        }
    }

    /// Replace the grid (order = `ids`). `force` skips the "many consoles" question.
    fn set_grid(&mut self, ids: Vec<u32>, force: bool) {
        let Some(s) = &self.session else { return };
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<u32> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        let new = ids.iter().filter(|id| !self.tiles.iter().any(|t| t.vmid() == **id)).count();
        if !force && new > MANY_CONSOLES {
            self.confirm = Some(Confirm::OpenMany { ids, new });
            return;
        }
        let vms = s.vms.clone();
        let nosync = self.bookmark_mut().map(|b| b.nosync.clone()).unwrap_or_default();
        let mut old: Vec<Tile> = std::mem::take(&mut self.tiles);
        for id in &ids {
            if let Some(pos) = old.iter().position(|t| t.vmid() == *id) {
                self.tiles.push(old.remove(pos));
            } else if let Some(vm) = vms.get(id) {
                self.tiles.push(Tile::new(vm.clone(), !nosync.contains(id)));
            }
        }
        drop(old); // closes the consoles of removed tiles
        if self.maxed.is_some_and(|m| !ids.contains(&m)) {
            self.maxed = None;
        }
        let kept: Vec<u32> = self.tiles.iter().map(Tile::vmid).collect();
        if let Some(b) = self.bookmark_mut() {
            b.grid = kept;
        }
        self.save_soon();
    }

    fn power(&mut self, vmid: u32, action: PowerAction) {
        let (Some(s), Some(t)) = (&self.session, self.tiles.iter().find(|t| t.vmid() == vmid)) else { return };
        let label = format!("{} {}", vmid, t.vm.name.as_deref().unwrap_or(""));
        let (backend, vm, tx, ctx, epoch) =
            (s.backend.clone(), t.vm.vm_ref(), self.tx.clone(), self.ctx.clone(), self.epoch);
        tracing::info!("power {} {label} on {}", action.as_str(), vm.node);
        let handle = self.rt.spawn(async move {
            let result = backend.power(&vm, action).await;
            let node = vm.node;
            let _ = tx.send(Msg::Power { epoch, vmid, node, label, action, result });
            ctx.request_repaint();
        });
        if let Some(s) = &mut self.session {
            s.track(handle.abort_handle());
        }
    }

    fn watch_task(&mut self, vmid: u32, node: String, label: String, action: PowerAction, upid: String) {
        let Some(s) = &self.session else { return };
        let (backend, tx, ctx, epoch) = (s.backend.clone(), self.tx.clone(), self.ctx.clone(), self.epoch);
        let handle = self.rt.spawn(async move {
            let mut last_err = String::from("timed out");
            for _ in 0..300 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match backend.task_status(&node, &upid).await {
                    Ok(t) if t.is_done() => {
                        let _ = tx.send(Msg::Task { epoch, vmid, label, action, result: Ok(t) });
                        ctx.request_repaint();
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => last_err = e,
                }
            }
            let _ = tx.send(Msg::Task { epoch, vmid, label, action, result: Err(last_err) });
            ctx.request_repaint();
        });
        if let Some(s) = &mut self.session {
            s.track(handle.abort_handle());
        }
    }

    // ---------- keyboard ----------

    fn target_tiles(&self, target: Target) -> impl Iterator<Item = &Tile> {
        self.tiles.iter().filter(move |t| {
            t.is_live()
                && match target {
                    Target::Broadcast => t.sync,
                    Target::Solo(v) => t.vmid() == v,
                    Target::None => false,
                }
        })
    }

    fn send_key(&mut self, keysym: u32, qnum: u32, down: bool) {
        let input = ClientInput::Key { keysym, qnum, down };
        if down {
            let ids: Vec<u32> = self.target_tiles(self.target).map(Tile::vmid).collect();
            for t in self.tiles.iter().filter(|t| ids.contains(&t.vmid())) {
                t.send(input);
            }
            let got = self.sent.entry((keysym, qnum)).or_default();
            got.extend(ids.into_iter().filter(|id| !got.contains(id)).collect::<Vec<_>>());
        } else {
            let ids = match self.sent.remove(&(keysym, qnum)) {
                Some(ids) => ids,
                None => self.target_tiles(self.target).map(Tile::vmid).collect(),
            };
            for t in self.tiles.iter().filter(|t| ids.contains(&t.vmid())) {
                t.send(input);
            }
        }
    }

    fn release_held(&mut self) {
        let mut out = Vec::new();
        self.router.release_all(&mut out);
        self.dispatch(out);
    }

    fn dispatch(&mut self, routed: Vec<Routed>) {
        for r in routed {
            match r {
                Routed::Key { keysym, qnum, down } => self.send_key(keysym, qnum, down),
                Routed::Paste(text) => {
                    let target = self.target;
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    if text.contains('\n') {
                        self.confirm = Some(Confirm::Paste { text, target });
                    } else {
                        self.type_text(target, |_| Some(text.clone()));
                    }
                }
            }
        }
    }

    fn route_keys(&mut self, ctx: &egui::Context) {
        let focused = ctx.memory(|m| m.focused());
        let target = if focused == Some(bar::capture_id()) {
            Target::Broadcast
        } else if let Some(t) = self.tiles.iter().find(|t| Some(tile::screen_id(t.vmid())) == focused) {
            Target::Solo(t.vmid())
        } else {
            Target::None
        };
        if target != self.target {
            self.release_held();
            self.target = target;
            self.esc.reset();
            self.esc_progress = None;
        }
        if target == Target::None {
            return;
        }
        let mut out = Vec::new();
        let now = Instant::now();
        ctx.input(|i| {
            for event in &i.events {
                if !self.esc.on_event(event, now, &mut out) {
                    self.router.route(event, cfg!(target_os = "macos"), &mut out);
                }
            }
        });
        self.dispatch(out);
        self.esc_progress = match self.esc.tick(now) {
            EscTick::Idle => None,
            EscTick::Holding(p) => {
                ctx.request_repaint_after(Duration::from_millis(30));
                Some(p)
            }
            EscTick::Leave => {
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
                None
            }
        };
    }

    fn type_text(&mut self, target: Target, text_for: impl Fn(&VmResource) -> Option<String>) {
        if self.typing.is_some() {
            self.toasts.error("Still typing. Press Stop first.");
            return;
        }
        let mut skipped = 0;
        let jobs: Vec<_> = self
            .target_tiles(target)
            .filter_map(|t| {
                let (strokes, s) = keys::text_strokes(&text_for(&t.vm)?);
                skipped = skipped.max(s);
                t.sender().map(|tx| (tx, strokes))
            })
            .collect();
        if jobs.is_empty() {
            self.toasts.error("No synced console is connected.");
            return;
        }
        let delay = Duration::from_millis(self.cfg.prefs.delay_ms.clamp(1, 1000));
        self.typing = Some(Typing::start(self.rt.handle(), jobs, delay, self.ctx.clone()));
        if skipped > 0 {
            self.toasts.error(format!(
                "{skipped} character{} not on a US keyboard were skipped",
                if skipped == 1 { "" } else { "s" }
            ));
        }
    }

    /// {{ipv4}} / {{ipv6}}: ask the guest agents (all targets at once), then type.
    fn lookup_ips_then_type(&mut self, target: Target, text: String) {
        let Some(s) = &self.session else { return };
        let vms: Vec<(u32, String, pve::VmRef)> = self
            .target_tiles(target)
            .map(|t| (t.vmid(), format!("{} {}", t.vmid(), t.vm.name.as_deref().unwrap_or("")), t.vm.vm_ref()))
            .collect();
        if vms.is_empty() {
            self.toasts.error("No synced console is connected.");
            return;
        }
        self.toasts.info(format!(
            "Looking up the addresses of {} VM{}…",
            vms.len(),
            if vms.len() == 1 { "" } else { "s" }
        ));
        let (backend, tx, ctx, epoch) = (s.backend.clone(), self.tx.clone(), self.ctx.clone(), self.epoch);
        let handle = self.rt.spawn(async move {
            let lookups = vms.into_iter().map(|(vmid, label, vm)| {
                let backend = backend.clone();
                async move { (vmid, label, backend.guest_ips(&vm).await.map_err(|e| widgets::sentence(&e))) }
            });
            let results = futures_join_all(lookups).await;
            let _ = tx.send(Msg::Ips { epoch, target, text, results });
            ctx.request_repaint();
        });
        if let Some(s) = &mut self.session {
            s.track(handle.abort_handle());
        }
    }

    fn send_combo(&self, combo: bar::Combo) {
        for t in self.target_tiles(Target::Broadcast) {
            for &(qnum, keysym) in combo {
                t.send(ClientInput::Key { keysym, qnum, down: true });
            }
            for &(qnum, keysym) in combo.iter().rev() {
                t.send(ClientInput::Key { keysym, qnum, down: false });
            }
        }
    }

    fn set_sync(&mut self, vmid: u32, on: bool) {
        if let Some(b) = self.bookmark_mut() {
            b.nosync.retain(|v| *v != vmid);
            if !on {
                b.nosync.push(vmid);
            }
        }
        self.save_soon();
    }

    // ---------- drawing ----------

    fn top_bar(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        widgets::vgradient(
            ui.painter(),
            rect,
            Color32::from_rgb(0x16, 0x1a, 0x17),
            Color32::from_rgb(0x10, 0x13, 0x11),
        );
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::LINE));
        let mut ui = ui.new_child(
            egui::UiBuilder::new().max_rect(rect.shrink2(vec2(12.0, 0.0))).layout(Layout::left_to_right(Align::Center)),
        );
        ui.spacing_mut().item_spacing.x = 10.0;
        let connected = self.session.is_some();
        if connected {
            let mut job = widgets::spaced("VMS ", theme::label(11.0), theme::TEXT, 1.6);
            job.append(
                &self.tiles.len().to_string(),
                0.0,
                egui::TextFormat { font_id: theme::mono_bold(12.0), color: theme::GREEN, ..Default::default() },
            );
            if widgets::plate_button(&mut ui, job, vec2(0.0, 28.0), PlateStyle::DEFAULT, true)
                .on_hover_text("Choose VMs")
                .clicked()
            {
                self.picker.toggle();
            }
        }
        let mut brand = widgets::spaced("VM", theme::bold(12.0), theme::MUTED, 3.8);
        brand.append(
            "HERD",
            0.0,
            egui::TextFormat {
                font_id: theme::bold(12.0),
                color: theme::GREEN,
                extra_letter_spacing: 3.8,
                ..Default::default()
            },
        );
        ui.label(brand);

        if let Some((name, demo)) = self.session.as_ref().map(|s| (s.name.clone(), s.backend.is_demo())) {
            let others: Vec<(Uuid, String)> = self
                .cfg
                .bookmarks
                .iter()
                .filter(|b| !demo && Some(b.id) != self.session.as_ref().map(|s| s.id))
                .map(|b| (b.id, b.name.clone()))
                .collect();
            let mut switch = None;
            let mut disconnect = false;
            ui.menu_button(RichText::new(format!("{name} ▾")).font(theme::bold(13.0)).color(theme::STEEL), |ui| {
                for (id, n) in &others {
                    if ui.button(format!("Connect to {n}")).clicked() {
                        switch = Some(*id);
                    }
                }
                if !others.is_empty() {
                    ui.separator();
                }
                if demo {
                    disconnect = ui.button("Leave demo").clicked();
                } else if ui.button("Manage clusters…").clicked() || ui.button("Disconnect").clicked() {
                    disconnect = true;
                }
            });
            if demo {
                demo_badge(&mut ui);
            }
            if let Some(id) = switch {
                self.connect(id);
            } else if disconnect {
                self.disconnect();
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let about = widgets::plate_button(
                ui,
                widgets::spaced("ABOUT", theme::label(11.0), theme::TEXT, 1.6),
                vec2(0.0, 28.0),
                PlateStyle::DEFAULT,
                true,
            );
            if about.on_hover_text("About VMherd and its keyboard shortcuts").clicked() {
                self.show_about = true;
            }
            if self.session.is_none() {
                return;
            }
            // ON AIR lamp
            let n = self.target_tiles(Target::Broadcast).count();
            let on = self.target == Target::Broadcast;
            let mut job = widgets::spaced(
                "ON AIR ",
                theme::bold(11.0),
                if on { Color32::WHITE } else { Color32::from_rgb(0x5a, 0x2a, 0x2a) },
                2.4,
            );
            job.append(
                &n.to_string(),
                0.0,
                egui::TextFormat {
                    font_id: theme::mono_bold(11.0),
                    color: if on { Color32::WHITE } else { Color32::from_rgb(0x5a, 0x2a, 0x2a) },
                    ..Default::default()
                },
            );
            let g = ui.painter().layout_job(job);
            let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 34.0, 28.0), Sense::hover());
            let p = ui.painter();
            if on {
                widgets::glow(p, r, 4, Color32::from_rgba_unmultiplied(255, 61, 61, 115), 18);
                p.rect_filled(r, 4.0, Color32::from_rgb(0x3a, 0x0d, 0x0d));
                p.rect_stroke(r, 4.0, Stroke::new(1.0, theme::RED), egui::StrokeKind::Inside);
                let pulse =
                    0.35 + 0.65 * (0.5 + 0.5 * (ui.input(|i| i.time) * std::f64::consts::TAU / 1.1).cos() as f32);
                let dot = pos2(r.left() + 14.0, r.center().y);
                p.circle_filled(dot, 7.0, theme::RED.gamma_multiply(0.3 * pulse));
                p.circle_filled(dot, 4.0, theme::RED.gamma_multiply(pulse));
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            } else {
                p.rect_filled(r, 4.0, Color32::from_rgb(0x14, 0x0c, 0x0c));
                p.rect_stroke(r, 4.0, Stroke::new(1.0, Color32::from_rgb(0x3a, 0x1c, 0x1c)), egui::StrokeKind::Inside);
                p.circle_filled(pos2(r.left() + 14.0, r.center().y), 4.0, Color32::from_rgb(0x3a, 0x1c, 0x1c));
            }
            p.galley(pos2(r.left() + 25.0, r.center().y - g.size().y / 2.0), g, Color32::WHITE);
            resp.on_hover_text("Keys typed in the broadcast bar go to every synced console");

            if let Some(s) = &self.session {
                let (text, color) = match &s.error {
                    Some(e) => (e.clone(), theme::RED),
                    None if s.loaded => {
                        (format!("{} · {} VMs", s.host, s.vms.values().filter(|v| !v.template).count()), theme::MUTED)
                    }
                    None => (format!("{} · loading…", s.host), theme::MUTED),
                };
                let mut job = widgets::plain(&text, theme::mono(11.0), color);
                job.wrap = egui::text::TextWrapping::truncate_at_width(ui.available_width().min(420.0) - 120.0);
                ui.label(job);
            }

            let cols = self.cfg.prefs.cols;
            egui::ComboBox::from_id_salt("cols")
                .width(64.0)
                .selected_text(
                    RichText::new(if cols == 0 { "auto".into() } else { cols.to_string() })
                        .font(theme::mono(12.0))
                        .color(theme::GREEN),
                )
                .show_ui(ui, |ui| {
                    for c in [0, 1, 2, 3, 4, 5, 6, 8] {
                        let label = if c == 0 { "auto".to_owned() } else { c.to_string() };
                        ui.selectable_value(&mut self.cfg.prefs.cols, c, label);
                    }
                })
                .response
                .on_hover_text("Tiles per row");
            ui.label(widgets::spaced("COLS", theme::label(11.0), theme::MUTED, 1.6));
            if cols != self.cfg.prefs.cols {
                self.save_soon();
            }
        });
    }

    fn grid(&mut self, ui: &mut Ui) {
        let area = ui.max_rect();
        ui.painter().rect_filled(area, 0.0, theme::BG);
        widgets::grid_background(ui.painter(), area);
        let loaded = self.session.as_ref().is_some_and(|s| s.loaded);
        if self.tiles.is_empty() {
            if loaded {
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(area)
                        .layout(Layout::centered_and_justified(egui::Direction::TopDown)),
                );
                child.vertical_centered(|ui| {
                    let logo = (area.height() * 0.36).clamp(120.0, 260.0);
                    ui.add_space(((area.height() - logo - 150.0) / 2.0).max(10.0));
                    widgets::logo(ui, logo);
                    ui.add_space(4.0);
                    ui.label(widgets::wordmark(30.0));
                    ui.add_space(14.0);
                    ui.label(widgets::spaced("NO VMS IN THE GRID", theme::label(15.0), theme::MUTED, 2.0));
                    ui.add_space(8.0);
                    if widgets::plate_button(
                        ui,
                        widgets::spaced("CHOOSE VMS", theme::label(11.0), theme::TEXT, 1.6),
                        vec2(0.0, 28.0),
                        PlateStyle::DEFAULT,
                        true,
                    )
                    .clicked()
                    {
                        self.picker.show();
                    }
                });
            }
            return;
        }
        let bcast = self.target == Target::Broadcast;
        let esc = self.esc_progress.filter(|_| matches!(self.target, Target::Solo(_)));
        let mut actions: Vec<(u32, TileAction)> = Vec::new();

        if let Some(m) = self.maxed
            && let Some(t) = self.tiles.iter_mut().find(|t| t.vmid() == m)
        {
            if let Some(a) = t.ui(ui, area.shrink(10.0), &TileView { bcast, maxed: true, esc }) {
                actions.push((m, a));
            }
        } else {
            let (w, h) = (area.width() - 2.0 * theme::GAP, area.height() - 2.0 * theme::GAP);
            let n = self.tiles.len();
            let (cols, tile_w) = layout(n, w, h, self.cfg.prefs.cols);
            let tile_h = theme::TILE_HEAD + tile_w / theme::SCREEN_ASPECT;
            let rows = n.div_ceil(cols);
            let total_w = cols as f32 * tile_w + (cols - 1) as f32 * theme::GAP;
            let total_h = rows as f32 * tile_h + (rows - 1) as f32 * theme::GAP + 2.0 * theme::GAP;
            let content_w = area.width().max(total_w + 2.0 * theme::GAP);
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                let (content, _) = ui.allocate_exact_size(vec2(content_w, total_h.max(area.height())), Sense::hover());
                let x0 = content.left() + ((content.width() - total_w) / 2.0).max(theme::GAP);
                for (i, t) in self.tiles.iter_mut().enumerate() {
                    let (r, c) = (i / cols, i % cols);
                    let min = pos2(
                        x0 + c as f32 * (tile_w + theme::GAP),
                        content.top() + theme::GAP + r as f32 * (tile_h + theme::GAP),
                    );
                    let rect = Rect::from_min_size(min, vec2(tile_w, tile_h));
                    if ui.is_rect_visible(rect)
                        && let Some(a) = t.ui(ui, rect, &TileView { bcast, maxed: false, esc })
                    {
                        actions.push((t.vmid(), a));
                    }
                }
            });
        }

        for (vmid, action) in actions {
            match action {
                TileAction::Power(p) => self.power(vmid, p),
                TileAction::Reconnect => {
                    if let Some(t) = self.tiles.iter_mut().find(|t| t.vmid() == vmid) {
                        t.reconnect();
                    }
                }
                TileAction::ToggleMax => self.maxed = if self.maxed == Some(vmid) { None } else { Some(vmid) },
                TileAction::Remove => {
                    let ids = self.tiles.iter().map(Tile::vmid).filter(|v| *v != vmid).collect();
                    self.set_grid(ids, true);
                }
                TileAction::SyncChanged => {
                    let on = self.tiles.iter().find(|t| t.vmid() == vmid).is_some_and(|t| t.sync);
                    self.set_sync(vmid, on);
                }
            }
        }
    }

    /// Keys for the app itself, only while no console, text field or dialog has the keyboard:
    /// Esc closes the VM list / restores a maximized tile, `/` shows or hides the VM list,
    /// Enter starts typing into all synced consoles.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.target != Target::None {
            return; // a console has the keyboard (⌘ keys are never sent to it, but they don't act here either)
        }
        // macOS: ⌘M minimizes to the Dock (the default app menu has ⌘H and ⌘Q, but no Window menu)
        if cfg!(target_os = "macos") {
            let cmd = |key: Key| {
                ctx.input(|i| {
                    i.events.iter().any(|e| {
                        matches!(e, Event::Key { key: k, pressed: true, repeat: false, modifiers, .. }
                            if *k == key && modifiers.mac_cmd && !modifiers.shift && !modifiers.alt && !modifiers.ctrl)
                    })
                })
            };
            if cmd(Key::M) {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
            }
        }
        if self.confirm.is_some() || self.trust.is_some() || self.show_about {
            return;
        }
        let pressed = |key: Key| {
            ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key))
            })
        };
        if pressed(Key::Escape) {
            if self.picker.open {
                self.picker.open = false;
            } else {
                self.maxed = None;
            }
        }
        let nothing_focused = ctx.memory(|m| m.focused().is_none());
        if !nothing_focused || self.session.is_none() {
            return;
        }
        if pressed(Key::Slash) {
            self.picker.toggle();
        } else if pressed(Key::Enter) && !self.picker.open {
            ctx.memory_mut(|m| m.request_focus(bar::capture_id()));
        }
    }

    fn about(&mut self, ctx: &egui::Context) {
        let mut close = false;
        Modal::new(Id::new("about")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.vertical_centered(|ui| {
                widgets::logo(ui, 120.0);
                ui.label(widgets::wordmark(26.0));
                ui.label(
                    RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                        .font(theme::mono(12.0))
                        .color(theme::MUTED),
                );
                ui.label(RichText::new("Many Proxmox VM consoles. One keyboard.").color(theme::MUTED));
                ui.add_space(4.0);
                ui.label(RichText::new(AUTHOR).font(theme::bold(15.0)).color(theme::TEXT));
                ui.add_space(2.0);
                widgets::link_row(
                    ui,
                    &[("github.com/WietseWind/VMherd", REPO_URL), ("README", README_URL)],
                    12.0,
                    true,
                );
                widgets::link_row(ui, SITE_LINKS, 12.0, true);
            });
            ui.add_space(10.0);
            ui.label(widgets::spaced("KEYBOARD", theme::bold(11.0), theme::MUTED, 2.0));
            egui::Grid::new("about-keys").num_columns(2).spacing(vec2(14.0, 5.0)).show(ui, |ui| {
                let paste = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };
                for (k, what) in [
                    ("Enter", "Type into all synced consoles (when nothing else has the keyboard)"),
                    ("Click a console", "Type into that console only (SOLO)"),
                    ("Hold Esc 1 s", "Stop typing into consoles; a short Esc goes to the consoles"),
                    ("/", "Show or hide the VM list"),
                    (
                        if cfg!(target_os = "macos") { "⌘M" } else { "" },
                        if cfg!(target_os = "macos") {
                            "Minimize to the Dock (when no console has the keyboard)"
                        } else {
                            ""
                        },
                    ),
                    ("Tab ↑ ↓ Space", "In the VM list: move, add or remove a VM; Esc closes it"),
                    (paste, "Type the clipboard into the consoles"),
                ] {
                    if k.is_empty() {
                        continue;
                    }
                    ui.label(RichText::new(k).font(theme::mono(12.0)).color(theme::GREEN));
                    ui.label(RichText::new(what).color(theme::TEXT));
                    ui.end_row();
                }
            });
            ui.add_space(10.0);
            ui.label(widgets::spaced("LICENSE", theme::bold(11.0), theme::MUTED, 2.0));
            license_text(ui);
            ui.add_space(10.0);
            ui.label(widgets::spaced("CREDITS", theme::bold(11.0), theme::MUTED, 2.0));
            // macOS uses the system TLS, the other platforms rustls
            let crates = if cfg!(target_os = "macos") { "egui, tokio" } else { "egui, tokio, rustls" };
            ui.label(
                RichText::new(format!(
                    "Fonts: Barlow and JetBrains Mono (SIL Open Font License 1.1); egui's bundled fonts. \
                     Built with {crates} and other open-source Rust crates: their licenses are in \
                     THIRD-PARTY-LICENSES.md, shipped with the app. Proxmox is a registered trademark of \
                     Proxmox Server Solutions GmbH. VMherd is not affiliated with or endorsed by Proxmox Server \
                     Solutions GmbH.",
                ))
                .color(theme::MUTED)
                .small(),
            );
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                close = widgets::plate_button(
                    ui,
                    widgets::spaced("CLOSE", theme::bold(12.0), theme::TEXT, 1.4),
                    vec2(90.0, 32.0),
                    PlateStyle::DEFAULT,
                    true,
                )
                .clicked();
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if close {
            self.show_about = false;
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.show_about {
            self.about(ctx);
        }
        if let Some(trust) = &self.trust {
            let fp = pve::format_fingerprint(&trust.problem.sha256);
            let (mut accept, mut cancel) = (false, false);
            Modal::new(Id::new("trust")).show(ctx, |ui| {
                ui.set_max_width(560.0);
                let (title, color) = if trust.problem.changed {
                    ("CERTIFICATE CHANGED", theme::RED)
                } else {
                    ("UNTRUSTED CERTIFICATE", theme::AMBER)
                };
                ui.label(widgets::spaced(title, theme::bold(15.0), color, 2.0));
                ui.add_space(6.0);
                let text = if trust.problem.changed {
                    format!("{} now presents a different certificate than the one you trusted. This is expected after a certificate renewal; otherwise someone may be intercepting the connection.", trust.url)
                } else {
                    format!("{} uses a certificate this computer does not trust (normal for Proxmox's self-signed certificates).", trust.url)
                };
                ui.label(text);
                ui.add_space(6.0);
                ui.label(RichText::new("SHA-256 fingerprint").color(theme::MUTED));
                ui.label(RichText::new(&fp).font(theme::mono(12.0)).color(theme::TEXT));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Compare it with the node's certificate (Proxmox UI: node → System → Certificates, or on the node: openssl x509 -in /etc/pve/local/pve-ssl.pem -noout -fingerprint -sha256).")
                        .color(theme::MUTED)
                        .small(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = if trust.problem.changed { "REPLACE PIN & CONNECT" } else { "TRUST & CONNECT" };
                    accept = widgets::plate_button(ui, widgets::spaced(label, theme::bold(12.0), theme::INK, 1.4), vec2(0.0, 32.0), PlateStyle::GO, true).clicked();
                    cancel = widgets::plate_button(ui, widgets::spaced("CANCEL", theme::bold(12.0), theme::TEXT, 1.4), vec2(0.0, 32.0), PlateStyle::DEFAULT, true).clicked();
                });
            });
            if accept {
                let id = trust.id;
                if let Some(b) = self.cfg.bookmark_mut(id) {
                    b.pinned_sha256 = Some(fp);
                }
                self.trust = None;
                self.save_now();
                self.connect(id);
            } else if cancel {
                self.trust = None;
            }
        }

        let Some(confirm) = &self.confirm else { return };
        let (question, detail) = match confirm {
            Confirm::Paste { text, target } => {
                let n = self.target_tiles(*target).count();
                let lines = text.trim_end_matches('\n').split('\n').count();
                let enter = if text.ends_with('\n') { ", ending with Enter" } else { "" };
                (
                    format!(
                        "Type {lines} line{} ({} characters{enter}) into {n} console{}?",
                        if lines == 1 { "" } else { "s" },
                        text.chars().count(),
                        if n == 1 { "" } else { "s" }
                    ),
                    text.lines().take(6).collect::<Vec<_>>().join("\n"),
                )
            }
            Confirm::OpenMany { new, .. } => (format!("Open {new} more consoles?"), String::new()),
        };
        let (mut yes, mut no) = (false, false);
        Modal::new(Id::new("confirm")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            ui.label(RichText::new(question).font(theme::bold(16.0)).color(theme::TEXT));
            if !detail.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new(detail).font(theme::mono(11.0)).color(theme::MUTED));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                yes = widgets::plate_button(
                    ui,
                    widgets::spaced("YES", theme::bold(12.0), theme::INK, 1.4),
                    vec2(70.0, 32.0),
                    PlateStyle::GO,
                    true,
                )
                .clicked();
                no = widgets::plate_button(
                    ui,
                    widgets::spaced("CANCEL", theme::bold(12.0), theme::TEXT, 1.4),
                    vec2(0.0, 32.0),
                    PlateStyle::DEFAULT,
                    true,
                )
                .clicked();
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                no = true;
            }
        });
        if yes {
            match self.confirm.take() {
                Some(Confirm::Paste { text, target }) => self.type_text(target, |_| Some(text.clone())),
                Some(Confirm::OpenMany { ids, .. }) => self.set_grid(ids, true),
                None => {}
            }
        } else if no {
            self.confirm = None;
        }
    }

    #[cfg(not(feature = "mas"))]
    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.screenshot else { return };
        let now = ctx.input(|i| i.time);
        if !shot.requested && now >= shot.at {
            shot.requested = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
        }
        if !shot.requested {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        let image = ctx.input(|i| {
            i.events
                .iter()
                .find_map(|e| if let Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None })
        });
        if let Some(image) = image {
            let path = shot.path.clone();
            let [w, h] = image.size;
            // RGB without alpha: App Store Connect rejects screenshots with an alpha channel
            let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
            match image::save_buffer(&path, &bytes, w as u32, h as u32, image::ColorType::Rgb8) {
                Ok(()) => eprintln!("screenshot saved to {}", path.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
            self.screenshot = None;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

/// The About box's license paragraph. App Store version: Apple's standard license agreement, at
/// home and at work; nothing there points to buying a license elsewhere (guideline 3.1.1).
#[cfg(feature = "mas")]
fn license_text(ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(RichText::new("This App Store version is licensed to you under ").color(theme::TEXT));
        ui.hyperlink_to("Apple's standard license agreement", APPLE_EULA_URL);
        ui.label(RichText::new(" and may be used at home and at work.").color(theme::TEXT));
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(RichText::new("Source code: ").color(theme::MUTED));
        ui.hyperlink_to("github.com/WietseWind/VMherd", REPO_URL);
        ui.label(RichText::new(" (PolyForm Noncommercial 1.0.0).").color(theme::MUTED));
    });
}

/// The About box's license paragraph (builds from the project page).
#[cfg(not(feature = "mas"))]
fn license_text(ui: &mut Ui) {
    ui.label(
        RichText::new(
            "Free for noncommercial use (PolyForm Noncommercial 1.0.0). Commercial use, also inside a \
             company, needs a commercial license from The Integrators BV (NL): ask via the project page.",
        )
        .color(theme::TEXT),
    );
}

/// Run futures concurrently and collect their results in order (no extra dependency needed).
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

/// The amber DEMO plate next to the session menu.
fn demo_badge(ui: &mut Ui) {
    let g = ui.painter().layout_job(widgets::spaced("DEMO", theme::bold(10.0), Color32::BLACK, 2.0));
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 12.0, 18.0), Sense::hover());
    ui.painter().rect_filled(r, 3.0, theme::AMBER);
    ui.painter().galley(r.center() - g.size() / 2.0, g, Color32::BLACK);
    resp.on_hover_text("Simulated VMs: nothing here touches a real server or the network, and nothing is saved");
}

fn power_label(a: PowerAction) -> &'static str {
    match a {
        PowerAction::Start => "Start",
        PowerAction::Shutdown => "Shutdown",
        PowerAction::Stop => "Stop",
    }
}

/// Columns and tile width: `fixed` columns, or (0) the largest tiles that fit the area.
pub fn layout(n: usize, w: f32, h: f32, fixed: u32) -> (usize, f32) {
    let g = theme::GAP;
    if fixed > 0 {
        let c = fixed as usize;
        return (c, ((w - (c - 1) as f32 * g) / c as f32).max(MIN_TILE_W));
    }
    let mut best = (1, 0.0_f32);
    for c in 1..=n.max(1) {
        let rows = n.div_ceil(c) as f32;
        let by_w = (w - (c - 1) as f32 * g) / c as f32;
        let by_h = ((h - (rows - 1.0) * g) / rows - theme::TILE_HEAD) * theme::SCREEN_ASPECT;
        let tw = by_w.min(by_h);
        if tw > best.1 {
            best = (c, tw);
        }
    }
    if best.1 >= MIN_TILE_W {
        return best;
    }
    // too many tiles to fit: as many columns of the minimum width as fit, the rest scrolls down
    let cols = (((w + g) / (MIN_TILE_W + g)).floor() as usize).clamp(1, n.max(1));
    (cols, ((w - (cols - 1) as f32 * g) / cols as f32).max(MIN_TILE_W))
}

const MIN_TILE_W: f32 = 220.0;

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        self.poll_vms();
        if let Some(s) = &self.session {
            let now = Instant::now();
            let mut wake = if s.polling { POLL } else { s.next_poll.saturating_duration_since(now) };
            let (backend, rt) = (s.backend.clone(), self.rt.handle().clone());
            for t in &mut self.tiles {
                if let Some(d) = t.tick(&rt, &backend, ctx, now) {
                    wake = wake.min(d);
                }
            }
            ctx.request_repaint_after(wake.max(Duration::from_millis(50)));
        }
        if self.typing.as_ref().is_some_and(Typing::is_finished)
            && let Some(t) = self.typing.take()
            && t.was_stopped()
        {
            self.toasts.error("Typing stopped.");
        }
        if self.save_at.is_some_and(|t| Instant::now() >= t) {
            self.save_now();
        }
        // ui() does not run while the window is hidden, so held keys are released here (only
        // releases: nothing from the stale input of the last shown frame is sent)
        let away = ctx.input(|i| i.viewport().focused == Some(false) || i.viewport().visible() == Some(false));
        if away {
            self.release_held();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Only here, never in logic(): while the window is hidden eframe calls logic() with the
        // previous frame's events, which would send those keys again.
        self.route_keys(&ctx);
        egui::Panel::top("top").exact_size(theme::TOP_H).frame(Frame::NONE).show(ui, |ui| self.top_bar(ui));

        if self.session.is_some() {
            let targets = self.target_tiles(Target::Broadcast).count();
            let progress = self.typing.as_ref().map(Typing::progress);
            let esc = self.esc_progress.filter(|_| self.target == Target::Broadcast);
            let mut out = bar::BarOut::default();
            egui::Panel::bottom("bar").exact_size(bar::HEIGHT).frame(Frame::NONE).show(ui, |ui| {
                out = self.bar.ui(ui, targets, progress, esc, &mut self.cfg.prefs);
            });
            if out.prefs_changed {
                self.save_soon();
            }
            if let Some(combo) = out.combo {
                self.send_combo(combo);
            }
            if let Some(on) = out.sync_all {
                for i in 0..self.tiles.len() {
                    self.tiles[i].sync = on;
                    let vmid = self.tiles[i].vmid();
                    self.set_sync(vmid, on);
                }
            }
            if out.stop_typing
                && let Some(t) = &self.typing
            {
                t.stop();
            }
            if out.type_text && !self.bar.text.is_empty() {
                let text = self.bar.text.replace("\r\n", "\n") + if self.cfg.prefs.add_enter { "\n" } else { "" };
                if typing::needs_ips(&text) {
                    self.lookup_ips_then_type(Target::Broadcast, text);
                } else {
                    self.type_text(Target::Broadcast, |vm| {
                        Some(typing::fill_template(&text, vm.vmid, vm.name.as_deref().unwrap_or(""), &vm.node, None))
                    });
                }
            }
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.session.is_some() {
                self.grid(ui);
            } else {
                ui.painter().rect_filled(ui.max_rect(), 0.0, theme::BG);
                widgets::grid_background(ui.painter(), ui.max_rect());
                if let Some(action) = self.clusters.ui(ui, &self.cfg, &self.status, self.persist) {
                    match action {
                        ClusterAction::Connect(id) => self.connect(id),
                        ClusterAction::Demo => self.connect_demo(Vec::new()),
                        ClusterAction::Save { bookmark, secret, forget_pin } => {
                            let result = self.save_bookmark(*bookmark, secret, forget_pin);
                            self.clusters.finish_save(result);
                        }
                        ClusterAction::Delete(id) => {
                            let removed = self
                                .cfg
                                .bookmarks
                                .iter()
                                .position(|b| b.id == id)
                                .map(|i| self.cfg.bookmarks.remove(i));
                            if let Some(b) = removed
                                && let Err(e) = secrets::delete(id, &b.secret)
                            {
                                self.toasts.error(e);
                            }
                            self.cancel_connect(id);
                            self.save_now();
                        }
                    }
                }
            }
        });

        let content = ctx.content_rect();
        if self.picker.open
            && let Some(s) = &self.session
        {
            let area = Rect::from_min_max(pos2(content.left(), content.top() + theme::TOP_H), content.max);
            let sel: Vec<u32> = self.tiles.iter().map(Tile::vmid).collect();
            let vms = s.vms.clone();
            let out = self.picker.ui(&ctx, area, &vms, &sel, &mut self.cfg.prefs);
            if out.prefs_changed {
                self.save_soon();
            }
            if let Some(ids) = out.sel {
                self.set_grid(ids, false);
            }
            if out.close {
                self.picker.open = false;
            }
        }
        self.shortcuts(&ctx);
        self.dialogs(&ctx);
        let bottom = if self.session.is_some() { content.bottom() - bar::HEIGHT } else { content.bottom() };
        self.toasts.ui(&ctx, content.right(), bottom);
        #[cfg(feature = "store-shots")]
        self.drive_scene(&ctx);
        #[cfg(not(feature = "mas"))]
        self.handle_screenshot(&ctx);
    }

    fn persist_egui_memory(&self) -> bool {
        self.persist
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self) {
        self.release_held();
        if self.save_at.is_some() {
            self.save_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::layout;

    #[test]
    fn auto_layout_fits() {
        // 9 tiles in 1400x700: 3 or more columns, all rows fit
        let (c, w) = layout(9, 1400.0, 700.0, 0);
        let rows = 9usize.div_ceil(c) as f32;
        assert!(rows * (28.0 + w / 1.6) + (rows - 1.0) * 10.0 <= 700.5, "c={c} w={w}");
        assert_eq!(layout(4, 1000.0, 700.0, 2).0, 2);
        assert_eq!(layout(1, 1000.0, 700.0, 0).0, 1);
    }
}
