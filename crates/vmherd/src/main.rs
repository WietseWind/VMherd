//! VMherd: many Proxmox VM consoles in one window, one keyboard for all of them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bar;
mod clusters;
mod config;
mod console;
mod input;
mod keys;
mod picker;
mod secrets;
mod texture;
mod theme;
mod tile;
mod toast;
mod typing;
mod widgets;

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "vmherd",
    version,
    author,
    about = "VMherd: many Proxmox VM consoles in one grid, with one keyboard for all of them.\nBy The Integrators BV (NL), Wietse Wind"
)]
struct Args {
    /// Connect to this cluster bookmark (by name) on start.
    #[arg(long, short)]
    cluster: Option<String>,
    /// VMIDs to put in the grid (uses --cluster, else the last used cluster).
    vmids: Vec<u32>,
    /// Save a PNG of the window after --screenshot-after seconds, then quit (docs / tests).
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
    #[arg(long, hide = true, default_value_t = 8.0)]
    screenshot_after: f64,
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,vmherd=info")))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let startup = app::Startup {
        cluster: args.cluster,
        vmids: args.vmids,
        screenshot: args.screenshot.map(|p| (p, args.screenshot_after)),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("VMherd")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native("vmherd", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, startup)?))))
}

/// The window / Dock icon (assets/icon, generated from vmherd.svg).
fn icon() -> egui::IconData {
    const PNG: &[u8] = include_bytes!("../../../assets/icon/vmherd-256.png");
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let img = img.into_rgba8();
            let (width, height) = img.dimensions();
            egui::IconData { rgba: img.into_raw(), width, height }
        }
        Err(e) => {
            tracing::warn!("bundled icon unreadable: {e}");
            egui::IconData::default()
        }
    }
}
