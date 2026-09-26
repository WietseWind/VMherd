//! JetBrains Mono rasterized once into 12x24 coverage masks (skrifa outlines, vello_cpu fill),
//! then painted cell by cell into RGBX pixels.

use std::collections::HashMap;

use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use vello_cpu::color::{OpaqueColor, Srgb};
use vello_cpu::kurbo::{Affine, BezPath};
use vello_cpu::{Pixmap, RenderContext, Resources};

use crate::term::{COLS, Cell, Color, ROWS};

/// At 20 px JetBrains Mono advances exactly 12 px, so 80x25 cells of 12x24 make 960x600 (16:10).
pub const CELL_W: usize = 12;
pub const CELL_H: usize = 24;
pub const WIDTH: usize = COLS * CELL_W;
pub const HEIGHT: usize = ROWS * CELL_H;
const FONT_PX: f32 = 20.0;
const BASELINE: f64 = 18.0;
/// Besides printable ASCII.
const EXTRA: &str = "█▓▒░─│┌┐└┘·—…●→✓";

const BG: [u8; 3] = [0, 0, 0];

fn rgb(color: Color) -> [u8; 3] {
    match color {
        Color::Default => [0xc9, 0xcf, 0xca],
        Color::Bright => [0xf4, 0xf7, 0xf4],
        Color::Dim => [0x74, 0x7e, 0x77],
        Color::Red => [0xff, 0x6b, 0x60],
        Color::Green => [0x4a, 0xe8, 0x8a],
        Color::Yellow => [0xff, 0xc4, 0x4d],
        Color::Blue => [0x6c, 0xb6, 0xff],
        Color::Cyan => [0x7f, 0xe3, 0xe8],
    }
}

type Mask = Box<[u8; CELL_W * CELL_H]>;

pub struct Glyphs {
    regular: HashMap<char, Mask>,
    bold: HashMap<char, Mask>,
}

impl Glyphs {
    pub fn new(regular: &[u8], bold: &[u8]) -> Result<Self, String> {
        Ok(Self { regular: rasterize(regular)?, bold: rasterize(bold)? })
    }

    /// Paint `cell` with its top-left pixel at (`x`, `y`) of an RGBX buffer `stride` bytes wide.
    pub fn paint(&self, cell: Cell, out: &mut [u8], stride: usize, x: usize, y: usize) {
        let (mut fg, mut bg) = (rgb(cell.fg), BG);
        if cell.bold && cell.fg == Color::Default {
            fg = rgb(Color::Bright);
        }
        if cell.inverse {
            (fg, bg) = (BG, fg);
        }
        let set = if cell.bold { &self.bold } else { &self.regular };
        let mask = set.get(&cell.ch).or_else(|| (cell.ch != ' ').then(|| set.get(&'?')).flatten());
        for row in 0..CELL_H {
            let start = (y + row) * stride + x * 4;
            let line = &mut out[start..start + CELL_W * 4];
            for (col, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let a = mask.map_or(0, |m| u16::from(m[row * CELL_W + col]));
                for i in 0..3 {
                    let (f, b) = (u16::from(fg[i]), u16::from(bg[i]));
                    px[i] = ((f * a + b * (255 - a) + 127) / 255) as u8;
                }
                px[3] = 0;
            }
        }
    }

    /// Whether `c` has a glyph (or is a space).
    #[cfg(test)]
    pub fn has(&self, c: char) -> bool {
        c == ' ' || (self.regular.contains_key(&c) && self.bold.contains_key(&c))
    }
}

struct Pen<'a>(&'a mut BezPath);

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to((f64::from(x), -f64::from(y)));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to((f64::from(x), -f64::from(y)));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to((f64::from(cx0), -f64::from(cy0)), (f64::from(x), -f64::from(y)));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.curve_to(
            (f64::from(cx0), -f64::from(cy0)),
            (f64::from(cx1), -f64::from(cy1)),
            (f64::from(x), -f64::from(y)),
        );
    }

    fn close(&mut self) {
        self.0.close_path();
    }
}

fn rasterize(ttf: &[u8]) -> Result<HashMap<char, Mask>, String> {
    let font = skrifa::FontRef::new(ttf).map_err(|e| format!("demo font: {e}"))?;
    let charmap = font.charmap();
    let outlines = font.outline_glyphs();
    let mut ctx = RenderContext::new(CELL_W as u16, CELL_H as u16);
    let mut resources = Resources::new();
    let mut out = HashMap::new();
    for c in ('!'..='~').chain(EXTRA.chars()) {
        let Some(outline) = charmap.map(c).and_then(|id| outlines.get(id)) else { continue };
        let mut path = BezPath::new();
        let settings = DrawSettings::unhinted(Size::new(FONT_PX), LocationRef::default());
        if outline.draw(settings, &mut Pen(&mut path)).is_err() {
            continue;
        }
        ctx.reset();
        ctx.set_transform(Affine::translate((0.0, BASELINE)));
        ctx.set_paint(OpaqueColor::<Srgb>::WHITE);
        ctx.fill_path(&path);
        let mut pixmap = Pixmap::new(CELL_W as u16, CELL_H as u16);
        ctx.render(&mut pixmap, &mut resources);
        let mut mask: Mask = Box::new([0; CELL_W * CELL_H]);
        for (m, px) in mask.iter_mut().zip(pixmap.data_as_u8_slice().as_chunks::<4>().0) {
            *m = px[3];
        }
        out.insert(c, mask);
    }
    if !out.contains_key(&'?') {
        return Err("demo font has no glyphs".into());
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn glyphs() -> Glyphs {
        Glyphs::new(
            include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
            include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
        )
        .unwrap()
    }

    #[test]
    fn every_printable_character_has_ink() {
        let g = glyphs();
        for c in ('!'..='~').chain(EXTRA.chars()) {
            assert!(g.has(c), "{c:?}");
            let ink = |set: &HashMap<char, Mask>| set[&c].iter().map(|&a| u32::from(a)).sum::<u32>();
            assert!(ink(&g.regular) > 0 && ink(&g.bold) > 0, "{c:?} is empty");
        }
        // a space paints plain background, the cursor inverts it
        let mut px = vec![9u8; CELL_W * CELL_H * 4];
        g.paint(Cell::BLANK, &mut px, CELL_W * 4, 0, 0);
        assert!(px.iter().all(|&b| b == 0));
        g.paint(Cell { inverse: true, ..Cell::BLANK }, &mut px, CELL_W * 4, 0, 0);
        assert_eq!(&px[..4], &[0xc9, 0xcf, 0xca, 0]);
    }
}
