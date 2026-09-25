//! Handshake and message loop.
//!
//! After the handshake the stream is used by two futures that run concurrently inside one
//! `select!`: the reader decodes server messages into the framebuffer, the writer sends the
//! application's input and the reader's update requests. Neither future is polled from a
//! `select!` of its own, so a message is never abandoned half-read or half-written; the session
//! ends as soon as either of them finishes.

mod handshake;
mod wire;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{MutexGuard, PoisonError};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::zrle::{self, Decoded, ZrleDecoder};
use crate::{ClientInput, Config, Error, Event, Framebuffer, Notify, Rect, SharedFramebuffer};

/// Read buffer size (server updates arrive in large bursts).
const READ_BUFFER: usize = 64 * 1024;
/// Largest accepted ServerCutText (skipped, never stored).
const MAX_CUT_TEXT: u32 = 16 << 20;
/// Scratch buffers keep at least this much capacity between updates.
const MIN_RETAINED: usize = 1 << 20;

// Every pixel rectangle lies inside the framebuffer, so the decoder's cap never rejects one.
const _: () = assert!(wire::MAX_PIXELS as usize * 4 <= zrle::MAX_DECODED);

/// State shared by the reader and the writer of one session.
#[derive(Debug, Default)]
struct Shared {
    /// The server supports QEMU Extended Key Events.
    ext_keys: AtomicBool,
    /// The framebuffer was resized: the next update request asks for everything.
    full_update: AtomicBool,
}

pub(crate) async fn run<S>(
    stream: S,
    config: Config,
    fb: SharedFramebuffer,
    input: UnboundedReceiver<ClientInput>,
    notify: Notify,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (read_half, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::with_capacity(READ_BUFFER, read_half);

    let init = handshake::handshake(&mut reader, &mut writer, &config).await?;
    {
        let mut fb = lock(&fb);
        fb.resize(init.width, init.height);
        fb.set_name(init.name.clone());
    }
    notify(Event::Connected { width: init.width, height: init.height, name: init.name });

    let (width, height) = (to_u16(init.width), to_u16(init.height));
    let mut buf = Vec::with_capacity(64);
    wire::set_pixel_format(&mut buf);
    wire::send(&mut writer, &buf).await?;
    buf.clear();
    wire::set_encodings(&mut buf);
    wire::send(&mut writer, &buf).await?;
    buf.clear();
    wire::update_request(&mut buf, false, width, height);
    wire::send(&mut writer, &buf).await?;

    let shared = Shared::default();
    // Capacity 1: update requests coalesce, the writer always uses the current size.
    let (update_tx, update_rx) = mpsc::channel(1);
    let server = ServerReader {
        stream: reader,
        fb: &fb,
        notify: &notify,
        shared: &shared,
        updates: update_tx,
        zrle: ZrleDecoder::default(),
        payload: Vec::new(),
        pixels: Vec::new(),
        width: init.width,
        height: init.height,
    };
    tokio::select! {
        result = server.run() => result,
        result = write_loop(writer, input, update_rx, &fb, &shared) => result,
    }
}

/// Lock the framebuffer; a panic elsewhere while holding it leaves plain pixel data behind,
/// which is still fine to use.
fn lock(fb: &SharedFramebuffer) -> MutexGuard<'_, Framebuffer> {
    fb.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Framebuffer edges are validated to at most 16384, so this never saturates in practice.
fn to_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn current_size(fb: &SharedFramebuffer) -> (u16, u16) {
    let fb = lock(fb);
    (to_u16(fb.width()), to_u16(fb.height()))
}

/// Sends input and update requests until `input` is closed (clean end) or a write fails.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut input: UnboundedReceiver<ClientInput>,
    mut updates: mpsc::Receiver<()>,
    fb: &SharedFramebuffer,
    shared: &Shared,
) -> Result<(), Error> {
    let mut buf = Vec::with_capacity(16);
    loop {
        buf.clear();
        tokio::select! {
            Some(()) = updates.recv() => {
                let incremental = !shared.full_update.swap(false, Ordering::Relaxed);
                let (width, height) = current_size(fb);
                wire::update_request(&mut buf, incremental, width, height);
            }
            message = input.recv() => match message {
                Some(message) => encode_input(&mut buf, message, fb, shared),
                None => {
                    tracing::debug!("input closed, ending the session");
                    return Ok(());
                }
            },
        }
        if !buf.is_empty() {
            wire::send(&mut writer, &buf).await?;
        }
    }
}

/// Encode one application input as a client message (nothing for a key without keysym and
/// scancode).
fn encode_input(buf: &mut Vec<u8>, input: ClientInput, fb: &SharedFramebuffer, shared: &Shared) {
    match input {
        ClientInput::Key { keysym, qnum, down } => {
            if qnum != 0 && shared.ext_keys.load(Ordering::Relaxed) {
                wire::qemu_key_event(buf, keysym, qnum, down);
            } else if keysym != 0 {
                wire::key_event(buf, keysym, down);
            }
        }
        ClientInput::Pointer { x, y, buttons } => wire::pointer_event(buf, x, y, buttons),
        ClientInput::RefreshFull => {
            let (width, height) = current_size(fb);
            wire::update_request(buf, false, width, height);
        }
    }
}

/// Rectangle header of a FramebufferUpdate.
#[derive(Clone, Copy, Debug)]
struct RectHeader {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    encoding: i32,
}

impl RectHeader {
    fn parse(raw: &[u8; 12]) -> Self {
        let u16_at = |i: usize| u16::from_be_bytes([raw[i], raw[i + 1]]);
        Self {
            x: u16_at(0),
            y: u16_at(2),
            w: u16_at(4),
            h: u16_at(6),
            encoding: i32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]),
        }
    }

    /// The pixel area, which must lie inside the current `width` x `height` framebuffer (servers
    /// clip updates to it). This bounds every per-rectangle buffer by the framebuffer size.
    fn pixel_rect(&self, width: u32, height: u32) -> Result<Rect, Error> {
        let (x, y, w, h) = (u32::from(self.x), u32::from(self.y), u32::from(self.w), u32::from(self.h));
        if x + w > width || y + h > height {
            return Err(Error::Protocol(format!("rectangle {w}x{h}+{x}+{y} exceeds the {width}x{height} framebuffer")));
        }
        Ok(Rect::new(x, y, w, h))
    }
}

/// Decodes server messages into the framebuffer until EOF or an error.
struct ServerReader<'a, R> {
    stream: R,
    fb: &'a SharedFramebuffer,
    notify: &'a Notify,
    shared: &'a Shared,
    updates: mpsc::Sender<()>,
    zrle: ZrleDecoder,
    /// Scratch: compressed ZRLE payload.
    payload: Vec<u8>,
    /// Scratch: decoded RGBA pixels of one rectangle.
    pixels: Vec<u8>,
    /// Current framebuffer size (only this reader changes it).
    width: u32,
    height: u32,
}

impl<R: AsyncRead + Unpin> ServerReader<'_, R> {
    async fn run(mut self) -> Result<(), Error> {
        loop {
            let mut kind = [0u8; 1];
            if self.stream.read(&mut kind).await? == 0 {
                tracing::debug!("server closed the connection");
                return Ok(());
            }
            match kind[0] {
                0 => self.framebuffer_update().await?,
                1 => self.colour_map_entries().await?,
                2 => (self.notify)(Event::Bell),
                3 => self.cut_text().await?,
                other => {
                    return Err(Error::Protocol(format!("unknown server message type {other}")));
                }
            }
        }
    }

    async fn framebuffer_update(&mut self) -> Result<(), Error> {
        let mut head = [0u8; 3]; // padding, number of rectangles
        self.stream.read_exact(&mut head).await?;
        let count = u16::from_be_bytes([head[1], head[2]]);
        for _ in 0..count {
            self.rectangle().await?;
        }
        self.release_excess();
        (self.notify)(Event::Updated);
        // A full channel means a request is already pending: nothing is lost by coalescing.
        let _ = self.updates.try_send(());
        Ok(())
    }

    async fn rectangle(&mut self) -> Result<(), Error> {
        let mut raw = [0u8; 12];
        self.stream.read_exact(&mut raw).await?;
        let header = RectHeader::parse(&raw);
        let (width, height) = (self.width, self.height);
        match header.encoding {
            wire::ENCODING_RAW => self.raw(header.pixel_rect(width, height)?).await,
            wire::ENCODING_COPY_RECT => self.copy_rect(header.pixel_rect(width, height)?).await,
            wire::ENCODING_ZRLE => self.zrle(header.pixel_rect(width, height)?).await,
            wire::ENCODING_DESKTOP_SIZE => self.resize(header.w, header.h),
            wire::ENCODING_EXTENDED_DESKTOP_SIZE => self.extended_desktop_size(header).await,
            wire::ENCODING_DESKTOP_NAME => self.desktop_name().await,
            wire::ENCODING_QEMU_EXT_KEY => {
                tracing::debug!("server supports QEMU extended key events");
                self.shared.ext_keys.store(true, Ordering::Relaxed);
                Ok(())
            }
            other => Err(Error::Protocol(format!("unsupported encoding {other}"))),
        }
    }

    /// Raw: `rect` lies inside the framebuffer, so this reads at most `MAX_PIXELS * 4` bytes.
    async fn raw(&mut self, rect: Rect) -> Result<(), Error> {
        let len = rect.w as usize * rect.h as usize * 4;
        wire::read_into(&mut self.stream, len, &mut self.pixels).await?;
        lock(self.fb).blit(rect, &self.pixels);
        Ok(())
    }

    async fn copy_rect(&mut self, rect: Rect) -> Result<(), Error> {
        let src_x = self.stream.read_u16().await?;
        let src_y = self.stream.read_u16().await?;
        lock(self.fb).copy_rect(rect, u32::from(src_x), u32::from(src_y));
        Ok(())
    }

    /// ZRLE: the payload may not exceed what a valid rectangle of this size can compress to.
    async fn zrle(&mut self, rect: Rect) -> Result<(), Error> {
        let (width, height) = (rect.w as usize, rect.h as usize);
        let len = self.stream.read_u32().await?;
        let max = zrle::max_payload_len(width, height);
        let len = usize::try_from(len).ok().filter(|&len| len <= max).ok_or_else(|| {
            Error::Protocol(format!("ZRLE payload of {len} bytes too large for a {width}x{height} rectangle"))
        })?;
        wire::read_into(&mut self.stream, len, &mut self.payload).await?;
        match self.zrle.decode(width, height, &self.payload, &mut self.pixels)? {
            Decoded::Solid(rgb) => lock(self.fb).fill(rect, rgb),
            Decoded::Pixels => lock(self.fb).blit(rect, &self.pixels),
        }
        Ok(())
    }

    /// DesktopSize / ExtendedDesktopSize: resize (clears to black) unless the size is unchanged.
    fn resize(&mut self, width: u16, height: u16) -> Result<(), Error> {
        let (width, height) = wire::check_size(width, height)?;
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        tracing::debug!(width, height, "desktop resized");
        lock(self.fb).resize(width, height);
        (self.width, self.height) = (width, height);
        self.shared.full_update.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// x = reason, y = status (non-zero: reply to a failed client request), w/h = new size.
    async fn extended_desktop_size(&mut self, header: RectHeader) -> Result<(), Error> {
        let mut head = [0u8; 4]; // number of screens, padding
        self.stream.read_exact(&mut head).await?;
        wire::skip(&mut self.stream, 16 * u64::from(head[0])).await?;
        if header.y == 0 {
            self.resize(header.w, header.h)
        } else {
            tracing::debug!(status = header.y, "ignoring failed ExtendedDesktopSize");
            Ok(())
        }
    }

    async fn desktop_name(&mut self) -> Result<(), Error> {
        let name = wire::read_string(&mut self.stream, wire::MAX_NAME, "desktop name").await?;
        tracing::debug!(%name, "desktop renamed");
        lock(self.fb).set_name(name);
        Ok(())
    }

    async fn colour_map_entries(&mut self) -> Result<(), Error> {
        let mut head = [0u8; 5]; // padding, first colour, number of colours
        self.stream.read_exact(&mut head).await?;
        let count = u16::from_be_bytes([head[3], head[4]]);
        wire::skip(&mut self.stream, 6 * u64::from(count)).await
    }

    async fn cut_text(&mut self) -> Result<(), Error> {
        let mut head = [0u8; 7]; // padding, length
        self.stream.read_exact(&mut head).await?;
        // Negative lengths announce the extended clipboard format; the size is the absolute value.
        let len = i32::from_be_bytes([head[3], head[4], head[5], head[6]]).unsigned_abs();
        if len > MAX_CUT_TEXT {
            return Err(Error::Protocol(format!("clipboard text too large ({len} bytes)")));
        }
        wire::skip(&mut self.stream, u64::from(len)).await
    }

    /// Drop scratch memory beyond what a full-screen update of the current size needs.
    fn release_excess(&mut self) {
        let keep = (self.width as usize * self.height as usize * 4).max(MIN_RETAINED);
        for buf in [&mut self.payload, &mut self.pixels] {
            if buf.capacity() > keep {
                buf.clear();
                buf.shrink_to(keep);
            }
        }
        self.zrle.release_excess(keep);
    }
}
