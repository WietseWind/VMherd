//! Colours, fonts and egui style: the "control room" look.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x0a, 0x0c, 0x0b);
pub const PANEL: Color32 = Color32::from_rgb(0x12, 0x15, 0x13);
pub const PANEL2: Color32 = Color32::from_rgb(0x18, 0x1c, 0x19);
pub const TILE_TOP: Color32 = Color32::from_rgb(0x17, 0x1b, 0x18);
pub const INPUT_BG: Color32 = Color32::from_rgb(0x0b, 0x0e, 0x0c);
pub const PLATE: Color32 = Color32::from_rgb(0x1c, 0x21, 0x1d);
pub const PLATE_HI: Color32 = Color32::from_rgb(0x22, 0x28, 0x23);
pub const LINE: Color32 = Color32::from_rgb(0x26, 0x2c, 0x28);
pub const LINE2: Color32 = Color32::from_rgb(0x35, 0x3d, 0x38);
pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xde, 0xd9);
pub const MUTED: Color32 = Color32::from_rgb(0x78, 0x83, 0x7b);
pub const DIM: Color32 = Color32::from_rgb(0x4c, 0x55, 0x4f);
pub const GREEN: Color32 = Color32::from_rgb(0x3e, 0xe2, 0x7f);
pub const GREEN_DARK: Color32 = Color32::from_rgb(0x2f, 0xc7, 0x6b);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x3d, 0x3d);
pub const SALMON: Color32 = Color32::from_rgb(0xff, 0x8a, 0x7a);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb2, 0x1e);
pub const STEEL: Color32 = Color32::from_rgb(0x9d, 0xb4, 0xc9);
pub const INK: Color32 = Color32::from_rgb(0x04, 0x14, 0x0a);

pub const TOP_H: f32 = 46.0;
pub const GAP: f32 = 10.0;
pub const TILE_HEAD: f32 = 28.0;
/// Console area aspect ratio (width / height) of a tile.
pub const SCREEN_ASPECT: f32 = 16.0 / 10.0;

/// JetBrains Mono, also handed to the demo cluster for its consoles (statics: embedded once).
pub static JETBRAINS_MONO: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
pub static JETBRAINS_MONO_BOLD: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf");

const BOLD: &str = "bold";
const MONO_BOLD: &str = "mono-bold";

/// Barlow Semi Condensed Medium (the DIN-like UI face).
pub fn label(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Barlow Semi Condensed Bold.
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

/// JetBrains Mono.
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// JetBrains Mono Bold.
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_BOLD.into()))
}

fn font(bytes: &'static [u8]) -> Arc<FontData> {
    Arc::new(FontData::from_static(bytes))
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("barlow".into(), font(include_bytes!("../../../assets/fonts/BarlowSemiCondensed-Medium.ttf")));
    fonts
        .font_data
        .insert("barlow-bold".into(), font(include_bytes!("../../../assets/fonts/BarlowSemiCondensed-Bold.ttf")));
    fonts.font_data.insert("jbmono".into(), font(JETBRAINS_MONO));
    fonts.font_data.insert("jbmono-bold".into(), font(JETBRAINS_MONO_BOLD));

    // Symbols (▾ ↵ ● ⌘ ✕ ...) fall back to JetBrains Mono, then egui's bundled fonts (Hack, emoji).
    let bundled_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let bundled_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let chain = |first: &[&str], rest: &[&[String]]| -> Vec<String> {
        let mut out: Vec<String> = first.iter().map(|s| (*s).to_owned()).collect();
        for name in rest.iter().flat_map(|r| r.iter()) {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    };
    fonts.families.insert(FontFamily::Proportional, chain(&["barlow", "jbmono"], &[&bundled_prop, &bundled_mono]));
    fonts
        .families
        .insert(FontFamily::Name(BOLD.into()), chain(&["barlow-bold", "jbmono-bold"], &[&bundled_prop, &bundled_mono]));
    fonts.families.insert(FontFamily::Monospace, chain(&["jbmono"], &[&bundled_mono, &bundled_prop]));
    fonts
        .families
        .insert(FontFamily::Name(MONO_BOLD.into()), chain(&["jbmono-bold", "jbmono"], &[&bundled_mono, &bundled_prop]));
    ctx.set_fonts(fonts);

    ctx.options_mut(|o| o.zoom_with_keyboard = false); // Ctrl/Cmd +/- belong to the consoles
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, label(11.0)),
            (TextStyle::Body, label(14.0)),
            (TextStyle::Button, label(13.0)),
            (TextStyle::Heading, bold(20.0)),
            (TextStyle::Monospace, mono(12.0)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(9.0, 4.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.text_edit_width = 260.0;
        s.visuals = visuals();
    });
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, LINE2);
    v.window_corner_radius = CornerRadius::same(6);
    v.extreme_bg_color = INPUT_BG;
    v.faint_bg_color = PANEL2;
    v.code_bg_color = INPUT_BG;
    v.override_text_color = None;
    v.hyperlink_color = STEEL;
    v.selection.bg_fill = Color32::from_rgb(0x1d, 0x4a, 0x31);
    v.selection.stroke = Stroke::new(1.0, GREEN);
    v.text_cursor.stroke = Stroke::new(2.0, GREEN);
    let radius = CornerRadius::same(4);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
        w.fg_stroke = Stroke::new(1.0, TEXT);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.bg_fill = PLATE;
    v.widgets.inactive.weak_bg_fill = PLATE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE2);
    v.widgets.hovered.bg_fill = PLATE_HI;
    v.widgets.hovered.weak_bg_fill = PLATE_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    v.widgets.active.bg_fill = PLATE_HI;
    v.widgets.active.weak_bg_fill = PLATE_HI;
    v.widgets.active.bg_stroke = Stroke::new(1.0, GREEN);
    v.widgets.open.bg_fill = PLATE_HI;
    v.widgets.open.weak_bg_fill = PLATE_HI;
    v
}
