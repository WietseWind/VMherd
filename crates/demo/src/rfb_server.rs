//! A small RFB 3.8 server for one console connection: security "None", a fixed 960x600 RGBX
//! screen, QEMU extended key events announced like QEMU does.
//!
//! Updates are pull based (only while the client has a request pending). Each one compares the
//! screen with what the client already has: a scroll becomes one CopyRect, then every changed
//! row span is sent as Raw pixels (the client applies rectangles in order).

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

use crate::cluster::Vm;
use crate::glyphs::{CELL_H, CELL_W, Glyphs, HEIGHT, WIDTH};
use crate::term::{COLS, Cell, Color, ROWS};

const ENCODING_RAW: i32 = 0;
const ENCODING_COPY_RECT: i32 = 1;
const ENCODING_QEMU_EXT_KEY: i32 = -258;
/// Longest client clipboard text we skip over.
const MAX_CUT_TEXT: u32 = 1 << 20;
/// Never equal to a real cell: rows the client has no valid pixels for.
const STALE: Cell = Cell { ch: '\0', fg: Color::Default, bold: false, inverse: false };

enum ClientMsg {
    Update { incremental: bool },
    Encodings(Vec<i32>),
    Key { keysym: u32, down: bool },
}

/// Serve one connection until the client leaves or the guest's power cycle ends (then the
/// stream is dropped: the client reads EOF).
pub(crate) async fn serve(stream: DuplexStream, vm: Arc<Vm>, glyphs: Arc<Glyphs>, epoch: u64) {
    let mut power = vm.subscribe();
    let off = async {
        while vm.alive(epoch) {
            if power.changed().await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = session(stream, &vm, &glyphs) => {}
        () = off => {}
    }
}

/// Aborts the task when dropped.
struct Guard(tokio::task::JoinHandle<()>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn session(stream: DuplexStream, vm: &Arc<Vm>, glyphs: &Glyphs) -> io::Result<()> {
    let (read_half, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    writer.write_all(b"RFB 003.008\n").await?;
    writer.flush().await?;
    let mut version = [0u8; 12];
    reader.read_exact(&mut version).await?;
    if &version[..4] != b"RFB " {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an RFB client"));
    }
    writer.write_all(&[1, 1]).await?; // one security type: None
    writer.flush().await?;
    if reader.read_u8().await? != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "client chose another security type"));
    }
    writer.write_all(&0u32.to_be_bytes()).await?; // SecurityResult OK
    writer.flush().await?;
    let _shared = reader.read_u8().await?;
    writer.write_all(&server_init(vm.spec.name)).await?;
    writer.flush().await?;

    // A separate reader, so a message is never abandoned half-read.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _reader = Guard(tokio::spawn(async move {
        let _ = read_messages(&mut reader, &tx).await;
    }));
    let mut changes = vm.subscribe();
    let mut updates = Updates::default();
    loop {
        if updates.pending {
            let screen = vm.lock().term.snapshot();
            if let Some(message) = updates.build(&screen, glyphs) {
                writer.write_all(&message).await?;
                writer.flush().await?;
            }
        }
        tokio::select! {
            msg = rx.recv() => match msg {
                None => return Ok(()),
                Some(ClientMsg::Update { incremental }) => {
                    updates.pending = true;
                    if !incremental {
                        updates.sent = None;
                    }
                }
                Some(ClientMsg::Encodings(list)) => updates.announce_keys = list.contains(&ENCODING_QEMU_EXT_KEY),
                Some(ClientMsg::Key { keysym, down }) => vm.key(keysym, down),
            },
            changed = changes.changed() => if changed.is_err() {
                return Ok(());
            },
        }
    }
}

fn server_init(name: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(24 + name.len());
    msg.extend((WIDTH as u16).to_be_bytes());
    msg.extend((HEIGHT as u16).to_be_bytes());
    // 32 bpp, depth 24, little endian, true colour, R/G/B at bits 0/8/16: bytes R, G, B, X
    msg.extend([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);
    msg.extend((name.len() as u32).to_be_bytes());
    msg.extend(name.as_bytes());
    msg
}

async fn skip<R: AsyncRead + Unpin>(reader: &mut R, len: u64) -> io::Result<()> {
    let skipped = tokio::io::copy(&mut (&mut *reader).take(len), &mut tokio::io::sink()).await?;
    if skipped < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

async fn read_messages<R: AsyncRead + Unpin>(r: &mut R, tx: &mpsc::UnboundedSender<ClientMsg>) -> io::Result<()> {
    loop {
        let msg = match r.read_u8().await? {
            0 => {
                // SetPixelFormat: the client always asks for the RGBX layout offered in ServerInit
                skip(r, 19).await?;
                continue;
            }
            2 => {
                r.read_u8().await?;
                let count = r.read_u16().await?;
                let mut list = Vec::with_capacity(usize::from(count));
                for _ in 0..count {
                    list.push(r.read_i32().await?);
                }
                ClientMsg::Encodings(list)
            }
            3 => {
                let incremental = r.read_u8().await? != 0;
                skip(r, 8).await?; // always the whole screen
                ClientMsg::Update { incremental }
            }
            4 => {
                let down = r.read_u8().await? != 0;
                skip(r, 2).await?;
                ClientMsg::Key { keysym: r.read_u32().await?, down }
            }
            5 => {
                skip(r, 5).await?; // pointer: the consoles are text only
                continue;
            }
            6 => {
                skip(r, 3).await?;
                let len = r.read_u32().await?;
                if len > MAX_CUT_TEXT {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                skip(r, u64::from(len)).await?;
                continue;
            }
            255 => {
                if r.read_u8().await? != 0 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let down = r.read_u16().await? != 0;
                let keysym = r.read_u32().await?;
                let _scancode = r.read_u32().await?;
                ClientMsg::Key { keysym, down }
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        if tx.send(msg).is_err() {
            return Ok(());
        }
    }
}

/// What the client has, and whether it asked for more.
#[derive(Default)]
struct Updates {
    pending: bool,
    /// The cells on the client's screen; `None` = send everything.
    sent: Option<Vec<Cell>>,
    announce_keys: bool,
    announced: bool,
}

impl Updates {
    /// A FramebufferUpdate bringing the client to `screen`, or `None` when it is up to date.
    fn build(&mut self, screen: &[Cell], glyphs: &Glyphs) -> Option<Vec<u8>> {
        let mut body = Vec::new();
        let mut count: u16 = 0;
        if self.announce_keys && !self.announced {
            self.announced = true;
            rect_header(&mut body, 0, 0, 0, 0, ENCODING_QEMU_EXT_KEY);
            count += 1;
        }
        match &self.sent {
            None => {
                raw_cells(&mut body, screen, glyphs, 0, 0, COLS, ROWS);
                count += 1;
            }
            Some(old) => {
                let mut old = old.clone();
                let n = scrolled(&old, screen);
                if n > 0 {
                    rect_header(&mut body, 0, 0, WIDTH, (ROWS - n) * CELL_H, ENCODING_COPY_RECT);
                    body.extend(0u16.to_be_bytes());
                    body.extend(((n * CELL_H) as u16).to_be_bytes());
                    count += 1;
                    old.copy_within(n * COLS.., 0);
                    old[(ROWS - n) * COLS..].fill(STALE);
                }
                for row in 0..ROWS {
                    let (now, was) = (&screen[row * COLS..][..COLS], &old[row * COLS..][..COLS]);
                    let Some(first) = (0..COLS).find(|&c| now[c] != was[c]) else { continue };
                    let last = (0..COLS).rev().find(|&c| now[c] != was[c]).unwrap_or(first);
                    raw_cells(&mut body, screen, glyphs, first, row, last + 1 - first, 1);
                    count += 1;
                }
            }
        }
        if count == 0 {
            return None;
        }
        self.sent = Some(screen.to_vec());
        self.pending = false;
        let mut message = Vec::with_capacity(4 + body.len());
        message.extend([0, 0]);
        message.extend(count.to_be_bytes());
        message.extend(body);
        Some(message)
    }
}

/// How many rows the screen scrolled up since `old` (0 if it did not).
fn scrolled(old: &[Cell], now: &[Cell]) -> usize {
    let blank = |r: usize| now[r * COLS..][..COLS].iter().all(|c| c.ch == ' ' && !c.inverse);
    let matching = |n: usize| {
        (0..ROWS - n).filter(|&r| !blank(r) && now[r * COLS..][..COLS] == old[(r + n) * COLS..][..COLS]).count()
    };
    let unmoved = matching(0);
    let best = (1..ROWS).map(|n| (n, matching(n))).max_by_key(|&(n, m)| (m, std::cmp::Reverse(n)));
    match best {
        Some((n, count)) if count > unmoved => n,
        _ => 0,
    }
}

fn rect_header(out: &mut Vec<u8>, x: usize, y: usize, w: usize, h: usize, encoding: i32) {
    for v in [x, y, w, h] {
        out.extend((v as u16).to_be_bytes());
    }
    out.extend(encoding.to_be_bytes());
}

/// A Raw rectangle of `cols` x `rows` cells starting at cell (`col`, `row`).
fn raw_cells(out: &mut Vec<u8>, screen: &[Cell], glyphs: &Glyphs, col: usize, row: usize, cols: usize, rows: usize) {
    let (w, h) = (cols * CELL_W, rows * CELL_H);
    rect_header(out, col * CELL_W, row * CELL_H, w, h, ENCODING_RAW);
    let start = out.len();
    out.resize(start + w * h * 4, 0);
    let pixels = &mut out[start..];
    for r in 0..rows {
        for c in 0..cols {
            glyphs.paint(screen[(row + r) * COLS + col + c], pixels, w * 4, c * CELL_W, r * CELL_H);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Term;

    #[test]
    fn updates_send_only_what_changed() {
        let glyphs = crate::glyphs::tests::glyphs();
        let mut t = Term::default();
        t.write("hello");
        let mut u = Updates { pending: true, announce_keys: true, ..Updates::default() };
        let first = u.build(&t.snapshot(), &glyphs).unwrap();
        assert_eq!(&first[..4], &[0, 0, 0, 2], "key announcement + the full screen");
        assert_eq!(first.len(), 4 + 12 + 12 + WIDTH * HEIGHT * 4);
        assert!(u.build(&t.snapshot(), &glyphs).is_none(), "nothing changed");

        t.write("!");
        let typed = u.build(&t.snapshot(), &glyphs).unwrap();
        // one span: the '!' and the cursor moving right
        assert_eq!(&typed[..4], &[0, 0, 0, 1]);
        assert_eq!(&typed[4..12], &[0, 60, 0, 0, 0, 24, 0, 24]);

        for i in 0..ROWS + 3 {
            t.write(&format!("\nline {i}"));
        }
        let before = t.snapshot();
        u.sent = Some(before.clone());
        t.write("\nmore");
        let scroll = u.build(&t.snapshot(), &glyphs).unwrap();
        assert_eq!(&scroll[12..16], &ENCODING_COPY_RECT.to_be_bytes(), "a scroll is a CopyRect");
        assert_eq!(scrolled(&before, &t.snapshot()), 1);
    }
}
