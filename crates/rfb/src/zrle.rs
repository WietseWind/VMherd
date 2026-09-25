//! ZRLE decoding (RFC 6143 7.7.6) for 32 bpp / depth 24 true colour (3-byte CPIXELs, R G B).
//!
//! All ZRLE rectangles of a session share one zlib stream, so the decoder is created once per
//! session. [`ZrleDecoder::decode`] inflates one rectangle and paints its 64x64 tiles into a
//! caller-owned RGBA buffer; the caller then blits that buffer into the framebuffer, keeping the
//! framebuffer lock out of the decoding work. Rectangles made only of solid tiles of one colour
//! (blank screen areas, very common on consoles) are reported as [`Decoded::Solid`] instead, so
//! the caller can fill them without touching the buffer.

use flate2::{Decompress, FlushDecompress, Status};

use crate::Error;

/// Tile edge length in pixels.
const TILE: usize = 64;
/// Largest palette (palette RLE sub-encoding 255).
const MAX_PALETTE: usize = 127;
/// Worst-case bytes per tile besides the pixels: sub-encoding byte plus a full palette.
const TILE_OVERHEAD: usize = 1 + MAX_PALETTE * 3;
/// Cap on the decoded RGBA bytes of one rectangle (8192 x 8192 pixels). The session keeps the
/// framebuffer, and so every rectangle inside it, within this.
pub(crate) const MAX_DECODED: usize = 256 << 20;
/// Absolute cap on the inflated bytes of one rectangle (guards against zlib bombs): the pixel
/// cap plus the tile overhead of any rectangle within it whose edges are at most 16384 pixels
/// (under 17k tiles, about 6.1 MiB), so it never rejects valid data of such a rectangle.
const MAX_INFLATED: usize = MAX_DECODED + (8 << 20);
/// Compressed bytes of one rectangle beyond zlib's expansion bound: the stream header (first
/// rectangle only) and the sync-flush marker, with a wide margin.
const PAYLOAD_SLACK: usize = 1 << 10;
/// Initial growth step of the inflate buffer.
const MIN_GROWTH: usize = 64 << 10;

/// Result of [`ZrleDecoder::decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decoded {
    /// The whole rectangle is this R, G, B colour; the output buffer was not written.
    Solid([u8; 3]),
    /// The output buffer holds the rectangle's RGBA pixels.
    Pixels,
}

/// Stateful ZRLE decoder: owns the session-wide zlib stream and a reusable inflate buffer.
pub(crate) struct ZrleDecoder {
    zlib: Decompress,
    /// Inflate scratch space. Its whole length is initialised and only ever grows (until
    /// [`ZrleDecoder::release_excess`]), so small rectangles never pay for re-zeroing it.
    scratch: Vec<u8>,
}

impl Default for ZrleDecoder {
    fn default() -> Self {
        Self { zlib: Decompress::new(true), scratch: Vec::new() }
    }
}

impl ZrleDecoder {
    /// Decode one ZRLE rectangle of `width` x `height` pixels from its zlib `payload`.
    ///
    /// Unless the rectangle is one solid colour ([`Decoded::Solid`]), `out` is resized to
    /// `width * height * 4` bytes and receives the RGBA pixels (alpha 255).
    /// Malformed data yields [`Error::Protocol`] or [`Error::Zlib`]; it never panics.
    pub(crate) fn decode(
        &mut self,
        width: usize,
        height: usize,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<Decoded, Error> {
        let decoded = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&bytes| bytes <= MAX_DECODED)
            .ok_or_else(|| protocol("ZRLE rectangle too large"))?;
        let inflated = self.inflate(payload, max_inflated_len(width, height))?;
        let data = &self.scratch[..inflated];
        if let Some(colour) = solid_colour(data, tile_count(width, height)) {
            return Ok(Decoded::Solid(colour));
        }
        // No need to clear: a successful decode writes every pixel of the rectangle.
        out.resize(decoded, 0);
        decode_tiles(data, width, height, out)?;
        Ok(Decoded::Pixels)
    }

    /// Release the inflate buffer's memory beyond `keep` bytes.
    pub(crate) fn release_excess(&mut self, keep: usize) {
        if self.scratch.len() > keep {
            self.scratch.truncate(keep);
            self.scratch.shrink_to(keep);
        }
    }

    /// Inflate all of `input` (one rectangle's data, ending on a sync flush) into the scratch
    /// buffer and return the number of bytes produced; fails beyond `limit` bytes.
    fn inflate(&mut self, mut input: &[u8], limit: usize) -> Result<usize, Error> {
        let mut filled = 0;
        loop {
            if filled == self.scratch.len() {
                // One byte beyond the limit is enough to detect oversized output.
                let room = limit.saturating_sub(filled).max(1);
                let grow = filled.max(input.len().saturating_mul(4)).max(MIN_GROWTH).min(room);
                self.scratch.resize(filled + grow, 0);
            }
            let (in_before, out_before) = (self.zlib.total_in(), self.zlib.total_out());
            let status = self
                .zlib
                .decompress(input, &mut self.scratch[filled..], FlushDecompress::Sync)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            // The counters grow by at most the slice lengths; clamp anyway so a bug there
            // cannot turn into an out-of-bounds slice.
            let consumed = progress(self.zlib.total_in() - in_before, input.len());
            let produced = progress(self.zlib.total_out() - out_before, self.scratch.len() - filled);
            input = &input[consumed..];
            filled += produced;
            if filled > limit {
                return Err(too_big());
            }
            let out_full = filled == self.scratch.len();
            match status {
                Status::StreamEnd if !input.is_empty() => {
                    return Err(Error::Zlib("zlib stream ended inside a rectangle".into()));
                }
                Status::StreamEnd => return Ok(filled),
                Status::Ok | Status::BufError => {}
            }
            if input.is_empty() && !out_full {
                return Ok(filled);
            }
            if consumed == 0 && produced == 0 && !out_full {
                return Err(Error::Zlib("zlib stream made no progress".into()));
            }
        }
    }
}

fn progress(delta: u64, max: usize) -> usize {
    usize::try_from(delta).map_or(max, |n| n.min(max))
}

/// Upper bound of the inflated size of a valid `width` x `height` rectangle: at most 4 bytes
/// per pixel (plain RLE runs of one pixel) plus a sub-encoding byte and palette per tile.
fn max_inflated_len(width: usize, height: usize) -> usize {
    let tiles = tile_count(width, height);
    width.saturating_mul(height).saturating_mul(4).saturating_add(tiles.saturating_mul(TILE_OVERHEAD)).min(MAX_INFLATED)
}

/// Upper bound of the compressed payload of a valid `width` x `height` rectangle: zlib's
/// conservative `deflateBound()` (valid for any compression settings, about 14% expansion)
/// applied to [`max_inflated_len`], plus [`PAYLOAD_SLACK`].
pub(crate) fn max_payload_len(width: usize, height: usize) -> usize {
    let n = max_inflated_len(width, height);
    n.saturating_add(n.div_ceil(8)).saturating_add(n.div_ceil(64)).saturating_add(5 + PAYLOAD_SLACK)
}

fn tile_count(width: usize, height: usize) -> usize {
    width.div_ceil(TILE).saturating_mul(height.div_ceil(TILE))
}

/// The colour of a rectangle whose `tiles` tiles are all solid (sub-encoding 1) and identical.
fn solid_colour(data: &[u8], tiles: usize) -> Option<[u8; 3]> {
    if tiles == 0 || data.len() != tiles.checked_mul(4)? {
        return None;
    }
    let (tiles, _) = data.as_chunks::<4>();
    let first = tiles[0];
    (first[0] == 1 && tiles.iter().all(|t| *t == first)).then_some([first[1], first[2], first[3]])
}

fn too_big() -> Error {
    Error::Protocol("ZRLE rectangle inflates beyond its size".into())
}

fn protocol(msg: &str) -> Error {
    Error::Protocol(msg.to_owned())
}

/// Position of one tile inside the rectangle's RGBA buffer.
#[derive(Clone, Copy, Debug)]
struct Tile {
    /// Byte offset of the tile's top-left pixel.
    base: usize,
    /// Bytes per row of the whole rectangle.
    stride: usize,
    /// Tile width in pixels.
    width: usize,
    /// Tile height in pixels.
    height: usize,
}

impl Tile {
    fn pixels(&self) -> usize {
        self.width * self.height
    }

    /// The tile's rows, each `width` RGBA pixels.
    fn rows<'a>(&self, out: &'a mut [u8]) -> impl Iterator<Item = &'a mut [[u8; 4]]> {
        let row_bytes = self.width * 4;
        out[self.base..]
            .chunks_mut(self.stride)
            .take(self.height)
            .map(move |row| row[..row_bytes].as_chunks_mut::<4>().0)
    }
}

/// Paint all tiles of a `width` x `height` rectangle (left to right, top to bottom).
fn decode_tiles(data: &[u8], width: usize, height: usize, out: &mut [u8]) -> Result<(), Error> {
    let mut reader = Reader { data };
    let stride = width * 4;
    for ty in (0..height).step_by(TILE) {
        for tx in (0..width).step_by(TILE) {
            let tile =
                Tile { base: ty * stride + tx * 4, stride, width: TILE.min(width - tx), height: TILE.min(height - ty) };
            decode_tile(&mut reader, out, tile)?;
        }
    }
    if !reader.data.is_empty() {
        tracing::debug!(extra = reader.data.len(), "ignoring trailing ZRLE data");
    }
    Ok(())
}

fn decode_tile(r: &mut Reader<'_>, out: &mut [u8], tile: Tile) -> Result<(), Error> {
    match r.u8()? {
        0 => raw_tile(r, out, tile),
        1 => {
            let colour = r.cpixel()?;
            for row in tile.rows(out) {
                row.fill(colour);
            }
            Ok(())
        }
        n @ 2..=16 => packed_palette_tile(r, out, tile, usize::from(n)),
        128 => plain_rle_tile(r, out, tile),
        n @ 130..=255 => palette_rle_tile(r, out, tile, usize::from(n - 128)),
        n => Err(Error::Protocol(format!("invalid ZRLE sub-encoding {n}"))),
    }
}

fn raw_tile(r: &mut Reader<'_>, out: &mut [u8], tile: Tile) -> Result<(), Error> {
    let data = r.take(tile.pixels() * 3)?;
    for (row, src) in tile.rows(out).zip(data.chunks_exact(tile.width * 3)) {
        for (px, c) in row.iter_mut().zip(src.as_chunks::<3>().0) {
            *px = [c[0], c[1], c[2], 255];
        }
    }
    Ok(())
}

fn packed_palette_tile(r: &mut Reader<'_>, out: &mut [u8], tile: Tile, size: usize) -> Result<(), Error> {
    let palette = Palette::read(r, size)?;
    let bits = match size {
        2 => 1,
        3 | 4 => 2,
        _ => 4,
    };
    let mask = (1u8 << bits) - 1;
    let row_bytes = (tile.width * bits).div_ceil(8);
    let data = r.take(row_bytes * tile.height)?;
    for (row, src) in tile.rows(out).zip(data.chunks_exact(row_bytes)) {
        for (x, px) in row.iter_mut().enumerate() {
            let bit = x * bits;
            let shift = 8 - bits - bit % 8;
            *px = palette.get((src[bit / 8] >> shift) & mask)?;
        }
    }
    Ok(())
}

fn plain_rle_tile(r: &mut Reader<'_>, out: &mut [u8], tile: Tile) -> Result<(), Error> {
    let mut runs = RunWriter::new(out, tile);
    while runs.remaining > 0 {
        let colour = r.cpixel()?;
        let len = r.run_length(runs.remaining)?;
        runs.put(colour, len)?;
    }
    Ok(())
}

fn palette_rle_tile(r: &mut Reader<'_>, out: &mut [u8], tile: Tile, size: usize) -> Result<(), Error> {
    let palette = Palette::read(r, size)?;
    let mut runs = RunWriter::new(out, tile);
    while runs.remaining > 0 {
        let byte = r.u8()?;
        let colour = palette.get(byte & 0x7f)?;
        let len = if byte & 0x80 == 0 { 1 } else { r.run_length(runs.remaining)? };
        runs.put(colour, len)?;
    }
    Ok(())
}

/// Up to 127 RGBA colours of a palette tile.
struct Palette {
    colours: [[u8; 4]; MAX_PALETTE],
    len: usize,
}

impl Palette {
    fn read(r: &mut Reader<'_>, len: usize) -> Result<Self, Error> {
        let mut colours = [[0, 0, 0, 255]; MAX_PALETTE];
        let data = r.take(len * 3)?;
        for (colour, c) in colours.iter_mut().zip(data.as_chunks::<3>().0) {
            *colour = [c[0], c[1], c[2], 255];
        }
        Ok(Self { colours, len })
    }

    fn get(&self, index: u8) -> Result<[u8; 4], Error> {
        let index = usize::from(index);
        if index < self.len { Ok(self.colours[index]) } else { Err(protocol("ZRLE palette index out of range")) }
    }
}

/// Writes runs of pixels into a tile in row-major order.
struct RunWriter<'a> {
    out: &'a mut [u8],
    tile: Tile,
    /// Byte offset of the current row's first pixel.
    row_start: usize,
    /// Current column inside the tile.
    col: usize,
    /// Pixels of the tile still to be written.
    remaining: usize,
}

impl<'a> RunWriter<'a> {
    fn new(out: &'a mut [u8], tile: Tile) -> Self {
        Self { out, tile, row_start: tile.base, col: 0, remaining: tile.pixels() }
    }

    fn put(&mut self, colour: [u8; 4], mut len: usize) -> Result<(), Error> {
        if len > self.remaining {
            return Err(protocol("ZRLE run exceeds the tile"));
        }
        self.remaining -= len;
        while len > 0 {
            let n = len.min(self.tile.width - self.col);
            let start = self.row_start + self.col * 4;
            self.out[start..start + n * 4].as_chunks_mut::<4>().0.fill(colour);
            self.col += n;
            len -= n;
            if self.col == self.tile.width {
                self.col = 0;
                self.row_start += self.tile.stride;
            }
        }
        Ok(())
    }
}

/// Cursor over the inflated bytes of one rectangle.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.data.len() {
            return Err(truncated());
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let (&byte, rest) = self.data.split_first().ok_or_else(truncated)?;
        self.data = rest;
        Ok(byte)
    }

    fn cpixel(&mut self) -> Result<[u8; 4], Error> {
        let (c, rest) = self.data.split_first_chunk::<3>().ok_or_else(truncated)?;
        self.data = rest;
        Ok([c[0], c[1], c[2], 255])
    }

    /// RLE run length: 1 + the sum of bytes up to and including the first one that is not 255.
    /// Fails as soon as the run exceeds `max` pixels.
    fn run_length(&mut self, max: usize) -> Result<usize, Error> {
        let mut len = 1usize;
        loop {
            let byte = self.u8()?;
            len += usize::from(byte);
            if len > max {
                return Err(protocol("ZRLE run exceeds the tile"));
            }
            if byte != 255 {
                return Ok(len);
            }
        }
    }
}

fn truncated() -> Error {
    protocol("ZRLE data truncated")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    use super::*;

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const WHITE: [u8; 3] = [255, 255, 255];
    const GREY: [u8; 3] = [128, 128, 128];

    /// Compresses rectangles the way a server does: one stream, a sync flush after each.
    struct Encoder(ZlibEncoder<Vec<u8>>);

    impl Encoder {
        fn new() -> Self {
            Self(ZlibEncoder::new(Vec::new(), Compression::default()))
        }

        fn rect(&mut self, tiles: &[u8]) -> Vec<u8> {
            self.0.write_all(tiles).unwrap();
            self.0.flush().unwrap();
            std::mem::take(self.0.get_mut())
        }
    }

    fn rgba(c: [u8; 3]) -> [u8; 4] {
        [c[0], c[1], c[2], 255]
    }

    fn pixel(out: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
        let i = (y * width + x) * 4;
        assert_eq!(out[i + 3], 255, "alpha at {x},{y}");
        [out[i], out[i + 1], out[i + 2]]
    }

    /// Decode with `decoder` and return the RGBA pixels, expanding a solid result.
    fn pixels(decoder: &mut ZrleDecoder, width: usize, height: usize, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let out = match decoder.decode(width, height, payload, &mut out)? {
            Decoded::Solid(c) => rgba(c).repeat(width * height),
            Decoded::Pixels => out,
        };
        assert_eq!(out.len(), width * height * 4);
        Ok(out)
    }

    fn decode(width: usize, height: usize, tiles: &[u8]) -> Result<Vec<u8>, Error> {
        pixels(&mut ZrleDecoder::default(), width, height, &Encoder::new().rect(tiles))
    }

    #[test]
    fn solid_and_raw_tiles_across_tile_columns() {
        // 66 x 2: a 64-pixel solid tile followed by a 2-pixel raw tile.
        let mut tiles = vec![1];
        tiles.extend(RED);
        tiles.push(0);
        for c in [GREEN, BLUE, WHITE, GREY] {
            tiles.extend(c);
        }
        let out = decode(66, 2, &tiles).unwrap();
        assert_eq!(pixel(&out, 66, 0, 0), RED);
        assert_eq!(pixel(&out, 66, 63, 1), RED);
        assert_eq!(pixel(&out, 66, 64, 0), GREEN);
        assert_eq!(pixel(&out, 66, 65, 0), BLUE);
        assert_eq!(pixel(&out, 66, 64, 1), WHITE);
        assert_eq!(pixel(&out, 66, 65, 1), GREY);
    }

    #[test]
    fn tiles_wrap_to_a_second_tile_row() {
        // 1 x 65: two solid tiles stacked vertically.
        let mut tiles = vec![1];
        tiles.extend(RED);
        tiles.push(1);
        tiles.extend(BLUE);
        let out = decode(1, 65, &tiles).unwrap();
        assert_eq!(pixel(&out, 1, 0, 63), RED);
        assert_eq!(pixel(&out, 1, 0, 64), BLUE);
    }

    #[test]
    fn uniform_solid_rectangles_skip_the_pixel_buffer() {
        let mut decoder = ZrleDecoder::default();
        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        // 130 x 70 = 3 x 2 tiles, all solid grey.
        let tiles = [1, 128, 128, 128].repeat(6);
        let result = decoder.decode(130, 70, &encoder.rect(&tiles), &mut out).unwrap();
        assert_eq!(result, Decoded::Solid(GREY));
        assert!(out.is_empty());
        // Solid tiles of different colours are painted normally.
        let mut tiles = [1, 128, 128, 128].repeat(5);
        tiles.extend([1, 1, 2, 3]);
        let result = decoder.decode(130, 70, &encoder.rect(&tiles), &mut out).unwrap();
        assert_eq!(result, Decoded::Pixels);
        assert_eq!(pixel(&out, 130, 0, 0), GREY);
        assert_eq!(pixel(&out, 130, 129, 69), [1, 2, 3]);
    }

    #[test]
    fn packed_palettes_of_every_index_width() {
        // 1 bit: 3 x 2 pixels, indices 0 1 0 / 1 1 0 -> rows 0b010_00000, 0b110_00000.
        let mut tiles = vec![2];
        tiles.extend(RED);
        tiles.extend(BLUE);
        tiles.extend([0b0100_0000, 0b1100_0000]);
        let out = decode(3, 2, &tiles).unwrap();
        let expected = [[RED, BLUE, RED], [BLUE, BLUE, RED]];
        for (y, row) in expected.iter().enumerate() {
            for (x, &c) in row.iter().enumerate() {
                assert_eq!(pixel(&out, 3, x, y), c, "1-bit {x},{y}");
            }
        }

        // 2 bits: 5 x 1 pixels, indices 2 0 1 2 1 -> 0b10_00_01_10, 0b01_000000.
        let mut tiles = vec![3];
        for c in [RED, GREEN, BLUE] {
            tiles.extend(c);
        }
        tiles.extend([0b1000_0110, 0b0100_0000]);
        let out = decode(5, 1, &tiles).unwrap();
        for (x, c) in [BLUE, RED, GREEN, BLUE, GREEN].into_iter().enumerate() {
            assert_eq!(pixel(&out, 5, x, 0), c, "2-bit {x}");
        }

        // 4 bits: 3 x 1 pixels, indices 4 0 3 -> 0x40, 0x30.
        let mut tiles = vec![5];
        for c in [RED, GREEN, BLUE, WHITE, GREY] {
            tiles.extend(c);
        }
        tiles.extend([0x40, 0x30]);
        let out = decode(3, 1, &tiles).unwrap();
        for (x, c) in [GREY, RED, WHITE].into_iter().enumerate() {
            assert_eq!(pixel(&out, 3, x, 0), c, "4-bit {x}");
        }
    }

    #[test]
    fn plain_rle_runs_span_rows() {
        // 64 x 5 = 320 pixels: 70 red (crosses into row 1), 250 blue.
        let mut tiles = vec![128];
        tiles.extend(RED);
        tiles.push(69);
        tiles.extend(BLUE);
        tiles.extend([249]);
        let out = decode(64, 5, &tiles).unwrap();
        assert_eq!(pixel(&out, 64, 63, 0), RED);
        assert_eq!(pixel(&out, 64, 5, 1), RED);
        assert_eq!(pixel(&out, 64, 6, 1), BLUE);
        assert_eq!(pixel(&out, 64, 63, 4), BLUE);

        // A run longer than 255 needs continuation bytes: 300 = 1 + 255 + 44.
        let mut tiles = vec![128];
        tiles.extend(GREEN);
        tiles.extend([255, 44]);
        tiles.extend(WHITE);
        tiles.push(19);
        let out = decode(64, 5, &tiles).unwrap();
        assert_eq!(pixel(&out, 64, 299 % 64, 299 / 64), GREEN);
        assert_eq!(pixel(&out, 64, 300 % 64, 300 / 64), WHITE);
    }

    #[test]
    fn palette_rle_mixes_single_pixels_and_runs() {
        // 4 x 2: red, run of 5 blue, green, white.
        let mut tiles = vec![128 + 4];
        for c in [RED, GREEN, BLUE, WHITE] {
            tiles.extend(c);
        }
        tiles.extend([0, 0x82, 4, 1, 3]);
        let out = decode(4, 2, &tiles).unwrap();
        let expected = [[RED, BLUE, BLUE, BLUE], [BLUE, BLUE, GREEN, WHITE]];
        for (y, row) in expected.iter().enumerate() {
            for (x, &c) in row.iter().enumerate() {
                assert_eq!(pixel(&out, 4, x, y), c, "{x},{y}");
            }
        }
    }

    #[test]
    fn one_zlib_stream_spans_rectangles() {
        let mut encoder = Encoder::new();
        let mut decoder = ZrleDecoder::default();
        for colour in [RED, GREEN, RED] {
            let mut tiles = vec![0];
            tiles.extend(colour.repeat(4));
            let out = pixels(&mut decoder, 2, 2, &encoder.rect(&tiles)).unwrap();
            assert_eq!(&out[12..], &rgba(colour));
        }
        // A later rectangle alone lacks the zlib header: a fresh stream must reject it.
        let payload = encoder.rect(&[1, 1, 2, 3]);
        let mut out = Vec::new();
        let fresh = ZrleDecoder::default().decode(2, 2, &payload, &mut out);
        assert!(matches!(fresh, Err(Error::Zlib(_))), "{fresh:?}");
        let result = decoder.decode(2, 2, &payload, &mut out).unwrap();
        assert_eq!(result, Decoded::Solid([1, 2, 3]));
    }

    #[test]
    fn large_rectangles_grow_the_inflate_buffer() {
        // 256 x 256 raw tiles: 16 tiles of 12 KiB, highly compressible.
        let mut tiles = Vec::new();
        for _ in 0..16 {
            tiles.push(0);
            tiles.extend(std::iter::repeat_n(7u8, TILE * TILE * 3));
        }
        let out = decode(256, 256, &tiles).unwrap();
        assert!(out.as_chunks::<4>().0.iter().all(|px| *px == [7, 7, 7, 255]));
    }

    #[test]
    fn empty_rectangle() {
        assert!(decode(0, 5, &[]).unwrap().is_empty());
    }

    fn assert_protocol_error(width: usize, height: usize, tiles: &[u8]) {
        match decode(width, height, tiles) {
            Err(Error::Protocol(_)) => {}
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_tiles_are_errors() {
        // Unused sub-encodings.
        assert_protocol_error(1, 1, &[17, 0, 0, 0]);
        assert_protocol_error(1, 1, &[127, 0, 0, 0]);
        assert_protocol_error(1, 1, &[129, 0, 0, 0, 0]);
        // Truncated raw tile and missing tiles.
        assert_protocol_error(2, 1, &[0, 1, 2, 3]);
        assert_protocol_error(65, 1, &[1, 1, 2, 3]);
        // Packed palette index 3 with a 3-colour palette.
        assert_protocol_error(1, 1, &[3, 0, 0, 0, 1, 1, 1, 2, 2, 2, 0b1100_0000]);
        // Plain RLE run longer than the tile (4 pixels).
        assert_protocol_error(2, 2, &[128, 9, 9, 9, 4]);
        assert_protocol_error(2, 2, &[128, 9, 9, 9, 255, 255, 255]);
        // Palette RLE index past the palette, and a run overflowing the tile.
        assert_protocol_error(2, 2, &[130, 1, 1, 1, 2, 2, 2, 2]);
        assert_protocol_error(2, 2, &[130, 1, 1, 1, 2, 2, 2, 0x81, 4]);
    }

    #[test]
    fn inflated_data_larger_than_the_rectangle_is_rejected() {
        // A 1x1 rectangle can never need more than a few hundred bytes.
        let tiles = vec![0u8; 100_000];
        assert_protocol_error(1, 1, &tiles);
    }

    #[test]
    fn oversized_rectangles_are_rejected_before_allocating() {
        let mut out = Vec::new();
        let payload = Encoder::new().rect(&[1, 0, 0, 0]);
        let result = ZrleDecoder::default().decode(65535, 65535, &payload, &mut out);
        assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
        assert_eq!(out.capacity(), 0);
    }

    #[test]
    fn inflate_cap_fits_every_rectangle_of_an_accepted_framebuffer() {
        // The session accepts edges up to 16384 and at most 8192 x 8192 pixels. For each width,
        // the tallest such rectangle has the most tiles, so the cap must not bite for any of them.
        for width in 1..=16384 {
            let height = (8192 * 8192 / width).min(16384);
            let exact = width * height * 4 + tile_count(width, height) * TILE_OVERHEAD;
            assert!(width * height * 4 <= MAX_DECODED);
            assert_eq!(max_inflated_len(width, height), exact, "{width}x{height}");
            assert!(exact < MAX_INFLATED, "{width}x{height}");
        }
    }

    #[test]
    fn payload_bound_covers_incompressible_rectangles() {
        // Pseudo-random raw tiles do not compress; stored blocks are the worst real case.
        let mut state = 0x2545_f491_u32;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        };
        for (width, height) in [(1, 1), (3, 5), (64, 64), (200, 70)] {
            let mut tiles = Vec::new();
            for ty in (0..height).step_by(TILE) {
                for tx in (0..width).step_by(TILE) {
                    let pixels = TILE.min(width - tx) * TILE.min(height - ty);
                    tiles.push(0);
                    tiles.extend((0..pixels * 3).map(|_| noise()));
                }
            }
            for level in [Compression::none(), Compression::fast(), Compression::best()] {
                let mut zlib = ZlibEncoder::new(Vec::new(), level);
                zlib.write_all(&tiles).unwrap();
                zlib.flush().unwrap();
                let payload = zlib.get_ref();
                assert!(payload.len() <= max_payload_len(width, height), "{width}x{height} {level:?}");
                let mut out = Vec::new();
                ZrleDecoder::default().decode(width, height, payload, &mut out).unwrap();
            }
        }
        assert!(max_payload_len(4, 4) < 4096);
    }

    #[test]
    fn garbage_zlib_data_is_an_error() {
        let mut out = Vec::new();
        let result = ZrleDecoder::default().decode(2, 2, &[0xde, 0xad, 0xbe, 0xef], &mut out);
        assert!(matches!(result, Err(Error::Zlib(_))), "{result:?}");
    }
}
