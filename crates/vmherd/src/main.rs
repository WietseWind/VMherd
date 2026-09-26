//! VMherd: many Proxmox VM consoles in one window, one keyboard for all of them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Scripted screenshots write files anywhere and are for development; the App Store build has none.
#[cfg(all(feature = "mas", feature = "store-shots"))]
compile_error!("the features \"mas\" (Mac App Store build) and \"store-shots\" cannot be combined");

mod app;
mod backend;
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

#[cfg(not(feature = "mas"))]
use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[cfg_attr(feature = "mas", derive(Default))]
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
    /// Start in the built-in demo cluster: 13 simulated VMs, no Proxmox server or network needed.
    /// Nothing is loaded from or saved to your settings.
    #[arg(long, conflicts_with = "cluster")]
    demo: bool,
    /// VMIDs to put in the grid (of --cluster, --demo, else the last used cluster).
    vmids: Vec<u32>,
    /// Save a PNG of the window after --screenshot-after seconds, then quit (docs / tests).
    #[cfg(not(feature = "mas"))]
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
    #[cfg(not(feature = "mas"))]
    #[arg(long, hide = true, default_value_t = 8.0)]
    screenshot_after: f64,
    /// Drive the demo into a store screenshot scene, save it to --out, then quit.
    #[cfg(feature = "store-shots")]
    #[arg(long, requires_all = ["demo", "out"], value_parser = clap::builder::PossibleValuesParser::new(app::SCENES))]
    scene: Option<String>,
    /// PNG file for --scene.
    #[cfg(feature = "store-shots")]
    #[arg(long, requires = "scene")]
    out: Option<PathBuf>,
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,vmherd=info")))
        .with_writer(std::io::stderr)
        .init();
    let args = parse_args();
    let demo = args.demo;
    let startup = app::Startup {
        cluster: args.cluster,
        vmids: args.vmids,
        #[cfg(not(feature = "mas"))]
        screenshot: args.screenshot.map(|p| (p, args.screenshot_after)),
        demo: args.demo,
        #[cfg(feature = "store-shots")]
        scene: args.scene.zip(args.out),
    };
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("VMherd")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(icon()),
        ..Default::default()
    };
    if demo {
        // --demo leaves no trace: no window geometry or UI state is restored or saved either
        // (nothing ever marks this storage file dirty, so it is never written)
        options.persist_window = false;
        options.persistence_path = Some(std::env::temp_dir().join("vmherd-demo-unsaved.ron"));
    }
    eframe::run_native("vmherd", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, startup)?))))
}

#[cfg(not(feature = "mas"))]
fn parse_args() -> Args {
    Args::parse()
}

/// Mac App Store build: an unexpected launch argument (the system or a tool may add some) is
/// ignored instead of ending the app; only an explicit --help / --version prints and exits.
#[cfg(feature = "mas")]
fn parse_args() -> Args {
    use clap::error::ErrorKind;
    match Args::try_parse() {
        Ok(args) => args,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => e.exit(),
        Err(e) => {
            tracing::warn!("ignoring the command line: {}", e.kind());
            Args::default()
        }
    }
}

/// The window / Dock icon (assets/icon, generated from vmherd-prompt.svg).
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
