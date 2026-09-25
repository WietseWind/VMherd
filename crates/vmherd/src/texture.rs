//! Framebuffer -> GPU texture, box-filtered down to roughly the displayed size.
//!
//! egui's renderer has no mipmaps, so a 1280x800 console drawn at 300 px would alias badly.
//! Averaging k x k blocks on the CPU (only for dirty areas) keeps small tiles readable.

use egui::{ColorImage, TextureHandle, TextureId, TextureOptions, Vec2, vec2};
use rfb::{Framebuffer, Rect};

#[derive(Default)]
pub struct ScreenTexture {
    tex: Option<TextureHandle>,
    generation: u64,
    factor: u32,
    size: (u32, u32),
}

impl ScreenTexture {
    /// Bring the texture up to date. `display_px` is the drawn width in physical pixels.
    /// Returns the texture and the framebuffer size (for aspect ratio and pointer mapping).
    pub fn sync(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        fb: &rfb::SharedFramebuffer,
        display_px: f32,
    ) -> Option<(TextureId, Vec2)> {
        let mut fb = fb.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (w, h) = (fb.width(), fb.height());
        if w == 0 || h == 0 {
            return None;
        }
        let factor = pick_factor(w, display_px, self.factor);
        let rebuild =
            self.tex.is_none() || fb.generation() != self.generation || factor != self.factor || (w, h) != self.size;
        if rebuild {
            fb.take_dirty();
            let image = downsample(&fb, Rect::new(0, 0, w, h), factor);
            match &mut self.tex {
                Some(tex) => tex.set(image, TextureOptions::LINEAR),
                None => self.tex = Some(ctx.load_texture(name, image, TextureOptions::LINEAR)),
            }
            self.generation = fb.generation();
            self.factor = factor;
            self.size = (w, h);
        } else if let (Some(dirty), Some(tex)) = (fb.take_dirty(), &mut self.tex) {
            let aligned = align(dirty, factor, w, h);
            let image = downsample(&fb, aligned, factor);
            tex.set_partial(
                [(aligned.x / factor) as usize, (aligned.y / factor) as usize],
                image,
                TextureOptions::LINEAR,
            );
        }
        drop(fb);
        self.tex.as_ref().map(|t| (t.id(), vec2(w as f32, h as f32)))
    }

    pub fn clear(&mut self) {
        self.tex = None;
    }
}

/// Integer shrink factor so the texture is at least as large as the display (1 = full size);
/// sticks to the current factor within a small band to avoid rebuilding on every resize step.
fn pick_factor(fb_width: u32, display_px: f32, current: u32) -> u32 {
    let ratio = fb_width as f32 / display_px.max(1.0);
    let ideal = ratio.floor().clamp(1.0, 8.0) as u32;
    let keep = (1..=8).contains(&current) && current as f32 <= ratio + 0.15 && current as f32 > ratio - 1.0;
    if keep { current } else { ideal }
}

/// Grow `r` to multiples of `k` (clipped to the framebuffer).
fn align(r: Rect, k: u32, w: u32, h: u32) -> Rect {
    let x0 = r.x / k * k;
    let y0 = r.y / k * k;
    let x1 = (r.x + r.w).div_ceil(k).saturating_mul(k).min(w);
    let y1 = (r.y + r.h).div_ceil(k).saturating_mul(k).min(h);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// Average `k` x `k` blocks of `r` (which starts on a multiple of `k`).
fn downsample(fb: &Framebuffer, r: Rect, k: u32) -> ColorImage {
    let (ow, oh) = (r.w.div_ceil(k) as usize, r.h.div_ceil(k) as usize);
    let stride = fb.width() as usize * 4;
    let px = fb.pixels();
    if k == 1 {
        let mut out = Vec::with_capacity(ow * oh * 4);
        for y in r.y..r.y + r.h {
            let start = y as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&px[start..start + r.w as usize * 4]);
        }
        return ColorImage::from_rgba_premultiplied([ow, oh], &out);
    }
    let mut out = vec![0u8; ow * oh * 4];
    let (x_end, y_end) = ((r.x + r.w) as usize, (r.y + r.h) as usize);
    for oy in 0..oh {
        let y0 = r.y as usize + oy * k as usize;
        let y1 = (y0 + k as usize).min(y_end);
        for ox in 0..ow {
            let x0 = r.x as usize + ox * k as usize;
            let x1 = (x0 + k as usize).min(x_end);
            let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);
            for y in y0..y1 {
                let row = &px[y * stride + x0 * 4..y * stride + x1 * 4];
                for p in row.as_chunks::<4>().0 {
                    sr += u32::from(p[0]);
                    sg += u32::from(p[1]);
                    sb += u32::from(p[2]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)).max(1) as u32;
            let o = (oy * ow + ox) * 4;
            out[o] = (sr / n) as u8;
            out[o + 1] = (sg / n) as u8;
            out[o + 2] = (sb / n) as u8;
            out[o + 3] = 255;
        }
    }
    ColorImage::from_rgba_premultiplied([ow, oh], &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_choice() {
        assert_eq!(pick_factor(1280, 2560.0, 0), 1);
        assert_eq!(pick_factor(1280, 640.0, 0), 2);
        assert_eq!(pick_factor(1280, 300.0, 0), 4);
        assert_eq!(pick_factor(1280, 100.0, 0), 8);
    }

    #[test]
    fn alignment() {
        assert_eq!(align(Rect::new(5, 5, 2, 2), 4, 100, 100), Rect::new(4, 4, 4, 4));
        assert_eq!(align(Rect::new(97, 0, 3, 1), 4, 99, 100), Rect::new(96, 0, 3, 4));
    }
}
