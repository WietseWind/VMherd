//! The VM picker: filterable, sortable table; click rows to add/remove tiles (shift-click = range).

use std::collections::BTreeMap;

use egui::{Color32, Event, Id, Order, Rect, RichText, Sense, Stroke, TextEdit, Ui, pos2, vec2};
use pve::VmResource;

use crate::config::{Prefs, SortKey};
use crate::theme;
use crate::tile::fmt_uptime;
use crate::widgets::{self, PlateStyle};

#[derive(Default)]
pub struct Picker {
    pub open: bool,
    last_clicked: Option<u32>,
    focus_filter: bool,
    /// VM row with keyboard focus (drawn highlighted)
    focused_row: Option<u32>,
}

pub struct PickerOut {
    /// New grid selection.
    pub sel: Option<Vec<u32>>,
    pub prefs_changed: bool,
    pub close: bool,
}

/// `a b, c` = (a and b) or c over vmid / name / node / status / tags; a list of numbers = exactly those VMIDs.
pub fn matches(vm: &VmResource, filter: &str) -> bool {
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        return true;
    }
    if f.chars().all(|c| c.is_ascii_digit() || c == ',' || c.is_whitespace()) {
        let id = vm.vmid.to_string();
        return f.split(|c: char| c == ',' || c.is_whitespace()).any(|t| t == id);
    }
    let hay = format!(
        "{} {} {} {} {}",
        vm.vmid,
        vm.name.as_deref().unwrap_or(""),
        vm.node,
        vm.status,
        vm.tags.as_deref().unwrap_or("").replace(';', " ")
    )
    .to_lowercase();
    f.split(',').any(|group| {
        let mut terms = group.split_whitespace().peekable();
        terms.peek().is_some() && terms.all(|t| hay.contains(t))
    })
}

fn sort(list: &mut [&VmResource], key: SortKey, desc: bool) {
    list.sort_by(|a, b| {
        let o = match key {
            SortKey::Vmid => a.vmid.cmp(&b.vmid),
            SortKey::Name => natural(a.name.as_deref().unwrap_or(""), b.name.as_deref().unwrap_or("")),
            SortKey::Node => natural(&a.node, &b.node),
            SortKey::Status => a.status.cmp(&b.status),
            SortKey::Tags => a.tags.cmp(&b.tags),
            SortKey::Uptime => a.uptime.cmp(&b.uptime),
        }
        .then(a.vmid.cmp(&b.vmid));
        if desc { o.reverse() } else { o }
    });
}

/// Natural order: "web-2" < "web-10".
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
                a.next();
                b.next();
            }
        }
    }
}

const ROW_H: f32 = 27.0;
const HEADERS: [(&str, Option<SortKey>); 7] = [
    ("", None),
    ("VMID", Some(SortKey::Vmid)),
    ("NAME", Some(SortKey::Name)),
    ("NODE", Some(SortKey::Node)),
    ("STATUS", Some(SortKey::Status)),
    ("TAGS", Some(SortKey::Tags)),
    ("UP", Some(SortKey::Uptime)),
];

/// Column x positions: fixed widths, the name column takes the rest.
struct Columns {
    x: [f32; 8],
}

impl Columns {
    fn new(width: f32) -> Self {
        const FIXED: [f32; 7] = [38.0, 62.0, 0.0, 110.0, 104.0, 130.0, 48.0];
        let name = (width - FIXED.iter().sum::<f32>()).max(150.0);
        let mut x = [0.0; 8];
        for i in 0..7 {
            x[i + 1] = x[i] + if i == 2 { name } else { FIXED[i] };
        }
        Self { x }
    }

    /// Cell `i` of a row (or the header) at `row`.
    fn cell(&self, row: Rect, i: usize) -> Rect {
        Rect::from_x_y_ranges(row.left() + self.x[i]..=row.left() + self.x[i + 1], row.y_range())
    }
}

impl Picker {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.focus_filter = self.open;
    }

    pub fn show(&mut self) {
        self.open = true;
        self.focus_filter = true;
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        area: Rect,
        vms: &BTreeMap<u32, VmResource>,
        sel: &[u32],
        prefs: &mut Prefs,
    ) -> PickerOut {
        let mut out = PickerOut { sel: None, prefs_changed: false, close: false };
        let width = area.width().min(780.0);
        let panel = Rect::from_min_size(area.min, vec2(width, area.height()));
        egui::Area::new(Id::new("picker")).order(Order::Foreground).fixed_pos(panel.min).show(ctx, |ui| {
            // The contents live in a child Ui, which does not size the Area. Claim the whole panel, or
            // egui thinks the pointer is over the layer below: no wheel scrolling, clicks fall through.
            ui.set_min_size(panel.size());
            // Tab and clicks stay inside the list while it is open
            ui.ctx().memory_mut(|m| m.set_modal_layer(ui.layer_id()));
            let _ = ui.interact(panel, Id::new("picker-background"), Sense::click());
            let painter = ui.painter();
            widgets::glow(painter, panel, 0, Color32::from_black_alpha(150), 40);
            painter.rect_filled(panel, 0.0, Color32::from_rgba_unmultiplied(0x0f, 0x12, 0x10, 245));
            painter.vline(panel.right(), panel.y_range(), Stroke::new(1.0, theme::LINE2));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(panel.shrink2(vec2(12.0, 0.0))));
            self.contents(&mut ui, vms, sel, prefs, &mut out);
        });
        // a press outside the panel closes it (the rest of the window does not react while it is open)
        let outside =
            ctx.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|p| !panel.contains(p)));
        if outside {
            out.close = true;
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &self,
        ui: &Ui,
        cols: &Columns,
        row: Rect,
        vm: &VmResource,
        slot: Option<usize>,
        hovered: bool,
        focused: bool,
    ) {
        let p = ui.painter();
        let bg = match (slot.is_some(), hovered || focused) {
            (true, true) => Some(Color32::from_rgb(0x14, 0x30, 0x23)),
            (true, false) => Some(Color32::from_rgb(0x10, 0x23, 0x1a)),
            (false, true) => Some(Color32::from_rgb(0x16, 0x1b, 0x17)),
            (false, false) => None,
        };
        if let Some(bg) = bg {
            p.rect_filled(row, 0.0, bg);
        }
        p.hline(row.x_range(), row.bottom() - 0.5, Stroke::new(1.0, Color32::from_rgb(0x1a, 0x1f, 0x1b)));
        if focused {
            p.rect_filled(Rect::from_min_size(row.left_top(), vec2(3.0, row.height())), 0.0, theme::GREEN);
            p.rect_stroke(
                row.shrink(0.5),
                2.0,
                Stroke::new(1.0, theme::GREEN.gamma_multiply(0.6)),
                egui::StrokeKind::Inside,
            );
        }
        let cy = row.center().y;
        let text = |i: usize, job: egui::text::LayoutJob| {
            let cell = cols.cell(row, i).shrink2(vec2(8.0, 0.0));
            let mut job = job;
            job.wrap = egui::text::TextWrapping::truncate_at_width(cell.width());
            let g = p.layout_job(job);
            p.galley(pos2(cell.left(), cy - g.size().y / 2.0), g, theme::TEXT);
        };
        // slot badge: pick order
        let badge = Rect::from_center_size(pos2(row.left() + cols.x[0] + 19.0, cy), vec2(24.0, 18.0));
        match slot {
            Some(n) => {
                p.rect_filled(badge, 3.0, theme::GREEN);
                let g = p.layout_no_wrap((n + 1).to_string(), theme::mono_bold(10.0), theme::INK);
                p.galley(badge.center() - g.size() / 2.0, g, theme::INK);
            }
            None => {
                p.rect_stroke(badge, 3.0, Stroke::new(1.0, theme::LINE2), egui::StrokeKind::Inside);
            }
        }
        text(1, widgets::plain(&vm.vmid.to_string(), theme::mono(12.0), theme::TEXT));
        text(2, widgets::plain(vm.name.as_deref().unwrap_or(""), theme::bold(14.0), theme::TEXT));
        text(3, widgets::plain(&vm.node, theme::mono(12.0), theme::TEXT));
        let status = cols.cell(row, 4);
        let (lamp, lit) = match vm.status.as_str() {
            "running" => (theme::GREEN, true),
            "stopped" => (theme::DIM, false),
            _ => (theme::AMBER, false),
        };
        widgets::lamp(p, pos2(status.left() + 13.0, cy), lamp, lit);
        let g = p.layout_job(widgets::spaced(&vm.status.to_uppercase(), theme::label(11.0), theme::MUTED, 1.1));
        p.galley(pos2(status.left() + 24.0, cy - g.size().y / 2.0), g, theme::MUTED);
        text(5, widgets::plain(&vm.tags.as_deref().unwrap_or("").replace(';', " "), theme::mono(11.0), theme::MUTED));
        text(6, widgets::plain(&fmt_uptime(vm.uptime.unwrap_or(0)), theme::mono(12.0), theme::TEXT));
    }

    fn contents(
        &mut self,
        ui: &mut Ui,
        vms: &BTreeMap<u32, VmResource>,
        sel: &[u32],
        prefs: &mut Prefs,
        out: &mut PickerOut,
    ) {
        ui.add_space(12.0);
        let filter_id = Id::new("picker-filter");
        // a pasted column of VMIDs becomes "101, 102, ..." instead of "101102"
        if ui.memory(|m| m.has_focus(filter_id)) {
            ui.input_mut(|i| {
                for e in &mut i.events {
                    if let Event::Paste(t) = e
                        && t.trim().contains('\n')
                    {
                        *t = t.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(", ");
                    }
                }
            });
        }
        // Widgets are created in reading order, which is also the Tab order:
        // filter, Running only, close, + All shown, − All shown, Clear, then the VM rows.
        let gap = ui.spacing().item_spacing.x;
        let text_w =
            |ui: &Ui, t: &str, font: egui::FontId| ui.painter().layout_no_wrap(t.into(), font, theme::TEXT).size().x;
        let check_w =
            ui.spacing().icon_width + ui.spacing().icon_spacing + text_w(ui, "Running only", theme::label(14.0));
        ui.horizontal(|ui| {
            let filter_w = (ui.available_width() - check_w - 30.0 - 3.0 * gap).max(120.0);
            let edit = TextEdit::singleline(&mut prefs.filter)
                .id(filter_id)
                .font(theme::mono(13.0))
                .hint_text("Filter by name, VMID, node or tag  (space = and, comma = or), or paste VMIDs")
                .desired_width(filter_w)
                .margin(vec2(10.0, 7.0));
            let r = ui.add(edit);
            if self.focus_filter {
                // select the old filter, so typing replaces it
                if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), filter_id) {
                    let all = egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(prefs.filter.chars().count()),
                    );
                    state.cursor.set_char_range(Some(all));
                    state.store(ui.ctx(), filter_id);
                }
                r.request_focus();
                self.focus_filter = false;
            }
            if r.changed() {
                out.prefs_changed = true;
                self.last_clicked = None;
            }
            if ui.checkbox(&mut prefs.only_running, RichText::new("Running only").color(theme::MUTED)).changed() {
                out.prefs_changed = true;
            }
            let close = widgets::plate_button(
                ui,
                widgets::plain("✕", theme::label(13.0), theme::TEXT),
                vec2(30.0, 30.0),
                PlateStyle::DEFAULT,
                true,
            );
            if close.on_hover_text("Close (Esc)").clicked() {
                out.close = true;
            }
        });

        let mut shown: Vec<&VmResource> = vms
            .values()
            .filter(|v| !v.template && (!prefs.only_running || v.status == "running") && matches(v, &prefs.filter))
            .collect();
        sort(&mut shown, prefs.sort_key, prefs.sort_desc);

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let info = format!(
                "{} shown · {} VMs · {} in grid",
                shown.len(),
                vms.values().filter(|v| !v.template).count(),
                sel.len()
            );
            ui.label(RichText::new(info).font(theme::mono(12.0)).color(theme::MUTED));
            const BUTTONS: [&str; 3] = ["+ All shown", "− All shown", "Clear"];
            let buttons_w: f32 =
                BUTTONS.iter().map(|t| text_w(ui, t, theme::label(12.0)) + 18.0).sum::<f32>() + 2.0 * gap;
            ui.add_space((ui.available_width() - buttons_w).max(0.0));
            let small = |ui: &mut Ui, t: &str| {
                widgets::plate_button(
                    ui,
                    widgets::plain(t, theme::label(12.0), theme::TEXT),
                    vec2(0.0, 26.0),
                    PlateStyle::DEFAULT,
                    true,
                )
            };
            if small(ui, BUTTONS[0]).clicked() {
                let mut s = sel.to_vec();
                s.extend(shown.iter().map(|v| v.vmid).filter(|id| !sel.contains(id)));
                out.sel = Some(s);
            }
            if small(ui, BUTTONS[1]).clicked() {
                out.sel = Some(sel.iter().copied().filter(|id| !shown.iter().any(|v| v.vmid == *id)).collect());
            }
            if small(ui, BUTTONS[2]).clicked() {
                out.sel = Some(Vec::new());
            }
        });
        ui.add_space(4.0);

        let hint_h = 34.0;
        let table_h = (ui.available_height() - hint_h).max(60.0);
        let mut clicked: Option<(usize, bool)> = None;
        let width = ui.available_width();
        let cols = Columns::new(width);

        // header: sort by clicking a column title (not a Tab stop)
        let (head, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
        ui.painter().rect_filled(head, 0.0, Color32::from_rgb(0x15, 0x19, 0x16));
        ui.painter().hline(head.x_range(), head.bottom(), Stroke::new(1.0, theme::LINE2));
        for (i, (label, key)) in HEADERS.iter().enumerate() {
            let Some(key) = key else { continue };
            let cell = cols.cell(head, i);
            let color = if *key == prefs.sort_key { theme::GREEN } else { theme::MUTED };
            let g = ui.painter().layout_job(widgets::spaced(label, theme::bold(10.0), color, 1.8));
            let r = Rect::from_min_size(pos2(cell.left() + 8.0, head.center().y - g.size().y / 2.0), g.size());
            ui.painter().galley(r.min, g, color);
            let resp = ui.interact(r.expand(4.0), Id::new(("picker-sort", i)), Sense::CLICK);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                prefs.sort_desc = prefs.sort_key == *key && !prefs.sort_desc;
                prefs.sort_key = *key;
                out.prefs_changed = true;
            }
        }

        // rows: every row is one widget, so Tab / arrows move per VM and Space / Enter toggles it
        let mut now_focused = None;
        egui::ScrollArea::vertical().max_height(table_h - 28.0).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, vm) in shown.iter().copied().enumerate() {
                let (row, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
                if resp.has_focus() {
                    now_focused = Some(vm.vmid);
                    if resp.gained_focus() {
                        resp.scroll_to_me(None);
                    }
                }
                if resp.clicked() {
                    clicked = Some((i, ui.input(|inp| inp.modifiers.shift)));
                }
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let slot = sel.iter().position(|id| *id == vm.vmid);
                self.paint_row(ui, &cols, row, vm, slot, resp.hovered(), resp.has_focus());
                if resp.hovered()
                    && let Some(tags) = vm.tags.as_deref().filter(|t| !t.is_empty())
                    && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| cols.cell(row, 5).contains(p))
                {
                    resp.on_hover_text_at_pointer(tags.replace(';', " "));
                }
            }
        });
        self.focused_row = now_focused;

        if let Some((i, shift)) = clicked {
            let id = shown[i].vmid;
            let last = self.last_clicked.and_then(|l| shown.iter().position(|v| v.vmid == l));
            let mut s = sel.to_vec();
            match (shift, last) {
                (true, Some(l)) => {
                    let range: Vec<u32> = shown[l.min(i)..=l.max(i)].iter().map(|v| v.vmid).collect();
                    if sel.contains(&id) {
                        s.retain(|x| !range.contains(x));
                    } else {
                        s.extend(range.into_iter().filter(|x| !sel.contains(x)));
                    }
                }
                _ => {
                    if let Some(p) = s.iter().position(|x| *x == id) {
                        s.remove(p);
                    } else {
                        s.push(id);
                    }
                }
            }
            self.last_clicked = Some(id);
            out.sel = Some(s);
        }

        let hint = ui.painter().layout_no_wrap(
            "Click rows to add/remove tiles (shift-click for a range). Tiles appear in the order you pick them.".into(),
            theme::label(12.0),
            theme::DIM,
        );
        let y = ui.max_rect().bottom() - hint_h + 10.0;
        ui.painter().hline(ui.max_rect().x_range(), ui.max_rect().bottom() - hint_h, Stroke::new(1.0, theme::LINE));
        ui.painter().galley(pos2(ui.max_rect().left(), y), hint, theme::DIM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pve::VmKind;

    fn vm(vmid: u32, name: &str, node: &str, status: &str, tags: &str) -> VmResource {
        VmResource {
            vmid,
            name: Some(name.into()),
            node: node.into(),
            status: status.into(),
            kind: VmKind::Qemu,
            template: false,
            tags: Some(tags.into()),
            uptime: None,
            lock: None,
            hastate: None,
        }
    }

    #[test]
    fn filter_semantics() {
        let a = vm(101, "lab.node-1", "pve2", "running", "lab;db");
        assert!(matches(&a, ""));
        assert!(matches(&a, "node pve2"));
        assert!(!matches(&a, "node pve1"));
        assert!(matches(&a, "pve1, node-1"));
        assert!(matches(&a, "101, 102"));
        assert!(!matches(&a, "106"));
        assert!(matches(&a, "lab"));
    }

    /// Headless: the mouse wheel over the list must scroll it (the scroll delta gets consumed).
    #[test]
    fn wheel_scrolls_the_vm_list() {
        use egui::{Event, Modifiers, MouseWheelUnit, Pos2, pos2};
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let vms: BTreeMap<u32, VmResource> =
            (100..300).map(|i| (i, vm(i, &format!("lab.web-{i}"), "pve1", "running", "lab"))).collect();
        let mut prefs = Prefs::default();
        let mut picker = Picker { open: true, ..Picker::default() };
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0));
        let area = Rect::from_min_max(pos2(0.0, 46.0), screen.max);
        let over = pos2(300.0, 420.0);
        let mut left = Vec::new();
        for frame in 0..40 {
            let mut events = vec![Event::PointerMoved(over)];
            if (3..10).contains(&frame) {
                events.push(Event::MouseWheel {
                    unit: MouseWheelUnit::Line,
                    delta: vec2(0.0, -3.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: Modifiers::NONE,
                });
            }
            let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                picker.ui(ui.ctx(), area, &vms, &[], &mut prefs);
            });
            out.textures_delta.clear();
            left.push(ctx.input(|i| i.smooth_scroll_delta.y));
        }
        assert_eq!(
            ctx.layer_id_at(over).map(|l| l.order),
            Some(egui::Order::Foreground),
            "pointer is not over the picker"
        );
        assert!(left.iter().all(|d| d.abs() < 0.01), "scroll delta not consumed by the list: {left:?}");
    }

    /// Headless: from the filter, Tab goes Running only, close, + All, − All, Clear, then the rows;
    /// Space on the focused row adds that VM.
    #[test]
    fn tab_to_the_first_row_and_space_toggles_it() {
        use egui::{Event, Key, Modifiers, Pos2};
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let vms: BTreeMap<u32, VmResource> =
            (100..130).map(|i| (i, vm(i, &format!("lab.web-{i}"), "pve1", "running", "lab"))).collect();
        let mut prefs = Prefs::default();
        let mut picker = Picker::default();
        picker.show();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0));
        let area = Rect::from_min_max(egui::pos2(0.0, 46.0), screen.max);
        let key =
            |key| Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: Modifiers::NONE };
        let mut script = vec![vec![], vec![]];
        script.extend((0..6).map(|_| vec![key(Key::Tab)]));
        script.push(vec![key(Key::Space)]);
        let mut chosen = None;
        for events in script {
            let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let o = picker.ui(ui.ctx(), area, &vms, &[], &mut prefs);
                if o.sel.is_some() {
                    chosen = o.sel;
                }
            });
            out.textures_delta.clear();
        }
        assert_eq!(chosen, Some(vec![100]));
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["web-10", "web-2", "Web-1"];
        v.sort_by(|a, b| natural(a, b));
        assert_eq!(v, vec!["Web-1", "web-2", "web-10"]);
    }
}
