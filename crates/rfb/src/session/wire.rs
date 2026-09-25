//! Byte-level protocol helpers: client message encoders, bounded readers and limits.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Error;

pub(super) const ENCODING_RAW: i32 = 0;
pub(super) const ENCODING_COPY_RECT: i32 = 1;
pub(super) const ENCODING_ZRLE: i32 = 16;
pub(super) const ENCODING_DESKTOP_SIZE: i32 = -223;
pub(super) const ENCODING_QEMU_EXT_KEY: i32 = -258;
pub(super) const ENCODING_DESKTOP_NAME: i32 = -307;
pub(super) const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;

/// Encodings announced in SetEncodings, most preferred first.
const ENCODINGS: [i32; 7] = [
    ENCODING_ZRLE,
    ENCODING_COPY_RECT,
    ENCODING_RAW,
    ENCODING_DESKTOP_SIZE,
    ENCODING_EXTENDED_DESKTOP_SIZE,
    ENCODING_DESKTOP_NAME,
    ENCODING_QEMU_EXT_KEY,
];

/// Largest accepted desktop edge in pixels.
pub(super) const MAX_DIMENSION: u16 = 16384;
/// Largest accepted desktop area in pixels (e.g. 8192 x 8192, 16384 x 4096 or two 8K screens
/// side by side): 256 MiB of RGBA, which every rectangle inside the framebuffer then stays within.
pub(super) const MAX_PIXELS: u32 = 8192 * 8192;
/// Largest accepted desktop name in bytes.
pub(super) const MAX_NAME: u32 = 4096;

/// SetPixelFormat: 32 bpp, depth 24, little endian, true colour, 8-bit channels with red,
/// green and blue at bit 0, 8 and 16. A pixel on the wire is then the bytes R, G, B, X.
pub(super) fn set_pixel_format(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&[0, 0, 0, 0]); // message type, padding
    buf.extend_from_slice(&[32, 24, 0, 1]); // bits per pixel, depth, big endian, true colour
    for max in [255u16; 3] {
        buf.extend_from_slice(&max.to_be_bytes());
    }
    buf.extend_from_slice(&[0, 8, 16, 0, 0, 0]); // red/green/blue shift, padding
}

/// SetEncodings with [`ENCODINGS`].
pub(super) fn set_encodings(buf: &mut Vec<u8>) {
    const COUNT: u16 = ENCODINGS.len() as u16;
    buf.extend_from_slice(&[2, 0]);
    buf.extend_from_slice(&COUNT.to_be_bytes());
    for encoding in ENCODINGS {
        buf.extend_from_slice(&encoding.to_be_bytes());
    }
}

/// FramebufferUpdateRequest for the whole `width` x `height` screen.
pub(super) fn update_request(buf: &mut Vec<u8>, incremental: bool, width: u16, height: u16) {
    buf.extend_from_slice(&[3, u8::from(incremental), 0, 0, 0, 0]);
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
}

/// Standard RFB KeyEvent.
pub(super) fn key_event(buf: &mut Vec<u8>, keysym: u32, down: bool) {
    buf.extend_from_slice(&[4, u8::from(down), 0, 0]);
    buf.extend_from_slice(&keysym.to_be_bytes());
}

/// QEMU Extended Key Event (client message 255, sub-type 0) carrying keysym and scancode.
pub(super) fn qemu_key_event(buf: &mut Vec<u8>, keysym: u32, qnum: u32, down: bool) {
    buf.extend_from_slice(&[255, 0]);
    buf.extend_from_slice(&u16::from(down).to_be_bytes());
    buf.extend_from_slice(&keysym.to_be_bytes());
    buf.extend_from_slice(&qnum.to_be_bytes());
}

/// PointerEvent.
pub(super) fn pointer_event(buf: &mut Vec<u8>, x: u16, y: u16, buttons: u8) {
    buf.extend_from_slice(&[5, buttons]);
    buf.extend_from_slice(&x.to_be_bytes());
    buf.extend_from_slice(&y.to_be_bytes());
}

/// Write one complete message and flush it (websocket adapters buffer until a flush).
pub(super) async fn send<W: AsyncWrite + Unpin>(writer: &mut W, message: &[u8]) -> Result<(), Error> {
    writer.write_all(message).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a `u32` length-prefixed string (lossy UTF-8); longer than `max` bytes is a protocol error.
///
/// The text ends at the first NUL: C servers sometimes count the terminator in the length
/// (QEMU does for "Authentication failed"), and a NUL would show up as a box in the UI.
pub(super) async fn read_string<R: AsyncRead + Unpin>(reader: &mut R, max: u32, what: &str) -> Result<String, Error> {
    let len = reader.read_u32().await?;
    if len > max {
        return Err(Error::Protocol(format!("{what} too long ({len} bytes)")));
    }
    let mut bytes = Vec::new();
    read_into(reader, usize::try_from(len).unwrap_or(usize::MAX), &mut bytes).await?;
    let text = bytes.split(|&b| b == 0).next().unwrap_or_default();
    Ok(String::from_utf8_lossy(text).into_owned())
}

/// Replace the contents of `buf` with exactly the next `len` bytes.
///
/// The buffer grows as the data arrives (reusing its capacity), so a length announced by the
/// server never allocates or zero-fills memory up front.
pub(super) async fn read_into<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    buf.clear();
    let limit = u64::try_from(len).unwrap_or(u64::MAX);
    let read = (&mut *reader).take(limit).read_to_end(buf).await?;
    if read < len {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    Ok(())
}

/// Discard exactly `len` bytes.
pub(super) async fn skip<R: AsyncRead + Unpin>(reader: &mut R, len: u64) -> Result<(), Error> {
    let skipped = tokio::io::copy(&mut (&mut *reader).take(len), &mut tokio::io::sink()).await?;
    if skipped < len {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    Ok(())
}

/// Validate a desktop size announced by the server: edges of 1 to [`MAX_DIMENSION`] pixels and
/// at most [`MAX_PIXELS`] in total.
pub(super) fn check_size(width: u16, height: u16) -> Result<(u32, u32), Error> {
    let valid = 1..=MAX_DIMENSION;
    let (w, h) = (u32::from(width), u32::from(height));
    if valid.contains(&width) && valid.contains(&height) && w * h <= MAX_PIXELS {
        Ok((w, h))
    } else {
        Err(Error::Protocol(format!("unsupported desktop size {width}x{height}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages() {
        let mut buf = Vec::new();
        set_pixel_format(&mut buf);
        assert_eq!(buf, [0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);

        buf.clear();
        set_encodings(&mut buf);
        assert_eq!(&buf[..4], &[2, 0, 0, 7]);
        assert_eq!(&buf[4..8], &[0, 0, 0, 16]);
        assert_eq!(&buf[buf.len() - 4..], &(-258i32).to_be_bytes());
        assert_eq!(buf.len(), 4 + 7 * 4);

        buf.clear();
        update_request(&mut buf, true, 1280, 800);
        assert_eq!(buf, [3, 1, 0, 0, 0, 0, 5, 0, 3, 32]);

        buf.clear();
        key_event(&mut buf, 0xff0d, true);
        assert_eq!(buf, [4, 1, 0, 0, 0, 0, 0xff, 0x0d]);

        buf.clear();
        qemu_key_event(&mut buf, 0xff52, 0xc8, false);
        assert_eq!(buf, [255, 0, 0, 0, 0, 0, 0xff, 0x52, 0, 0, 0, 0xc8]);

        buf.clear();
        pointer_event(&mut buf, 300, 2, 0b101);
        assert_eq!(buf, [5, 5, 1, 44, 0, 2]);
    }

    #[test]
    fn sizes() {
        assert_eq!(check_size(1, 16384).unwrap(), (1, 16384));
        assert!(check_size(0, 10).is_err());
        assert!(check_size(10, 16385).is_err());
        // The area is capped too: 8192 x 8192 and 16384 x 4096 fit, 16384 x 16384 does not.
        assert_eq!(check_size(8192, 8192).unwrap(), (8192, 8192));
        assert_eq!(check_size(16384, 4096).unwrap(), (16384, 4096));
        assert!(check_size(16384, 4097).is_err());
        assert!(check_size(16384, 16384).is_err());
    }

    #[tokio::test]
    async fn bounded_reads() {
        let mut data: &[u8] = &[0, 0, 0, 2, b'h', 0xff, 9];
        assert_eq!(read_string(&mut data, 10, "x").await.unwrap(), "h\u{fffd}");
        assert_eq!(data, &[9]);

        let mut data: &[u8] = &[0, 0, 0, 20];
        assert!(matches!(read_string(&mut data, 10, "x").await, Err(Error::Protocol(_))));

        // QEMU counts the C string terminator: "Authentication failed\0" has length 22.
        let mut data: &[u8] = &[0, 0, 0, 5, b'o', b'k', 0, b'x', b'y', 9];
        assert_eq!(read_string(&mut data, 10, "x").await.unwrap(), "ok");
        assert_eq!(data, &[9]);

        let mut data: &[u8] = &[0, 0, 0, 3, b'a', b'b'];
        assert!(matches!(read_string(&mut data, 10, "x").await, Err(Error::Io(_))));

        let mut buf = vec![7; 100];
        let mut data: &[u8] = &[1, 2, 3, 4];
        read_into(&mut data, 3, &mut buf).await.unwrap();
        assert_eq!((buf.as_slice(), data), (&[1, 2, 3][..], &[4][..]));
        read_into(&mut data, 0, &mut buf).await.unwrap();
        assert!(buf.is_empty());
        // A length alone reserves nothing: the short read fails before any big allocation.
        let mut buf = Vec::new();
        assert!(matches!(read_into(&mut data, 256 << 20, &mut buf).await, Err(Error::Io(_))));
        assert!(buf.capacity() < 1 << 20, "{}", buf.capacity());

        let mut data: &[u8] = &[1, 2, 3, 4];
        skip(&mut data, 3).await.unwrap();
        assert_eq!(data, &[4]);
        assert!(matches!(skip(&mut data, 2).await, Err(Error::Io(_))));
    }
}
