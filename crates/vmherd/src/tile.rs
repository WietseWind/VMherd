//! One grid tile: header (sync toggle, VMID, name, node, status, power buttons) + live console.

use std::time::{Duration, Instant};

use egui::text::TextWrapping;
use egui::{
    Color32, CornerRadius, CursorIcon, EventFilter, Id, Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, Ui, UiBuilder,
    pos2, vec2,
};
use pve::{PowerAction, VmResource};
use rfb::ClientInput;
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::Backend;
use crate::console::{ConnState, Console};
use crate::texture::ScreenTexture;
use crate::theme;
use crate::widgets::{self, Icon, PlateStyle};

const ARM_TIME: Duration = Duration::from_secs(3);
const MAX_BACKOFF: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileAction {
    Power(PowerAction),
    Reconnect,
    ToggleMax,
    Remove,
    SyncChanged,
}

/// How the grid wants the tile drawn.
pub struct TileView {
    /// The broadcast box has keyboard focus.
    pub bcast: bool,
    pub maxed: bool,
    /// How far a long Esc press is (0..1), shown on the solo tile.
    pub esc: Option<f32>,
}

/// Id of the console area; it has keyboard focus in "solo" mode.
pub fn screen_id(vmid: u32) -> Id {
    Id::new(("console-screen", vmid))
}

pub struct Tile {
    pub vm: VmResource,
    pub sync: bool,
    console: Option<Console>,
    live: bool,
    note: String,
    retry: u32,
    next_attempt: Option<Instant>,
    screen: ScreenTexture,
    armed: Option<(PowerAction, Instant)>,
    last_pointer: Option<(u16, u16, u8)>,
    /// scroll not yet sent as wheel clicks (points)
    wheel: f32,
}

pub fn fmt_uptime(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

impl Tile {
    pub fn new(vm: VmResource, sync: bool) -> Self {
        Self {
            vm,
            sync,
            console: None,
            live: false,
            note: String::new(),
            retry: 0,
            next_attempt: None,
            screen: ScreenTexture::default(),
            armed: None,
            last_pointer: None,
            wheel: 0.0,
        }
    }

    pub fn vmid(&self) -> u32 {
        self.vm.vmid
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    fn running(&self) -> bool {
        self.vm.status == "running"
    }

    pub fn send(&self, input: ClientInput) {
        if let (true, Some(c)) = (self.live, &self.console) {
            c.send(input);
        }
    }

    pub fn sender(&self) -> Option<UnboundedSender<ClientInput>> {
        if self.live { self.console.as_ref().map(Console::sender) } else { None }
    }

    pub fn set_vm(&mut self, vm: VmResource) {
        self.vm = vm;
        if !self.running() {
            self.next_attempt = None;
            self.retry = 0;
        }
    }

    /// Retry soon without backoff (after a power action).
    pub fn retry_now(&mut self) {
        self.retry = 0;
        if self.console.is_none() {
            self.next_attempt = None;
        }
    }

    /// Show a power button armed, as after its first click (scripted screenshots).
    #[cfg(feature = "store-shots")]
    pub fn arm(&mut self, action: PowerAction) {
        self.armed = Some((action, Instant::now()));
    }

    pub fn reconnect(&mut self) {
        self.console = None;
        self.live = false;
        self.screen.clear();
        self.retry = 0;
        self.next_attempt = None;
        self.note = "Reconnecting…".into();
    }

    /// Drive the connection: open it while the VM runs, notice when it ends, back off between
    /// attempts. Returns when it wants to be called again.
    pub fn tick(
        &mut self,
        rt: &tokio::runtime::Handle,
        backend: &Backend,
        ctx: &egui::Context,
        now: Instant,
    ) -> Option<Duration> {
        if let Some(console) = &self.console {
            match console.state() {
                ConnState::Connecting => {}
                ConnState::Live => {
                    if !self.live {
                        self.live = true;
                        self.retry = 0;
                        self.note.clear();
                    }
                }
                ConnState::Ended(result) => {
                    let was_live = self.live;
                    self.console = None;
                    self.live = false;
                    self.screen.clear(); // the next session may have another size
                    self.note = match result {
                        Err(e) => widgets::sentence(&e),
                        Ok(()) if was_live => "Console closed, reconnecting…".into(),
                        Ok(()) => "Console closed".into(),
                    };
                    if self.running() {
                        let delay = Duration::from_secs(1u64 << self.retry.min(4)).min(MAX_BACKOFF);
                        self.retry += 1;
                        self.next_attempt = Some(now + delay);
                    }
                }
            }
        }
        if self.console.is_none() && self.running() {
            match self.next_attempt {
                Some(t) if t > now => return Some(t - now),
                _ => {
                    self.next_attempt = None;
                    self.note = "Connecting…".into();
                    self.console = Some(Console::open(rt, backend.clone(), self.vm.vm_ref(), ctx.clone()));
                }
            }
        }
        None
    }

    pub fn ui(&mut self, ui: &mut Ui, rect: Rect, view: &TileView) -> Option<TileAction> {
        let vmid = self.vmid();
        let id = Id::new(("tile", vmid));
        let sid = screen_id(vmid);
        let solo = ui.memory(|m| m.has_focus(sid));
        let on_air = view.bcast && self.sync && self.live;
        let now = Instant::now();
        if self.armed.is_some_and(|(_, t)| now.duration_since(t) > ARM_TIME) {
            self.armed = None;
        }

        let mut ui = ui.new_child(UiBuilder::new().max_rect(rect).id_salt(id));
        if view.bcast && !self.sync {
            ui.multiply_opacity(0.45);
        }
        let painter = ui.painter().clone();
        let radius = CornerRadius::same(6);
        painter.add(
            Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(115) }
                .as_shape(rect, radius),
        );
        if on_air {
            widgets::glow(&painter, rect, 6, Color32::from_rgba_unmultiplied(255, 61, 61, 70), 22);
        } else if solo {
            widgets::glow(&painter, rect, 6, Color32::from_rgba_unmultiplied(255, 178, 30, 76), 22);
        }
        painter.rect_filled(rect, radius, theme::TILE_TOP);

        let head = Rect::from_min_size(rect.min, vec2(rect.width(), theme::TILE_HEAD));
        let mut action = self.header(&mut ui, head, id, solo, view.maxed);

        let screen = Rect::from_min_max(pos2(rect.left(), head.bottom()), rect.max);
        if let Some(a) = self.screen_ui(&mut ui, screen, sid) {
            action = Some(a);
        }
        if let (true, Some(p)) = (solo, view.esc) {
            let bar = Rect::from_min_size(
                pos2(screen.left() + 1.0, screen.top() + 1.0),
                vec2((screen.width() - 2.0) * p.clamp(0.0, 1.0), 3.0),
            );
            painter.rect_filled(bar, 1.0, theme::AMBER);
        }

        let (width, color) = if on_air {
            (1.5, theme::RED)
        } else if solo {
            (1.5, theme::AMBER)
        } else {
            (1.0, theme::LINE)
        };
        painter.rect_stroke(rect, radius, Stroke::new(width, color), StrokeKind::Inside);
        if self.armed.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(40));
        }
        action
    }

    fn header(&mut self, ui: &mut Ui, head: Rect, id: Id, solo: bool, maxed: bool) -> Option<TileAction> {
        let painter = ui.painter().clone();
        let cy = head.center().y;
        let compact = head.width() < 440.0;
        let tiny = head.width() < 300.0;
        let running = self.running();
        let mut action = None;

        // left: sync toggle + VMID
        let mut x = head.left() + 8.0;
        let toggle = Rect::from_min_size(pos2(x, cy - 7.0), vec2(26.0, 14.0));
        let before = self.sync;
        widgets::sync_toggle(ui, id.with("sync"), toggle, &mut self.sync)
            .on_hover_text("Sync: this console receives the broadcast keys");
        if self.sync != before {
            action = Some(TileAction::SyncChanged);
        }
        x = toggle.right() + 7.0;
        let vmid = painter.layout_no_wrap(self.vm.vmid.to_string(), theme::mono_bold(12.0), theme::STEEL);
        painter.galley(pos2(x, cy - vmid.size().y / 2.0), vmid.clone(), theme::STEEL);
        x += vmid.size().x + 7.0;

        // right: buttons, right to left
        let mut rx = head.right() - 6.0;
        let buttons: [(&str, Icon, bool, &str); 6] = [
            ("remove", Icon::Close, !tiny, "Remove from grid"),
            ("max", if maxed { Icon::Restore } else { Icon::Maximize }, true, "Maximize / restore"),
            ("reconnect", Icon::Refresh, !tiny, "Reconnect console"),
            ("stop", Icon::Stop, true, "Stop (hard power off): click twice"),
            ("shutdown", Icon::Power, true, "Shutdown (ACPI, graceful): click twice"),
            ("start", Icon::Play, true, "Start"),
        ];
        for (name, icon, shown, tip) in buttons {
            if !shown {
                continue;
            }
            let power = match name {
                "start" => Some(PowerAction::Start),
                "shutdown" => Some(PowerAction::Shutdown),
                "stop" => Some(PowerAction::Stop),
                _ => None,
            };
            let enabled = match name {
                "start" => !running,
                "shutdown" | "stop" | "reconnect" => running,
                _ => true,
            };
            let armed = power.is_some() && self.armed.map(|(a, _)| a) == power;
            let w = if armed {
                46.0
            } else if compact {
                20.0
            } else {
                24.0
            };
            let r = Rect::from_min_size(pos2(rx - w, cy - 11.0), vec2(w, 22.0));
            rx -= w + 3.0;
            let resp = ui.interact(r, id.with(name), if enabled { Sense::click() } else { Sense::hover() });
            let mut p = painter.clone();
            if !enabled {
                p.multiply_opacity(0.25);
            }
            let style = if armed {
                PlateStyle { radius: 3, ..PlateStyle::DANGER }
            } else {
                PlateStyle { radius: 3, ..PlateStyle::DEFAULT }
            };
            let alpha = if armed { 0.7 + 0.3 * (ui.input(|i| i.time) * 12.0).cos() as f32 } else { 1.0 };
            let mut pp = p.clone();
            pp.multiply_opacity(alpha);
            widgets::paint_plate(&pp, r, style, enabled && resp.hovered());
            if armed {
                let label = if power == Some(PowerAction::Stop) { "STOP?" } else { "OFF?" };
                let g = p.layout_job(widgets::spaced(label, theme::bold(10.0), Color32::WHITE, 0.8));
                p.galley(r.center() - g.size() / 2.0, g, Color32::WHITE);
            } else {
                let color = match name {
                    "start" if enabled => theme::GREEN,
                    "shutdown" | "stop" if enabled => theme::SALMON,
                    _ => theme::TEXT,
                };
                widgets::paint_icon(&p, r.center(), icon, color);
            }
            if !enabled {
                continue;
            }
            let resp = resp.on_hover_text(tip).on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                action = match (name, power) {
                    (_, Some(PowerAction::Start)) => Some(TileAction::Power(PowerAction::Start)),
                    (_, Some(p)) if armed => {
                        self.armed = None;
                        Some(TileAction::Power(p))
                    }
                    (_, Some(p)) => {
                        self.armed = Some((p, Instant::now()));
                        None
                    }
                    ("reconnect", None) => Some(TileAction::Reconnect),
                    ("max", None) => Some(TileAction::ToggleMax),
                    ("remove", None) => Some(TileAction::Remove),
                    _ => None,
                }
                .or(action);
            }
        }

        if solo {
            let g = painter.layout_job(widgets::spaced("SOLO", theme::bold(10.0), Color32::BLACK, 2.0));
            let r = Rect::from_min_size(pos2(rx - g.size().x - 10.0, cy - 8.0), vec2(g.size().x + 10.0, 16.0));
            painter.rect_filled(r, 2.0, theme::AMBER);
            painter.galley(r.center() - g.size() / 2.0, g, Color32::BLACK);
            rx = r.left() - 6.0;
        }

        // middle: name, node, lamp + status (left to right, name shrinks first)
        let status = {
            let mut s = if running { fmt_uptime(self.vm.uptime.unwrap_or(0)) } else { self.vm.status.clone() };
            if s.is_empty() {
                s = "on".into();
            }
            if let Some(lock) = &self.vm.lock {
                s = format!("{s} · {lock}");
            }
            s.to_uppercase()
        };
        let status_g =
            (!compact).then(|| painter.layout_job(widgets::spaced(&status, theme::label(10.0), theme::MUTED, 1.4)));
        let node_g = (!compact).then(|| painter.layout_no_wrap(self.vm.node.clone(), theme::mono(11.0), theme::MUTED));
        let tail =
            node_g.as_ref().map_or(0.0, |g| g.size().x + 7.0) + 13.0 + status_g.as_ref().map_or(0.0, |g| g.size().x);
        let name_w = (rx - 8.0 - x - tail).max(12.0);

        let full = self.vm.name.clone().unwrap_or_else(|| "?".into());
        let (prefix, rest) = match full.split_once('.') {
            Some((p, r)) if !compact => (format!("{p}."), r.to_owned()),
            Some((_, r)) => (String::new(), r.to_owned()),
            None => (String::new(), full.clone()),
        };
        let mut job = widgets::plain(&prefix, theme::label(14.0), theme::MUTED);
        job.append(
            &rest,
            0.0,
            egui::TextFormat { font_id: theme::bold(14.0), color: theme::TEXT, ..Default::default() },
        );
        job.wrap =
            TextWrapping { max_width: name_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let name_g = painter.layout_job(job);
        let name_rect = Rect::from_min_size(pos2(x, cy - name_g.size().y / 2.0), name_g.size());
        painter.galley(name_rect.min, name_g, theme::TEXT);
        ui.interact(name_rect, id.with("name"), Sense::hover()).on_hover_text(&full);
        x = name_rect.right() + 7.0;
        if let Some(g) = node_g {
            painter.galley(pos2(x, cy - g.size().y / 2.0), g.clone(), theme::MUTED);
            x += g.size().x + 7.0;
        }
        let (lamp, lit) = match self.vm.status.as_str() {
            "running" => (theme::GREEN, true),
            "stopped" => (theme::DIM, false),
            _ => (theme::AMBER, false),
        };
        widgets::lamp(&painter, pos2(x + 4.0, cy), lamp, lit);
        if let Some(g) = status_g {
            painter.galley(pos2(x + 13.0, cy - g.size().y / 2.0), g, theme::MUTED);
        }
        action
    }

    fn screen_ui(&mut self, ui: &mut Ui, screen: Rect, sid: Id) -> Option<TileAction> {
        let painter = ui.painter().clone();
        let radius = CornerRadius { nw: 0, ne: 0, sw: 5, se: 5 };
        painter.rect_filled(screen, radius, Color32::BLACK);
        painter.hline(screen.x_range(), screen.top(), Stroke::new(1.0, theme::LINE));

        let resp = ui.interact(screen, sid, Sense::click_and_drag());
        if resp.is_pointer_button_down_on() && !resp.has_focus() {
            resp.request_focus();
        }
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    sid,
                    EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                );
            });
        }

        let mut shown = None;
        if let (true, Some(console)) = (self.live, &self.console) {
            let ppp = ui.ctx().pixels_per_point();
            let name = format!("console-{}", self.vm.vmid);
            if let Some((tex, size)) = self.screen.sync(ui.ctx(), &name, &console.fb, screen.width() * ppp) {
                let fit = fit(screen.shrink(1.0), size);
                painter.image(tex, fit, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                shown = Some((fit, size));
            }
        }

        let Some((fit, size)) = shown else {
            widgets::scanlines(&painter.with_clip_rect(screen.shrink(1.0)), screen.shrink(1.0));
            if self.vm.status == "stopped" {
                let g = painter.layout_job(widgets::spaced("POWERED OFF", theme::label(15.0), theme::DIM, 4.5));
                painter.galley(screen.center() - g.size() / 2.0, g, theme::DIM);
            } else if !self.note.is_empty() {
                let mut job = widgets::spaced(&self.note, theme::mono(11.0), theme::MUTED, 0.6);
                job.wrap.max_width = screen.width() - 24.0;
                job.halign = egui::Align::Center;
                let g = painter.layout_job(job);
                painter.galley(pos2(screen.center().x, screen.center().y - g.size().y / 2.0), g, theme::MUTED);
            }
            self.last_pointer = None;
            return None;
        };

        // pointer -> VM (absolute coordinates), like noVNC
        let dragging = resp.is_pointer_button_down_on();
        let buttons = if dragging {
            ui.input(|i| {
                u8::from(i.pointer.primary_down())
                    | u8::from(i.pointer.middle_down()) << 1
                    | u8::from(i.pointer.secondary_down()) << 2
            })
        } else {
            0
        };
        let pos = ui.input(|i| i.pointer.latest_pos()).filter(|p| dragging || fit.contains(*p));
        let at = pos.map(|p| {
            let fx = ((p.x - fit.left()) / fit.width() * size.x).clamp(0.0, size.x - 1.0) as u16;
            let fy = ((p.y - fit.top()) / fit.height() * size.y).clamp(0.0, size.y - 1.0) as u16;
            (fx, fy)
        });
        // outside the image: still send a button release, at the last position
        let at = at.or_else(|| self.last_pointer.filter(|l| l.2 != buttons).map(|l| (l.0, l.1)));
        if let Some((fx, fy)) = at {
            if self.last_pointer != Some((fx, fy, buttons)) {
                self.last_pointer = Some((fx, fy, buttons));
                self.send(ClientInput::Pointer { x: fx, y: fy, buttons });
            }
            // wheel in solo mode: one click per 40 points of scrolling (a mouse-wheel notch)
            if resp.has_focus() && resp.hovered() {
                self.wheel += ui.input_mut(|i| {
                    i.smooth_scroll_delta = egui::Vec2::ZERO;
                    i.events
                        .iter()
                        .map(|e| match e {
                            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, .. } => delta.y * 40.0,
                            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Page, delta, .. } => delta.y * 400.0,
                            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => delta.y,
                            _ => 0.0,
                        })
                        .sum::<f32>()
                });
                while self.wheel.abs() >= 40.0 {
                    let up = self.wheel > 0.0;
                    self.wheel -= if up { 40.0 } else { -40.0 };
                    let click = if up { 1 << 3 } else { 1 << 4 };
                    self.send(ClientInput::Pointer { x: fx, y: fy, buttons: buttons | click });
                    self.send(ClientInput::Pointer { x: fx, y: fy, buttons });
                }
            }
        }
        None
    }
}

/// Largest rect with the framebuffer's aspect ratio centred in `area`.
fn fit(area: Rect, size: egui::Vec2) -> Rect {
    let scale = (area.width() / size.x).min(area.height() / size.y);
    Rect::from_center_size(area.center(), size * scale)
}
