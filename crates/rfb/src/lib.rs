//! Minimal async RFB (VNC) client, scoped to what QEMU (Proxmox consoles) speaks.
//!
//! * RFB 3.3 / 3.7 / 3.8, security "None" and "VNC Authentication" (DES challenge).
//! * Pixel format is fixed to 32 bpp true colour, little endian, R/G/B shifts 0/8/16, so the
//!   framebuffer is plain RGBA8 (alpha forced to 255) and can be uploaded to a GPU texture as is.
//! * Encodings: ZRLE, CopyRect, Raw; pseudo-encodings DesktopSize, ExtendedDesktopSize,
//!   DesktopName and QEMU Extended Key Event. The cursor is left to the server (drawn into the
//!   framebuffer), which is what QEMU does when the client does not ask for cursor updates.
//! * Keys are sent as QEMU extended key events (keysym + XT scancode) once the server announced
//!   support, so the guest maps them with its own layout like a physical keyboard. Before that,
//!   or for keys without a scancode, a plain RFB KeyEvent with the keysym is sent.
//!
//! The transport is any `AsyncRead + AsyncWrite` byte stream (TCP, or a websocket adapter).
//!
//! ```ignore
//! let fb = rfb::SharedFramebuffer::default();
//! let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
//! let notify: rfb::Notify = std::sync::Arc::new(|ev| println!("{ev:?}"));
//! rfb::run(stream, rfb::Config::with_password("secret"), fb.clone(), rx, notify).await?;
//! ```

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::UnboundedReceiver;

mod auth;
mod framebuffer;
pub mod keysym;
mod session;
mod zrle;

pub use framebuffer::{Framebuffer, Rect};

/// The framebuffer shared between the network task (writer) and the UI (reader).
/// Keep lock times short: the session holds it only while copying decoded pixels in.
pub type SharedFramebuffer = Arc<Mutex<Framebuffer>>;

/// Called from the network task for every [`Event`]; must be cheap and must not block
/// (typically: request a UI repaint).
pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

/// Session settings.
///
/// `Debug` redacts the password, so the config can be logged safely.
#[derive(Clone)]
pub struct Config {
    /// Password for VNC Authentication; only the first 8 bytes are used (protocol limit).
    pub password: Option<String>,
    /// Ask the server to keep other clients connected (ClientInit shared flag). Default `true`.
    pub shared: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { password: None, shared: true }
    }
}

impl Config {
    /// Default settings with a VNC password (for Proxmox: the ticket from `vncproxy`).
    pub fn with_password(password: impl Into<String>) -> Self {
        Self { password: Some(password.into()), ..Self::default() }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("shared", &self.shared)
            .finish()
    }
}

/// Input from the application to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientInput {
    /// A key press or release.
    ///
    /// `keysym` is the X11 keysym (see [`keysym`]), `qnum` the QEMU key number: the XT set 1
    /// scancode, with extended (0xE0-prefixed) keys encoded as `0x80 | low byte`
    /// (e.g. ArrowUp 0xE048 -> 0xC8). `qnum == 0` means "no scancode, send the keysym only".
    Key { keysym: u32, qnum: u32, down: bool },
    /// Absolute pointer position in framebuffer pixels and the button mask
    /// (bit 0 left, 1 middle, 2 right, 3/4 wheel up/down).
    Pointer { x: u16, y: u16, buttons: u8 },
    /// Ask for a complete (non-incremental) framebuffer update.
    RefreshFull,
}

/// What happened in the session (delivered through [`Notify`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Handshake done; the framebuffer has been sized.
    Connected { width: u32, height: u32, name: String },
    /// New pixels (see [`Framebuffer::take_dirty`]) or a size change (see [`Framebuffer::generation`]).
    Updated,
    /// The server rang the bell.
    Bell,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("unsupported server version {0:?}")]
    Version(String),
    #[error("no supported security type (server offers {0:?})")]
    NoSecurity(Vec<u8>),
    #[error("the server wants a VNC password")]
    PasswordRequired,
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("zlib: {0}")]
    Zlib(String),
}

/// Run one VNC session until the server closes the stream, an error occurs, or `input` is closed
/// (all senders dropped), whichever comes first. Returns `Ok(())` on a clean close.
///
/// The framebuffer is resized on ServerInit and on desktop-size changes; decoded rectangles are
/// written into it and marked dirty, then `notify(Event::Updated)` is called once per
/// FramebufferUpdate message.
pub async fn run<S>(
    stream: S,
    config: Config,
    fb: SharedFramebuffer,
    input: UnboundedReceiver<ClientInput>,
    notify: Notify,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    session::run(stream, config, fb, input, notify).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_debug_redacts_the_password() {
        let text = format!("{:?}", Config::with_password("hunter2"));
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(text, r#"Config { password: Some("<redacted>"), shared: true }"#);
        assert_eq!(format!("{:?}", Config::default()), "Config { password: None, shared: true }");
    }
}
