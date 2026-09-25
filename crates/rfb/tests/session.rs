//! Drives `rfb::run` against a scripted mock server over an in-memory duplex stream.

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use rfb::{ClientInput, Config, Error, Event, SharedFramebuffer};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);

type Rgb = [u8; 3];
const BLACK: Rgb = [0, 0, 0];
const RED: Rgb = [255, 0, 0];
const GREEN: Rgb = [0, 255, 0];
const BLUE: Rgb = [0, 0, 255];
const WHITE: Rgb = [255, 255, 255];
const GREY: Rgb = [128, 128, 128];

// ---------------------------------------------------------------------------------------------
// Harness

struct Client {
    fb: SharedFramebuffer,
    events: Arc<Mutex<Vec<Event>>>,
    input: mpsc::UnboundedSender<ClientInput>,
    task: JoinHandle<Result<(), Error>>,
}

impl Client {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn send(&self, input: ClientInput) {
        self.input.send(input).unwrap();
    }
}

/// Spawn a session (which also proves the `run` future is `Send`) and return the server end.
fn start(config: Config) -> (Client, DuplexStream) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let fb = SharedFramebuffer::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let notify: rfb::Notify = Arc::new(move |event| sink.lock().unwrap().push(event));
    let (input, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(rfb::run(client_io, config, fb.clone(), rx, notify));
    (Client { fb, events, input, task }, server_io)
}

async fn finish(task: JoinHandle<Result<(), Error>>) -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, task).await.expect("session did not end").expect("session panicked")
}

async fn write(server: &mut DuplexStream, bytes: &[u8]) {
    server.write_all(bytes).await.unwrap();
}

async fn read(server: &mut DuplexStream, len: usize) -> Vec<u8> {
    let mut got = vec![0; len];
    tokio::time::timeout(TIMEOUT, server.read_exact(&mut got))
        .await
        .expect("timed out waiting for the client")
        .unwrap();
    got
}

async fn expect(server: &mut DuplexStream, expected: &[u8]) {
    assert_eq!(read(server, expected.len()).await, expected);
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn server_init(width: u16, height: u16, name: &str) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend(width.to_be_bytes());
    msg.extend(height.to_be_bytes());
    // The server's native format (BGRX here); the client must override it.
    msg.extend([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    msg.extend((name.len() as u32).to_be_bytes());
    msg.extend(name.as_bytes());
    msg
}

fn update_request(incremental: bool, width: u16, height: u16) -> Vec<u8> {
    let mut msg = vec![3, u8::from(incremental), 0, 0, 0, 0];
    msg.extend(width.to_be_bytes());
    msg.extend(height.to_be_bytes());
    msg
}

/// SetPixelFormat, SetEncodings and the first (full) update request.
async fn expect_setup(server: &mut DuplexStream, width: u16, height: u16) {
    expect(server, &[0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]).await;
    expect(server, &[2, 0, 0, 7]).await;
    let bytes = read(server, 28).await;
    let encodings: Vec<i32> = bytes.as_chunks::<4>().0.iter().map(|c| i32::from_be_bytes(*c)).collect();
    assert_eq!(encodings, [16, 1, 0, -223, -308, -307, -258]);
    expect(server, &update_request(false, width, height)).await;
}

/// A 3.8 handshake with security "None", up to the client's first update request.
async fn handshake_none(server: &mut DuplexStream, width: u16, height: u16) {
    write(server, b"RFB 003.008\n").await;
    expect(server, b"RFB 003.008\n").await;
    write(server, &[1, 1]).await;
    expect(server, &[1]).await;
    write(server, &0u32.to_be_bytes()).await;
    expect(server, &[1]).await; // shared
    write(server, &server_init(width, height, "vm")).await;
    expect_setup(server, width, height).await;
}

fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut msg = Vec::new();
    for v in [x, y, w, h] {
        msg.extend(v.to_be_bytes());
    }
    msg.extend(encoding.to_be_bytes());
    msg
}

fn framebuffer_update(rects: &[Vec<u8>]) -> Vec<u8> {
    let mut msg = vec![0, 0];
    msg.extend((rects.len() as u16).to_be_bytes());
    for rect in rects {
        msg.extend(rect);
    }
    msg
}

fn zrle_rect(x: u16, y: u16, w: u16, h: u16, payload: &[u8]) -> Vec<u8> {
    let mut rect = rect_header(x, y, w, h, 16);
    rect.extend((payload.len() as u32).to_be_bytes());
    rect.extend(payload);
    rect
}

fn pixel(client: &Client, x: u32, y: u32) -> [u8; 4] {
    let fb = client.fb.lock().unwrap();
    let i = ((y * fb.width() + x) * 4) as usize;
    fb.pixels()[i..i + 4].try_into().unwrap()
}

// ---------------------------------------------------------------------------------------------
// A tiny, independent ZRLE encoder: every tile yields its bytes and its expected pixels.

struct Tile {
    bytes: Vec<u8>,
    pixels: Vec<Rgb>, // row-major, tile width x tile height
}

fn run_length(len: usize) -> Vec<u8> {
    let mut rest = len - 1;
    let mut out = Vec::new();
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
    out
}

fn solid(w: usize, h: usize, c: Rgb) -> Tile {
    let mut bytes = vec![1];
    bytes.extend(c);
    Tile { bytes, pixels: vec![c; w * h] }
}

fn raw(w: usize, h: usize, f: impl Fn(usize, usize) -> Rgb) -> Tile {
    let pixels: Vec<Rgb> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect();
    let mut bytes = vec![0];
    bytes.extend(pixels.iter().flatten());
    Tile { bytes, pixels }
}

fn packed(w: usize, h: usize, palette: &[Rgb], index: impl Fn(usize, usize) -> usize) -> Tile {
    let bits = match palette.len() {
        2 => 1,
        3 | 4 => 2,
        _ => 4,
    };
    let mut bytes = vec![palette.len() as u8];
    bytes.extend(palette.iter().flatten());
    let mut pixels = Vec::new();
    for y in 0..h {
        let mut row = vec![0u8; (w * bits).div_ceil(8)];
        for x in 0..w {
            let i = index(x, y);
            pixels.push(palette[i]);
            let bit = x * bits;
            row[bit / 8] |= (i as u8) << (8 - bits - bit % 8);
        }
        bytes.extend(row);
    }
    Tile { bytes, pixels }
}

fn plain_rle(runs: &[(Rgb, usize)]) -> Tile {
    let mut bytes = vec![128];
    let mut pixels = Vec::new();
    for &(c, len) in runs {
        bytes.extend(c);
        bytes.extend(run_length(len));
        pixels.extend(std::iter::repeat_n(c, len));
    }
    Tile { bytes, pixels }
}

fn palette_rle(palette: &[Rgb], runs: &[(usize, usize)]) -> Tile {
    let mut bytes = vec![128 + palette.len() as u8];
    bytes.extend(palette.iter().flatten());
    let mut pixels = Vec::new();
    for &(i, len) in runs {
        if len == 1 {
            bytes.push(i as u8);
        } else {
            bytes.push(i as u8 | 0x80);
            bytes.extend(run_length(len));
        }
        pixels.extend(std::iter::repeat_n(palette[i], len));
    }
    Tile { bytes, pixels }
}

/// Expected framebuffer contents.
struct Image {
    width: usize,
    pixels: Vec<Rgb>,
}

impl Image {
    fn new(width: usize, height: usize) -> Self {
        Self { width, pixels: vec![BLACK; width * height] }
    }

    fn paint(&mut self, x: usize, y: usize, w: usize, pixels: &[Rgb]) {
        for (i, &c) in pixels.iter().enumerate() {
            self.pixels[(y + i / w) * self.width + x + i % w] = c;
        }
    }

    fn assert_matches(&self, client: &Client) {
        let fb = client.fb.lock().unwrap();
        assert_eq!(fb.width() as usize, self.width);
        let got = fb.pixels().as_chunks::<4>().0;
        assert_eq!(got.len(), self.pixels.len());
        for (i, (got, want)) in got.iter().zip(&self.pixels).enumerate() {
            let (x, y) = (i % self.width, i / self.width);
            assert_eq!(*got, [want[0], want[1], want[2], 255], "pixel {x},{y}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn full_session_with_vnc_auth() {
    let (client, mut server) = start(Config::with_password("secret"));

    // Handshake: 3.8, VNC authentication.
    write(&mut server, b"RFB 003.008\n").await;
    expect(&mut server, b"RFB 003.008\n").await;
    write(&mut server, &[1, 2]).await;
    expect(&mut server, &[2]).await;
    let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
    write(&mut server, &challenge).await;
    // Computed independently: openssl enc -des-ecb -K cea6c64ea62e0000 -nopad.
    expect(&mut server, &hex("ee22539f33a5983ec12f9c2edbc995dd")).await;
    write(&mut server, &0u32.to_be_bytes()).await;
    expect(&mut server, &[1]).await; // shared
    write(&mut server, &server_init(4, 2, "t")).await;
    expect_setup(&mut server, 4, 2).await;
    assert_eq!(client.events(), [Event::Connected { width: 4, height: 2, name: "t".into() }]);
    {
        let fb = client.fb.lock().unwrap();
        assert_eq!((fb.width(), fb.height(), fb.name()), (4, 2, "t"));
    }

    // Update 1: QEMU ext keys, resize to 136x66, a raw rect and two ZRLE rects on one zlib stream.
    let (width, height) = (136u16, 66u16);
    let mut expected = Image::new(width.into(), height.into());

    let mut ext_size = rect_header(0, 0, width, height, -308);
    ext_size.extend([1, 0, 0, 0]);
    ext_size.extend([0, 0, 0, 0, 0, 0, 0, 0, 0, 136, 0, 66, 0, 0, 0, 0]);

    let raw_colour = |x: usize, y: usize| [10 * x as u8, 10 * y as u8 + 1, 200];
    let mut raw_rect = rect_header(0, 0, 6, 2, 0);
    for y in 0..2 {
        for x in 0..6 {
            raw_rect.extend(raw_colour(x, y));
            raw_rect.push(7); // padding byte: alpha must still come out as 255
            expected.paint(x, y, 1, &[raw_colour(x, y)]);
        }
    }

    // ZRLE rect 1 at (6,0), 130x66: tiles 64+64+2 wide, 64+2 high.
    let tiles = [
        (0, 0, 64, raw(64, 64, |x, y| [x as u8 * 4, y as u8 * 4, 0x55])),
        (64, 0, 64, plain_rle(&[(RED, 100), (GREEN, 300), (BLUE, 64 * 64 - 400)])),
        (128, 0, 2, solid(2, 64, WHITE)),
        (0, 64, 64, palette_rle(&[RED, GREEN, BLUE, GREY], &[(0, 1), (3, 1), (1, 60), (2, 1), (0, 65)])),
        (64, 64, 64, packed(64, 2, &[BLUE, WHITE], |x, y| (x / 3 + y) % 2)),
        (128, 64, 2, packed(2, 2, &[RED, GREEN, BLUE, WHITE, GREY], |x, y| [[4, 0], [1, 3]][y][x])),
    ];
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    for (tx, ty, tw, tile) in &tiles {
        assert_eq!(tile.pixels.len() % tw, 0);
        zlib.write_all(&tile.bytes).unwrap();
        expected.paint(6 + tx, *ty, *tw, &tile.pixels);
    }
    zlib.flush().unwrap(); // sync flush ends rect 1
    let zrle1 = zlib_take(&mut zlib);

    // ZRLE rect 2 at (0,2), 6x4 continues the same zlib stream (no header of its own).
    let tile = packed(6, 4, &[GREY, RED, GREEN], |x, y| (x + y) % 3);
    zlib.write_all(&tile.bytes).unwrap();
    zlib.flush().unwrap();
    let zrle2 = zlib_take(&mut zlib);
    expected.paint(0, 2, 6, &tile.pixels);

    let update = framebuffer_update(&[
        rect_header(0, 0, 0, 0, -258),
        ext_size,
        raw_rect,
        zrle_rect(6, 0, 130, 66, &zrle1),
        zrle_rect(0, 2, 6, 4, &zrle2),
    ]);
    write(&mut server, &update).await;
    // After a resize the next request asks for the whole new screen.
    expect(&mut server, &update_request(false, width, height)).await;
    expected.assert_matches(&client);
    assert_eq!(client.events().len(), 2);
    assert_eq!(client.events()[1], Event::Updated);

    // Input: ext keys are on now.
    client.send(ClientInput::Key { keysym: 0x61, qnum: 0x1e, down: true });
    expect(&mut server, &[255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x1e]).await;
    client.send(ClientInput::Key { keysym: 0xff0d, qnum: 0, down: false });
    expect(&mut server, &[4, 0, 0, 0, 0, 0, 0xff, 0x0d]).await;
    client.send(ClientInput::Key { keysym: 0, qnum: 0, down: true }); // dropped
    client.send(ClientInput::Pointer { x: 300, y: 2, buttons: 1 });
    expect(&mut server, &[5, 1, 1, 44, 0, 2]).await;
    client.send(ClientInput::RefreshFull);
    expect(&mut server, &update_request(false, width, height)).await;

    // Update 2: CopyRect, a new name and a same-size DesktopSize (must not clear the screen).
    let mut copy = rect_header(0, 10, 6, 2, 1);
    copy.extend([0, 0, 0, 0]);
    let mut name = rect_header(0, 0, 0, 0, -307);
    name.extend(7u32.to_be_bytes());
    name.extend(b"renamed");
    write(&mut server, &framebuffer_update(&[copy, name, rect_header(0, 0, width, height, -223)])).await;
    expect(&mut server, &update_request(true, width, height)).await;
    let top_left: Vec<Rgb> =
        (0..2).flat_map(|y| (0..6).map(move |x| (x, y))).map(|(x, y)| expected.pixels[y * 136 + x]).collect();
    expected.paint(0, 10, 6, &top_left);
    expected.assert_matches(&client);
    assert_eq!(client.fb.lock().unwrap().name(), "renamed");
    assert_eq!(pixel(&client, 5, 11), [50, 11, 200, 255]);

    // Bell, extended clipboard (negative length), colour map, then an empty update to sync.
    write(&mut server, &[2]).await;
    let mut cut = vec![3, 0, 0, 0];
    cut.extend((-5i32).to_be_bytes());
    cut.extend(b"hello");
    write(&mut server, &cut).await;
    let mut colours = vec![1, 0, 0, 0, 0, 2];
    colours.extend([9; 12]);
    write(&mut server, &colours).await;
    write(&mut server, &framebuffer_update(&[])).await;
    expect(&mut server, &update_request(true, width, height)).await;
    assert_eq!(
        client.events(),
        [
            Event::Connected { width: 4, height: 2, name: "t".into() },
            Event::Updated,
            Event::Updated,
            Event::Bell,
            Event::Updated,
        ]
    );

    // Dropping the input sender ends the session cleanly.
    let Client { input, task, .. } = client;
    drop(input);
    assert!(finish(task).await.is_ok());
}

fn zlib_take(zlib: &mut ZlibEncoder<Vec<u8>>) -> Vec<u8> {
    std::mem::take(zlib.get_mut())
}

#[tokio::test]
async fn version_3_3_with_no_security_and_server_close() {
    let (client, mut server) = start(Config { password: None, shared: false });
    write(&mut server, b"RFB 003.003\n").await;
    expect(&mut server, b"RFB 003.003\n").await;
    write(&mut server, &1u32.to_be_bytes()).await; // None, no SecurityResult in 3.3
    expect(&mut server, &[0]).await; // not shared
    write(&mut server, &server_init(8, 8, "vm")).await;
    expect_setup(&mut server, 8, 8).await;
    drop(server); // EOF between messages is a clean end
    let Client { task, events, .. } = client;
    assert!(finish(task).await.is_ok());
    assert_eq!(*events.lock().unwrap(), [Event::Connected { width: 8, height: 8, name: "vm".into() }]);
}

#[tokio::test]
async fn version_3_7_none_has_no_security_result() {
    let (client, mut server) = start(Config::with_password("unused"));
    write(&mut server, b"RFB 003.007\n").await;
    expect(&mut server, b"RFB 003.007\n").await;
    write(&mut server, &[3, 16, 2, 1]).await;
    expect(&mut server, &[1]).await;
    expect(&mut server, &[1]).await; // ClientInit right away
    write(&mut server, &server_init(2, 2, "")).await;
    expect_setup(&mut server, 2, 2).await;
    drop(client.input);
    assert!(finish(client.task).await.is_ok());
}

#[tokio::test]
async fn newer_versions_get_3_8() {
    let (client, mut server) = start(Config::default());
    write(&mut server, b"RFB 003.889\n").await;
    expect(&mut server, b"RFB 003.008\n").await;
    drop(server);
    assert!(matches!(finish(client.task).await, Err(Error::Io(_))));
}

async fn handshake_error(config: Config, server_bytes: &[&[u8]]) -> Error {
    let (client, mut server) = start(config);
    let mut sink = Vec::new();
    for bytes in server_bytes {
        write(&mut server, bytes).await;
        // Drain whatever the client answered so it never blocks.
        let _ = tokio::time::timeout(Duration::from_millis(50), server.read_buf(&mut sink)).await;
    }
    finish(client.task).await.expect_err("handshake should fail")
}

#[tokio::test]
async fn handshake_failures() {
    let err = handshake_error(Config::default(), &[b"RFB 003.005\n"]).await;
    assert!(matches!(err, Error::Version(ref v) if v == "RFB 003.005"), "{err:?}");

    let err = handshake_error(Config::default(), &[b"RFB 003.008\n", &[1, 2]]).await;
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");

    let err = handshake_error(Config::default(), &[b"RFB 003.008\n", &[2, 19, 16]]).await;
    assert!(matches!(err, Error::NoSecurity(ref v) if v == &[19, 16]), "{err:?}");

    let mut refused = vec![0, 0, 0, 0, 4];
    refused.extend(b"busy");
    let err = handshake_error(Config::default(), &[b"RFB 003.008\n", &refused]).await;
    assert!(matches!(err, Error::AuthFailed(ref r) if r == "busy"), "{err:?}");

    let mut failed = vec![0, 0, 0, 1, 0, 0, 0, 12];
    failed.extend(b"bad password");
    let err = handshake_error(Config::with_password("nope"), &[b"RFB 003.008\n", &[1, 2], &[7; 16], &failed]).await;
    assert!(matches!(err, Error::AuthFailed(ref r) if r == "bad password"), "{err:?}");

    // QEMU (expired Proxmox ticket or wrong password) counts the C string terminator.
    const QEMU_ERR: &[u8] = b"Authentication failed\0";
    let mut failed = vec![0, 0, 0, 1];
    failed.extend((QEMU_ERR.len() as u32).to_be_bytes());
    failed.extend(QEMU_ERR);
    let err = handshake_error(Config::with_password("expired"), &[b"RFB 003.008\n", &[1, 2], &[7; 16], &failed]).await;
    assert!(matches!(err, Error::AuthFailed(ref r) if r == "Authentication failed"), "{err:?}");
    assert_eq!(err.to_string(), "authentication failed: Authentication failed");

    // A reason that is only a terminator and blanks falls back to the generic text.
    let err = handshake_error(Config::default(), &[b"RFB 003.008\n", &[0, 0, 0, 0, 3, b' ', 0, b'\n']]).await;
    assert!(matches!(err, Error::AuthFailed(ref r) if r == "authentication failed"), "{err:?}");

    // 3.3 sends no reason after a failed SecurityResult.
    let err =
        handshake_error(Config::with_password("nope"), &[b"RFB 003.003\n", &[0, 0, 0, 2], &[7; 16], &[0, 0, 0, 1]])
            .await;
    assert!(matches!(err, Error::AuthFailed(_)), "{err:?}");

    let mut huge_name = server_init(4, 4, "");
    huge_name[20..24].copy_from_slice(&5000u32.to_be_bytes());
    let err = handshake_error(Config::default(), &[b"RFB 003.008\n", &[1, 1], &[0, 0, 0, 0], &huge_name]).await;
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");

    let err =
        handshake_error(Config::default(), &[b"RFB 003.008\n", &[1, 1], &[0, 0, 0, 0], &server_init(0, 4, "")]).await;
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");
}

/// Handshake, send `messages`, and return the session's error.
async fn session_error(messages: &[u8]) -> Error {
    let (client, mut server) = start(Config::default());
    handshake_none(&mut server, 16, 16).await;
    write(&mut server, messages).await;
    let result = finish(client.task).await;
    drop(server);
    result.expect_err("session should fail")
}

#[tokio::test]
async fn server_closing_mid_message_is_an_error() {
    let (client, mut server) = start(Config::default());
    handshake_none(&mut server, 16, 16).await;
    write(&mut server, &[0, 0, 0, 1, 0, 0, 0]).await; // FBU with half a rectangle header
    drop(server);
    let err = finish(client.task).await.expect_err("truncated message");
    assert!(matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof), "{err:?}");
}

#[tokio::test]
async fn malformed_zrle_is_an_error() {
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib.write_all(&[17, 0, 0, 0]).unwrap(); // unused sub-encoding
    zlib.flush().unwrap();
    let err = session_error(&framebuffer_update(&[zrle_rect(0, 0, 4, 4, &zlib_take(&mut zlib))])).await;
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");

    let err = session_error(&framebuffer_update(&[zrle_rect(0, 0, 4, 4, &[1, 2, 3, 4, 5])])).await;
    assert!(matches!(err, Error::Zlib(_)), "{err:?}");

    let mut oversized = rect_header(0, 0, 4, 4, 16);
    oversized.extend((65u32 << 20).to_be_bytes());
    let err = session_error(&framebuffer_update(&[oversized])).await;
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");

    // The payload cap follows the rectangle size: 1 MiB can never be a valid 4x4 rectangle, so
    // the client fails at once instead of buffering it (the server never sends the bytes).
    let mut oversized = rect_header(0, 0, 4, 4, 16);
    oversized.extend((1u32 << 20).to_be_bytes());
    let err = session_error(&framebuffer_update(&[oversized])).await;
    assert!(matches!(err, Error::Protocol(ref m) if m.contains("4x4")), "{err:?}");
}

#[tokio::test]
async fn rectangles_must_lie_inside_the_framebuffer() {
    // The session is 16x16. None of these may allocate anything: the client must fail on the
    // header alone (the server never sends the pixel data).
    let beyond = [
        rect_header(0, 0, 17, 1, 0),       // Raw, one column too wide
        rect_header(0, 15, 1, 2, 0),       // Raw, one row too tall
        rect_header(0, 0, 8192, 8192, 0),  // Raw, 256 MiB announced in 16 bytes
        rect_header(65535, 0, 2, 1, 0),    // Raw, beyond the 16-bit coordinate space
        rect_header(10, 10, 8, 8, 1),      // CopyRect
        rect_header(15, 15, 2, 1, 16),     // ZRLE
        rect_header(0, 0, 8192, 8192, 16), // ZRLE, would allow a 256 MiB zlib bomb
    ];
    for rect in beyond {
        let err = session_error(&framebuffer_update(std::slice::from_ref(&rect))).await;
        assert!(matches!(err, Error::Protocol(ref m) if m.contains("16x16 framebuffer")), "{rect:?}: {err:?}");
    }

    // Too large a desktop is refused as well (16384 x 16384 would be 1 GiB).
    let err = session_error(&framebuffer_update(&[rect_header(0, 0, 16384, 16384, -223)])).await;
    assert!(matches!(err, Error::Protocol(_)), "{err:?}");

    // The bound follows resizes inside one update: grow, then paint the new area; then shrink,
    // after which the same rectangle is out of bounds.
    let (client, mut server) = start(Config::default());
    handshake_none(&mut server, 16, 16).await;
    let mut paint = rect_header(28, 15, 4, 1, 0);
    paint.extend([200u8; 16]);
    write(&mut server, &framebuffer_update(&[rect_header(0, 0, 32, 16, -223), paint.clone()])).await;
    expect(&mut server, &update_request(false, 32, 16)).await;
    assert_eq!(pixel(&client, 31, 15), [200, 200, 200, 255]);
    write(&mut server, &framebuffer_update(&[rect_header(0, 0, 16, 16, -223), paint])).await;
    let err = finish(client.task).await.expect_err("rect beyond the shrunk framebuffer");
    assert!(matches!(err, Error::Protocol(ref m) if m.contains("16x16 framebuffer")), "{err:?}");
}

#[tokio::test]
async fn protocol_violations_are_errors() {
    let err = session_error(&[9]).await;
    assert!(matches!(err, Error::Protocol(_)), "unknown message: {err:?}");

    let err = session_error(&framebuffer_update(&[rect_header(0, 0, 1, 1, 7)])).await;
    assert!(matches!(err, Error::Protocol(_)), "unknown encoding: {err:?}");

    let err = session_error(&framebuffer_update(&[rect_header(0, 0, 65535, 65535, 0)])).await;
    assert!(matches!(err, Error::Protocol(_)), "huge raw rect: {err:?}");

    let err = session_error(&framebuffer_update(&[rect_header(0, 0, 0, 16, -223)])).await;
    assert!(matches!(err, Error::Protocol(_)), "zero desktop size: {err:?}");

    let mut cut = vec![3, 0, 0, 0];
    cut.extend((17u32 << 20).to_be_bytes());
    let err = session_error(&cut).await;
    assert!(matches!(err, Error::Protocol(_)), "huge clipboard: {err:?}");
}

#[tokio::test]
async fn keys_without_ext_key_support_use_keysyms() {
    let (client, mut server) = start(Config::default());
    handshake_none(&mut server, 16, 16).await;
    client.send(ClientInput::Key { keysym: 0x61, qnum: 0x1e, down: true });
    expect(&mut server, &[4, 1, 0, 0, 0, 0, 0, 0x61]).await;
    // A failed ExtendedDesktopSize reply (status != 0) must not resize.
    let mut ext = rect_header(1, 3, 800, 600, -308);
    ext.extend([0, 0, 0, 0]);
    write(&mut server, &framebuffer_update(&[ext])).await;
    expect(&mut server, &update_request(true, 16, 16)).await;
    assert_eq!(client.fb.lock().unwrap().width(), 16);
    drop(client.input);
    assert!(finish(client.task).await.is_ok());
}
