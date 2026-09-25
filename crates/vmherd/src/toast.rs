//! Short messages in the bottom-right corner.

use std::time::{Duration, Instant};

use egui::{Color32, Id, Order, Rect, Stroke, StrokeKind, pos2, vec2};

use crate::theme;

struct Toast {
    text: String,
    err: bool,
    until: Instant,
}

#[derive(Default)]
pub struct Toasts {
    list: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(text.into(), false, Duration::from_secs(4));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(text.into(), true, Duration::from_secs(10));
    }

    fn push(&mut self, text: String, err: bool, ttl: Duration) {
        let text = crate::widgets::sentence(&text);
        tracing::info!(err, "{text}");
        self.list.push(Toast { text, err, until: Instant::now() + ttl });
        if self.list.len() > 8 {
            self.list.remove(0);
        }
    }

    /// Draw above `bottom` (y), right-aligned to `right` (x).
    pub fn ui(&mut self, ctx: &egui::Context, right: f32, bottom: f32) {
        let now = Instant::now();
        self.list.retain(|t| t.until > now);
        if let Some(next) = self.list.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next - now);
        }
        let mut y = bottom - 12.0;
        for (i, t) in self.list.iter().enumerate().rev() {
            let painter = ctx.layer_painter(egui::LayerId::new(Order::Tooltip, Id::new(("toast", i))));
            let color = if t.err { Color32::from_rgb(0xff, 0xc9, 0xc9) } else { theme::TEXT };
            let mut job = crate::widgets::plain(&t.text, theme::mono(12.0), color);
            job.wrap.max_width = 440.0;
            let g = painter.layout_job(job);
            let size = g.size() + vec2(24.0, 16.0);
            let rect = Rect::from_min_size(pos2(right - 14.0 - size.x, y - size.y), size);
            crate::widgets::glow(&painter, rect, 4, Color32::from_black_alpha(128), 24);
            painter.rect_filled(rect, 4.0, Color32::from_rgb(0x1b, 0x20, 0x1c));
            painter.rect_stroke(rect, 4.0, Stroke::new(1.0, theme::LINE2), StrokeKind::Inside);
            let accent = Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
            painter.rect_filled(
                accent,
                egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 },
                if t.err { theme::RED } else { theme::GREEN },
            );
            painter.galley(rect.min + vec2(14.0, 8.0), g, color);
            y = rect.top() - 6.0;
        }
    }
}
