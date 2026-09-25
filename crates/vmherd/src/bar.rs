//! Bottom bar: the broadcast box (ON AIR), special keys, sync all/none, and "type text".

use egui::{
    Align, Color32, ComboBox, CursorIcon, EventFilter, Id, Key, Layout, Modifiers, Rect, Response, RichText, Sense,
    Stroke, StrokeKind, TextEdit, Ui, vec2,
};
use rfb::keysym as ks;

use crate::config::Prefs;
use crate::theme;
use crate::widgets::{self, PlateStyle};

pub const HEIGHT: f32 = 104.0;

pub fn capture_id() -> Id {
    Id::new("broadcast-capture")
}

/// A key combination: (qnum, keysym) pressed in order, released in reverse.
pub type Combo = &'static [(u32, u32)];

const COMBOS: [(&str, &str, Combo); 9] = [
    ("↵", "Enter", &[(0x1c, ks::RETURN)]),
    ("Tab", "Tab", &[(0x0f, ks::TAB)]),
    ("Esc", "Escape", &[(0x01, ks::ESCAPE)]),
    ("↑", "Arrow up (shell history)", &[(0xc8, ks::UP)]),
    ("↓", "Arrow down", &[(0xd0, ks::DOWN)]),
    ("^C", "Ctrl+C", &[(0x1d, ks::CONTROL_L), (0x2e, 'c' as u32)]),
    ("^D", "Ctrl+D", &[(0x1d, ks::CONTROL_L), (0x20, 'd' as u32)]),
    ("^Z", "Ctrl+Z", &[(0x1d, ks::CONTROL_L), (0x2c, 'z' as u32)]),
    ("^L", "Ctrl+L", &[(0x1d, ks::CONTROL_L), (0x26, 'l' as u32)]),
];

#[derive(Default)]
pub struct Bar {
    pub text: String,
    /// The broadcast box had focus when the current pointer press started.
    capture_at_press: bool,
    /// A key button was used: give the focus back to the broadcast box (after it handled the click).
    refocus: std::cell::Cell<bool>,
}

#[derive(Default)]
pub struct BarOut {
    pub combo: Option<Combo>,
    pub sync_all: Option<bool>,
    pub type_text: bool,
    pub stop_typing: bool,
    pub prefs_changed: bool,
}

const PASTE_HINT: &str = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };

impl Bar {
    /// `targets`: consoles that would receive broadcast keys; `typing`: (done, total) while typing.
    /// `esc`: how far a long Esc press is (0..1) while the broadcast box has the keyboard.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        targets: usize,
        typing: Option<(usize, usize)>,
        esc: Option<f32>,
        prefs: &mut Prefs,
    ) -> BarOut {
        let mut out = BarOut::default();
        let rect = ui.max_rect();
        widgets::vgradient(ui.painter(), rect, theme::PANEL, Color32::from_rgb(0x0d, 0x10, 0x0e));
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::LINE));

        if ui.input(|i| i.pointer.any_pressed()) {
            self.capture_at_press = ui.memory(|m| m.has_focus(capture_id()));
        }

        let inner = rect.shrink2(vec2(10.0, 8.0));
        let row_h = (inner.height() - 6.0) / 2.0;
        let row1 = Rect::from_min_size(inner.min, vec2(inner.width(), row_h.min(32.0)));
        let row2 = Rect::from_min_max(egui::pos2(inner.left(), row1.bottom() + 6.0), inner.max);

        let mut r1 = ui.new_child(egui::UiBuilder::new().max_rect(row1).layout(Layout::right_to_left(Align::Center)));
        r1.spacing_mut().item_spacing.x = 3.0;
        for (label, on) in [("None", false), ("All", true)] {
            let r = self.key_button(&mut r1, label, &format!("Sync {}", label.to_lowercase()));
            if r.clicked() {
                out.sync_all = Some(on);
            }
        }
        r1.label(widgets::spaced("SYNC", theme::label(10.0), theme::MUTED, 1.6));
        r1.add_space(6.0);
        for (label, title, combo) in COMBOS.iter().rev() {
            let r = self.key_button(&mut r1, label, &format!("{title} → all synced consoles"));
            if r.clicked() {
                out.combo = Some(combo);
            }
        }
        r1.add_space(5.0);
        let w = r1.available_width();
        self.capture_box(&mut r1, w, targets, esc);
        if self.refocus.take() {
            r1.memory_mut(|m| m.request_focus(capture_id()));
        }

        let mut r2 = ui.new_child(egui::UiBuilder::new().max_rect(row2).layout(Layout::right_to_left(Align::Center)));
        r2.spacing_mut().item_spacing.x = 8.0;
        if let Some((done, total)) = typing {
            r2.label(RichText::new(format!("{done}/{total}")).font(theme::mono(12.0)).color(theme::MUTED));
            let stop = widgets::plate_button(
                &mut r2,
                widgets::spaced("STOP", theme::bold(12.0), Color32::WHITE, 1.4),
                vec2(64.0, 32.0),
                PlateStyle::DANGER,
                true,
            );
            if stop.clicked() {
                out.stop_typing = true;
            }
        } else {
            let go = widgets::plate_button(
                &mut r2,
                widgets::spaced("TYPE", theme::bold(12.0), theme::INK, 1.4),
                vec2(70.0, 32.0),
                PlateStyle::GO,
                !self.text.is_empty(),
            );
            if go
                .on_hover_text(format!(
                    "Type the text into all synced consoles ({})",
                    if cfg!(target_os = "macos") { "⌘↵" } else { "Ctrl+↵" }
                ))
                .clicked()
            {
                out.type_text = true;
            }
        }
        let delay_before = prefs.delay_ms;
        ComboBox::from_id_salt("type-delay")
            .width(78.0)
            .selected_text(RichText::new(format!("{} ms", prefs.delay_ms)).font(theme::mono(11.0)))
            .show_ui(&mut r2, |ui| {
                for d in [10, 20, 40, 80] {
                    ui.selectable_value(
                        &mut prefs.delay_ms,
                        d,
                        RichText::new(format!("{d} ms")).font(theme::mono(11.0)),
                    );
                }
            })
            .response
            .on_hover_text("Delay per character; raise it if characters get lost");
        if r2
            .checkbox(&mut prefs.add_enter, RichText::new("+ ↵").color(theme::MUTED))
            .on_hover_text("Press Enter after the text")
            .changed()
        {
            out.prefs_changed = true;
        }
        out.prefs_changed |= delay_before != prefs.delay_ms;

        let text_id = Id::new("type-text");
        if r2.memory(|m| m.has_focus(text_id))
            && r2.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter))
            && typing.is_none()
        {
            out.type_text = true;
        }
        let edit = TextEdit::multiline(&mut self.text)
            .id(text_id)
            .font(theme::mono(12.0))
            .desired_rows(2)
            .desired_width(r2.available_width())
            .margin(vec2(10.0, 6.0))
            .hint_text(
                "Text to type into all synced consoles.  Per VM: {{vmid}} {{name}} {{ipv4}} {{ipv6}} {{node}}.  E.g.  echo {{name}} {{ipv4}}",
            );
        r2.add_sized(vec2(r2.available_width(), row2.height()), edit);
        out
    }

    /// Plate button that keeps the broadcast box focused when it had focus.
    fn key_button(&self, ui: &mut Ui, label: &str, tip: &str) -> Response {
        let r = widgets::plate_button(
            ui,
            widgets::plain(label, theme::mono(11.0), theme::TEXT),
            vec2(34.0, 32.0),
            PlateStyle::DEFAULT,
            true,
        )
        .on_hover_text(tip);
        if self.capture_at_press && (r.is_pointer_button_down_on() || r.clicked()) {
            self.refocus.set(true);
        }
        r
    }

    fn capture_box(&mut self, ui: &mut Ui, width: f32, targets: usize, esc: Option<f32>) {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
        let id = capture_id();
        let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::Text);
        if resp.is_pointer_button_down_on() && !resp.has_focus() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        let painter = ui.painter();
        let s = if targets == 1 { "" } else { "s" };
        let (fill, stroke, text, color) = if focused {
            widgets::glow(painter, rect, 4, Color32::from_rgba_unmultiplied(255, 61, 61, 64), 16);
            (
                Color32::from_rgb(0x1c, 0x09, 0x09),
                Stroke::new(1.5, theme::RED),
                match esc {
                    Some(_) => "Keep holding Esc to leave  ·  release it to send Esc to the consoles".to_owned(),
                    None => format!(
                        "●  ON AIR → {targets} console{s}  ·  {PASTE_HINT} types the clipboard  ·  hold Esc to stop"
                    ),
                },
                Color32::from_rgb(0xff, 0x9c, 0x9c),
            )
        } else {
            (
                theme::INPUT_BG,
                Stroke::new(1.0, theme::LINE2),
                format!(
                    "⌨  Press Enter or click here to type into all {targets} synced console{s}  ·  click a console for just that one"
                ),
                theme::DIM,
            )
        };
        painter.rect_filled(rect, 4.0, fill);
        painter.rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
        let mut job = widgets::plain(&text, theme::mono(12.0), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 20.0);
        let g = painter.layout_job(job);
        painter.galley(egui::pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, color);
        if let (true, Some(p)) = (focused, esc) {
            let bar = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 2.0, rect.bottom() - 4.0),
                vec2((rect.width() - 4.0) * p.clamp(0.0, 1.0), 2.0),
            );
            painter.rect_filled(bar, 1.0, theme::RED);
        }
    }
}
