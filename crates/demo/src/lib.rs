//! VMherd's built-in demo cluster: 13 simulated guests (and a template) on three nodes, with
//! power actions, tasks, guest-agent addresses and live consoles.
//!
//! Nothing here touches the network. Each console is an in-process RFB 3.8 server on one end
//! of a `tokio::io::duplex` pipe; the app runs its normal RFB client on the other end. A
//! console shows an 80x25 terminal drawn in JetBrains Mono (960x600 pixels) with a pretend
//! root shell. All names and addresses are reserved documentation values (`*.example`,
//! RFC 5737 / RFC 3849 / RFC 7042).
//!
//! ```ignore
//! let cluster = demo::Cluster::new(demo::Fonts { regular, bold })?;
//! let stream = cluster.open_console(101).await?; // for rfb::run
//! ```

mod cluster;
mod data;
mod glyphs;
mod rfb_server;
mod shell;
mod term;

pub use cluster::Cluster;
pub use data::DEFAULT_GRID;

/// Console size in pixels.
pub const SCREEN: (u32, u32) = (glyphs::WIDTH as u32, glyphs::HEIGHT as u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Qemu,
    Lxc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Start,
    /// Graceful: the guest prints its shutdown, then is off.
    Shutdown,
    /// Hard power off.
    Stop,
}

/// The JetBrains Mono faces the consoles are drawn with (the app hands over the bytes it
/// already embeds for its own UI).
#[derive(Clone, Copy)]
pub struct Fonts {
    pub regular: &'static [u8],
    pub bold: &'static [u8],
}

/// A guest as the cluster lists it right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guest {
    pub vmid: u32,
    pub name: &'static str,
    pub node: &'static str,
    pub kind: Kind,
    pub running: bool,
    pub template: bool,
    /// `;`-separated
    pub tags: &'static str,
    /// Seconds (0 when off).
    pub uptime: u64,
}

/// What the guest agent reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Addresses {
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}
