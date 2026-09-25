/// A rectangle in framebuffer pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = (self.x + self.w).max(other.x + other.w);
        let b = (self.y + self.h).max(other.y + other.h);
        Rect::new(x, y, r - x, b - y)
    }

    /// Clip to a `width` x `height` area; `None` when nothing is left.
    pub fn clip(&self, width: u32, height: u32) -> Option<Rect> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let r = self.x.saturating_add(self.w).min(width);
        let b = self.y.saturating_add(self.h).min(height);
        let rect = Rect::new(x, y, r - x, b - y);
        (!rect.is_empty()).then_some(rect)
    }
}

/// RGBA8 framebuffer (row-major, 4 bytes per pixel, alpha always 255) with dirty tracking.
#[derive(Debug, Default)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    dirty: Option<Rect>,
    generation: u64,
    name: String,
}

impl Framebuffer {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Desktop name from ServerInit / DesktopName.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// All pixels, `width * height * 4` bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Bumped whenever the size changes: a reader holding a texture of the old size must
    /// re-create it from [`Framebuffer::pixels`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The area changed since the last call (union of all updates), and clear it.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    /// Copy of the pixels in `rect` (clipped), `rect.w * rect.h * 4` bytes, with the clipped rect.
    pub fn region(&self, rect: Rect) -> Option<(Rect, Vec<u8>)> {
        let r = rect.clip(self.width, self.height)?;
        let stride = self.width as usize * 4;
        let row = r.w as usize * 4;
        let mut out = Vec::with_capacity(row * r.h as usize);
        for y in r.y..r.y + r.h {
            let start = y as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&self.pixels[start..start + row]);
        }
        Some((r, out))
    }

    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// Resize and clear to black; everything becomes dirty.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width as usize * height as usize * 4, 0);
        for px in self.pixels.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        self.generation += 1;
        self.dirty = Some(Rect::new(0, 0, width, height));
    }

    fn mark(&mut self, rect: Rect) {
        self.dirty = Some(match self.dirty {
            Some(d) => d.union(&rect),
            None => rect,
        });
    }

    /// Write `rgba` (`rect.w * rect.h * 4` bytes, alpha ignored and forced to 255) at `rect`.
    /// Parts outside the framebuffer are dropped.
    pub(crate) fn blit(&mut self, rect: Rect, rgba: &[u8]) {
        debug_assert_eq!(rgba.len(), rect.w as usize * rect.h as usize * 4);
        let Some(c) = rect.clip(self.width, self.height) else {
            return;
        };
        let stride = self.width as usize * 4;
        let src_stride = rect.w as usize * 4;
        let cols = c.w as usize * 4;
        for dy in 0..c.h as usize {
            let sy = (c.y - rect.y) as usize + dy;
            let src = &rgba[sy * src_stride + (c.x - rect.x) as usize * 4..][..cols];
            let dst_start = (c.y as usize + dy) * stride + c.x as usize * 4;
            let dst = &mut self.pixels[dst_start..dst_start + cols];
            dst.copy_from_slice(src);
            for px in dst.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
        self.mark(c);
    }

    /// Fill `rect` with one colour.
    pub(crate) fn fill(&mut self, rect: Rect, rgb: [u8; 3]) {
        let Some(c) = rect.clip(self.width, self.height) else {
            return;
        };
        let stride = self.width as usize * 4;
        for y in c.y..c.y + c.h {
            let start = y as usize * stride + c.x as usize * 4;
            for px in self.pixels[start..start + c.w as usize * 4].as_chunks_mut::<4>().0 {
                px.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
        self.mark(c);
    }

    /// CopyRect: copy the `dst.w` x `dst.h` area at (`src_x`, `src_y`) to `dst` (areas may overlap).
    pub(crate) fn copy_rect(&mut self, dst: Rect, src_x: u32, src_y: u32) {
        let Some(src) = Rect::new(src_x, src_y, dst.w, dst.h).clip(self.width, self.height) else {
            return;
        };
        let Some(d) = Rect::new(dst.x, dst.y, src.w, src.h).clip(self.width, self.height) else {
            return;
        };
        let (w, h) = (d.w.min(src.w) as usize, d.h.min(src.h) as usize);
        let stride = self.width as usize * 4;
        let rows: Box<dyn Iterator<Item = usize>> = if d.y > src.y { Box::new((0..h).rev()) } else { Box::new(0..h) };
        for row in rows {
            let s = (src.y as usize + row) * stride + src.x as usize * 4;
            let t = (d.y as usize + row) * stride + d.x as usize * 4;
            self.pixels.copy_within(s..s + w * 4, t);
        }
        self.mark(Rect::new(d.x, d.y, w as u32, h as u32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_and_clip() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert_eq!(Rect::new(8, 8, 10, 10).clip(10, 10), Some(Rect::new(8, 8, 2, 2)));
        assert_eq!(Rect::new(20, 0, 5, 5).clip(10, 10), None);
    }

    #[test]
    fn blit_fill_copy_and_dirty() {
        let mut fb = Framebuffer::default();
        fb.resize(4, 3);
        assert_eq!(fb.generation(), 1);
        assert_eq!(fb.take_dirty(), Some(Rect::new(0, 0, 4, 3)));
        assert_eq!(fb.take_dirty(), None);

        fb.fill(Rect::new(0, 0, 2, 1), [1, 2, 3]);
        assert_eq!(&fb.pixels()[..8], &[1, 2, 3, 255, 1, 2, 3, 255]);
        fb.blit(Rect::new(3, 2, 2, 1), &[9, 9, 9, 0, 7, 7, 7, 0]); // half outside
        assert_eq!(&fb.pixels()[(2 * 4 + 3) * 4..], &[9, 9, 9, 255]);
        assert_eq!(fb.take_dirty(), Some(Rect::new(0, 0, 4, 3)));

        fb.copy_rect(Rect::new(1, 1, 2, 1), 0, 0); // copy the filled pixels down-right
        let (r, px) = fb.region(Rect::new(1, 1, 2, 1)).unwrap();
        assert_eq!(r, Rect::new(1, 1, 2, 1));
        assert_eq!(px, vec![1, 2, 3, 255, 1, 2, 3, 255]);
    }
}
