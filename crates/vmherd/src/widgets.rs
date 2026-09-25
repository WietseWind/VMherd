//! Small painted widgets: plates (buttons), lamps, the sync toggle, vector icons, gradients, glow.

use std::f32::consts::PI;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Id, Mesh, Painter, Pos2, Rect, Response, Sense, Shadow, Shape, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::theme;

/// First letter upper case (messages coming from libraries are lower case).
pub fn sentence(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// The app icon at `size` points (texture made once, kept in egui's memory).
pub fn logo(ui: &mut Ui, size: f32) {
    let id = Id::new("vmherd-logo");
    let tex = ui.ctx().data(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
        const PNG: &[u8] = include_bytes!("../../../assets/icon/vmherd-512.png");
        let image = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
            .map(|i| {
                let i = i.into_rgba8();
                egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
            })
            .unwrap_or_else(|_| egui::ColorImage::filled([1, 1], Color32::TRANSPARENT));
        let tex = ui.ctx().load_texture("vmherd-logo", image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, tex.clone()));
        tex
    });
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    glow(
        ui.painter(),
        rect.shrink(size * 0.14),
        (size * 0.22) as u8,
        Color32::from_rgba_unmultiplied(62, 226, 127, 40),
        60,
    );
    ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
}

/// "VMHERD" with "HERD" in green.
pub fn wordmark(size: f32) -> LayoutJob {
    let mut job = spaced("VM", theme::bold(size), theme::MUTED, size * 0.2);
    job.append(
        "HERD",
        0.0,
        TextFormat {
            font_id: theme::bold(size),
            color: theme::GREEN,
            extra_letter_spacing: size * 0.2,
            ..Default::default()
        },
    );
    job
}

/// Text with extra letter spacing.
pub fn spaced(text: &str, font: FontId, color: Color32, spacing: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(text, 0.0, TextFormat { font_id: font, color, extra_letter_spacing: spacing, ..Default::default() });
    job
}

pub fn plain(text: &str, font: FontId, color: Color32) -> LayoutJob {
    spaced(text, font, color, 0.0)
}

/// Vertical gradient (no rounding; use for bars and backgrounds).
pub fn vgradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(Shape::mesh(mesh));
}

/// Soft coloured glow around `rect` (CSS `box-shadow: 0 0 <blur> <color>`).
pub fn glow(painter: &Painter, rect: Rect, radius: u8, color: Color32, blur: u8) {
    let shadow = Shadow { offset: [0, 0], blur, spread: 0, color };
    painter.add(shadow.as_shape(rect, CornerRadius::same(radius)));
}

/// Status lamp; `lit` adds a halo.
pub fn lamp(painter: &Painter, center: Pos2, color: Color32, lit: bool) {
    if lit {
        painter.circle_filled(center, 6.5, color.gamma_multiply(0.22));
    }
    painter.circle_filled(center, 4.0, color);
}

/// Faint scan lines on console placeholders.
pub fn scanlines(painter: &Painter, rect: Rect) {
    let color = Color32::from_white_alpha(5);
    let mut y = rect.top();
    while y < rect.bottom() {
        painter.hline(rect.x_range(), y, Stroke::new(1.0, color));
        y += 3.0;
    }
}

/// Background grid of the main area.
pub fn grid_background(painter: &Painter, rect: Rect) {
    let color = Color32::from_white_alpha(5);
    let step = 24.0;
    let mut x = rect.left() - rect.left().rem_euclid(step);
    while x < rect.right() {
        painter.vline(x, rect.y_range(), Stroke::new(1.0, color));
        x += step;
    }
    let mut y = rect.top() - rect.top().rem_euclid(step);
    while y < rect.bottom() {
        painter.hline(rect.x_range(), y, Stroke::new(1.0, color));
        y += step;
    }
    // green haze at the top centre
    let haze = Rect::from_min_size(rect.min, vec2(rect.width(), 180.0_f32.min(rect.height())));
    vgradient(painter, haze, Color32::from_rgba_unmultiplied(62, 226, 127, 10), Color32::TRANSPARENT);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Power,
    Stop,
    Refresh,
    Maximize,
    Restore,
    Close,
}

fn arc(center: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    // angles in radians, 0 = up, clockwise
    let n = 24;
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            pos2(center.x + r * a.sin(), center.y - r * a.cos())
        })
        .collect()
}

pub fn paint_icon(painter: &Painter, c: Pos2, icon: Icon, color: Color32) {
    let s = Stroke::new(1.5, color);
    match icon {
        Icon::Play => {
            let pts = vec![pos2(c.x - 3.5, c.y - 4.5), pos2(c.x + 4.5, c.y), pos2(c.x - 3.5, c.y + 4.5)];
            painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Stop => {
            painter.rect_filled(Rect::from_center_size(c, vec2(8.0, 8.0)), 1.0, color);
        }
        Icon::Power => {
            painter.add(Shape::line(arc(c, 4.8, 0.2 * PI, 1.8 * PI), s));
            painter.line_segment([pos2(c.x, c.y - 6.2), pos2(c.x, c.y - 1.2)], s);
        }
        Icon::Refresh => {
            let pts = arc(c, 4.6, 0.15 * PI, 1.7 * PI);
            let end = *pts.last().unwrap_or(&c);
            painter.add(Shape::line(pts, s));
            let head =
                vec![pos2(end.x - 0.5, end.y - 3.2), pos2(end.x + 3.4, end.y + 0.2), pos2(end.x - 1.2, end.y + 1.6)];
            painter.add(Shape::convex_polygon(head, color, Stroke::NONE));
        }
        Icon::Maximize | Icon::Restore => {
            let (a, b) = (pos2(c.x - 4.5, c.y + 4.5), pos2(c.x + 4.5, c.y - 4.5));
            painter.line_segment([a, b], s);
            let head = |tip: Pos2, dx: f32, dy: f32| {
                painter.line_segment([tip, pos2(tip.x + dx * 3.5, tip.y)], s);
                painter.line_segment([tip, pos2(tip.x, tip.y + dy * 3.5)], s);
            };
            if icon == Icon::Maximize {
                head(b, -1.0, 1.0);
                head(a, 1.0, -1.0);
            } else {
                let mid = pos2(c.x + 1.0, c.y - 1.0);
                head(mid, 1.0, -1.0);
                let mid = pos2(c.x - 1.0, c.y + 1.0);
                head(mid, -1.0, 1.0);
            }
        }
        Icon::Close => {
            let d = 3.8;
            painter.line_segment([pos2(c.x - d, c.y - d), pos2(c.x + d, c.y + d)], s);
            painter.line_segment([pos2(c.x - d, c.y + d), pos2(c.x + d, c.y - d)], s);
        }
    }
}

/// Look of a plate button.
#[derive(Clone, Copy)]
pub struct PlateStyle {
    pub fill: Color32,
    pub stroke: Color32,
    pub hover_stroke: Color32,
    pub radius: u8,
}

impl PlateStyle {
    pub const DEFAULT: Self = Self { fill: theme::PLATE, stroke: theme::LINE2, hover_stroke: theme::MUTED, radius: 4 };
    pub const GO: Self =
        Self { fill: theme::GREEN_DARK, stroke: theme::GREEN_DARK, hover_stroke: theme::GREEN, radius: 4 };
    pub const DANGER: Self = Self { fill: theme::RED, stroke: theme::RED, hover_stroke: Color32::WHITE, radius: 4 };
}

pub fn paint_plate(painter: &Painter, rect: Rect, style: PlateStyle, hovered: bool) {
    let radius = CornerRadius::same(style.radius);
    painter.rect_filled(rect, radius, style.fill);
    // inset top highlight
    painter.hline(
        rect.x_range().shrink(style.radius as f32),
        rect.top() + 1.0,
        Stroke::new(1.0, Color32::from_white_alpha(10)),
    );
    let stroke = if hovered { style.hover_stroke } else { style.stroke };
    painter.rect_stroke(rect, radius, Stroke::new(1.0, stroke), StrokeKind::Inside);
}

/// A button showing a text job; returns the response (pointer cursor on hover).
pub fn plate_button(ui: &mut Ui, job: LayoutJob, min: Vec2, style: PlateStyle, enabled: bool) -> Response {
    let galley = ui.painter().layout_job(job);
    let size = vec2((galley.size().x + 18.0).max(min.x), min.y.max(galley.size().y + 8.0));
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let mut painter = ui.painter().clone();
        if !enabled {
            painter.multiply_opacity(0.3);
        }
        paint_plate(&painter, rect, style, response.hovered());
        let pos = rect.center() - galley.size() / 2.0;
        painter.galley(pos, galley, theme::TEXT);
        if response.has_focus() {
            focus_ring(&painter, rect, style.radius);
        }
    }
    if enabled { response.on_hover_cursor(CursorIcon::PointingHand) } else { response }
}

/// Keyboard focus outline (Tab navigation).
pub fn focus_ring(painter: &Painter, rect: Rect, radius: u8) {
    let r = rect.expand(2.5);
    glow(painter, r, radius + 2, Color32::from_rgba_unmultiplied(62, 226, 127, 60), 10);
    painter.rect_stroke(r, CornerRadius::same(radius + 2), Stroke::new(1.5, theme::GREEN), StrokeKind::Outside);
}

/// The red "sync" pill switch. Returns the response; flips `on` when clicked.
pub fn sync_toggle(ui: &mut Ui, id: Id, rect: Rect, on: &mut bool) -> Response {
    let response = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    if response.clicked() {
        *on = !*on;
    }
    let t = ui.ctx().animate_bool_with_time(id, *on, 0.15);
    let painter = ui.painter();
    let (bg, border) = if *on {
        (Color32::from_rgb(0x4a, 0x16, 0x16), Color32::from_rgb(0x7a, 0x27, 0x27))
    } else {
        (Color32::from_rgb(0x26, 0x2b, 0x27), theme::LINE2)
    };
    let radius = CornerRadius::same((rect.height() / 2.0) as u8);
    painter.rect_filled(rect, radius, bg);
    painter.rect_stroke(rect, radius, Stroke::new(1.0, border), StrokeKind::Inside);
    let r = rect.height() / 2.0 - 2.0;
    let x = egui::lerp((rect.left() + r + 2.0)..=(rect.right() - r - 2.0), t);
    let knob = if *on { theme::RED } else { theme::MUTED };
    painter.circle_filled(pos2(x, rect.center().y), r, knob);
    response
}
