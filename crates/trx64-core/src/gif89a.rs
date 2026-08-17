//! gif89a.rs — Spec 812: an animated GIF89a written straight from VIC colour indices.
//!
//! The VIC already hands us exactly what a GIF wants. `render_canvas_indices`
//! returns 384×272 with **one byte per pixel, each a 4-bit colour index**, and
//! [`crate::render::COLODORE`] is **16 RGB entries**. A GIF global colour table is
//! a palette of that shape and GIF pixel data IS palette indices — so there is
//! nothing to quantize, nothing to dither, and no colour that drifts between
//! frames. That is the whole reason this file is short.
//!
//! Written here rather than pulled in: the input is already palette-indexed (so
//! the encoder is a bit-packer and an LZW loop), the byte-budget clamp belongs
//! next to the frames it drops, and the workspace gains no dependency.
//!
//! Structure produced (GIF89a, §Appendix B of the 89a spec):
//!
//! ```text
//!   "GIF89a"
//!   Logical Screen Descriptor      w, h, packed(GCT|res|size), bg, aspect
//!   Global Colour Table            16 × RGB
//!   Application Extension          NETSCAPE2.0, loop forever
//!   per frame:
//!     Graphic Control Extension    disposal=2 (restore to background), delay
//!     Image Descriptor             full-frame, no local table, not interlaced
//!     LZW data                     min-code-size, then ≤255-byte sub-blocks
//!   0x3B                           trailer
//! ```
//!
//! `disposal = 2` is what makes the cut HARD: each frame replaces the canvas
//! instead of compositing onto it, which is what a release reel wants and what a
//! crossfade would violate.

/// GIF delays are centiseconds. A reel's delay is uniform across all frames
/// (Spec 812 §6 — CSDb asks for one delay per project).
pub type Centiseconds = u16;

/// One frame: `width * height` palette indices, each `< palette.len()`.
#[derive(Debug, Clone)]
pub struct Frame {
    pub indices: Vec<u8>,
}

/// What [`encode`] rejected, and why — never silent (Spec 812 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    /// Indices (into the caller's frame list) that were dropped to meet a byte
    /// budget, in the order they were dropped.
    pub dropped: Vec<usize>,
}

/// Encode `frames` as a looping GIF89a. `palette` must hold 2..=256 entries; the
/// global colour table is padded to the next power of two, as the format requires.
///
/// Every frame must be exactly `width * height` bytes.
pub fn encode(
    width: u16,
    height: u16,
    palette: &[[u8; 3]],
    frames: &[Frame],
    delay: Centiseconds,
) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("gif89a: no frames".into());
    }
    if palette.len() < 2 || palette.len() > 256 {
        return Err(format!("gif89a: palette must hold 2..=256 entries, got {}", palette.len()));
    }
    let px = width as usize * height as usize;
    for (i, f) in frames.iter().enumerate() {
        if f.indices.len() != px {
            return Err(format!(
                "gif89a: frame {i} is {} bytes, expected {px} ({width}×{height})",
                f.indices.len()
            ));
        }
    }

    // GCT size field n encodes 2^(n+1) entries, so the table is padded up.
    let gct_bits = gct_bits_for(palette.len());
    let gct_entries = 1usize << gct_bits;
    // The LZW minimum code size must be ≥ 2 even for a 2-colour image (89a spec).
    let min_code_size = gct_bits.max(2);

    let mut out = Vec::with_capacity(px * frames.len() / 2 + 1024);
    out.extend_from_slice(b"GIF89a");

    // ── Logical Screen Descriptor ────────────────────────────────────────────
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    // bit7 global colour table present; bits6-4 colour resolution (bits-1);
    // bit3 sort flag (0); bits2-0 table size exponent - 1.
    out.push(0x80 | (((gct_bits - 1) & 0x07) << 4) | ((gct_bits - 1) & 0x07));
    out.push(0); // background colour index
    out.push(0); // pixel aspect ratio: not specified

    // ── Global Colour Table, padded to the declared size ─────────────────────
    for i in 0..gct_entries {
        let [r, g, b] = palette.get(i).copied().unwrap_or([0, 0, 0]);
        out.push(r);
        out.push(g);
        out.push(b);
    }

    // ── Netscape looping extension (loop forever) ────────────────────────────
    out.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    out.extend_from_slice(b"NETSCAPE2.0");
    out.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);

    for frame in frames {
        // ── Graphic Control Extension ────────────────────────────────────────
        // packed: reserved(3) | disposal(3) = 2 | user input(1) = 0 | transparent(1) = 0
        out.extend_from_slice(&[0x21, 0xF9, 0x04, 0x02 << 2]);
        out.extend_from_slice(&delay.to_le_bytes());
        out.push(0x00); // transparent colour index (unused)
        out.push(0x00); // block terminator

        // ── Image Descriptor: full frame, no local table, not interlaced ─────
        out.push(0x2C);
        out.extend_from_slice(&0u16.to_le_bytes()); // left
        out.extend_from_slice(&0u16.to_le_bytes()); // top
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.push(0x00);

        // ── LZW image data ───────────────────────────────────────────────────
        out.push(min_code_size);
        let lzw = lzw_compress(&frame.indices, min_code_size);
        write_sub_blocks(&mut out, &lzw);
    }

    out.push(0x3B); // trailer
    Ok(out)
}

/// Encode under a hard byte budget. Over budget, **frames are dropped, never
/// re-encoded at lower fidelity and never truncated** (Spec 812 §6) — a truncated
/// GIF is a corrupt file, and a silently degraded one is a lie about what the
/// machine drew.
///
/// Frames are dropped from the middle outwards, so the first and last frame — the
/// ones that establish and close the reel — are the last to go. Returns which
/// indices went.
pub fn encode_within(
    width: u16,
    height: u16,
    palette: &[[u8; 3]],
    frames: &[Frame],
    delay: Centiseconds,
    max_bytes: usize,
) -> Result<Encoded, String> {
    let mut keep: Vec<usize> = (0..frames.len()).collect();
    let mut dropped: Vec<usize> = Vec::new();

    loop {
        let subset: Vec<Frame> = keep.iter().map(|&i| frames[i].clone()).collect();
        let bytes = encode(width, height, palette, &subset, delay)?;
        if bytes.len() <= max_bytes || keep.len() <= 1 {
            if bytes.len() > max_bytes {
                return Err(format!(
                    "gif89a: a single {width}×{height} frame is {} bytes, over the \
                     {max_bytes}-byte budget — nothing left to drop",
                    bytes.len()
                ));
            }
            return Ok(Encoded { bytes, dropped });
        }
        // Drop the middle-most surviving frame.
        let victim = keep.len() / 2;
        dropped.push(keep.remove(victim));
    }
}

/// Table-size exponent: the smallest `n` with `2^n >= len`, clamped to 1..=8.
fn gct_bits_for(len: usize) -> u8 {
    let mut bits = 1u8;
    while (1usize << bits) < len && bits < 8 {
        bits += 1;
    }
    bits
}

/// Split `data` into ≤255-byte sub-blocks, each length-prefixed, terminated by 0.
fn write_sub_blocks(out: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0x00);
}

/// LSB-first variable-width bit packer (GIF's bit order).
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, bits: 0 }
    }
    fn write(&mut self, code: u16, width: u32) {
        self.acc |= (code as u32) << self.bits;
        self.bits += width;
        while self.bits >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

/// GIF-flavoured LZW. Clear code = `1 << min_code_size`, end code = clear + 1,
/// first assignable code = clear + 2, codes grow to 12 bits and then the table is
/// reset with an explicit clear (which is what keeps a decoder in step).
fn lzw_compress(data: &[u8], min_code_size: u8) -> Vec<u8> {
    let clear: u16 = 1 << min_code_size;
    let end: u16 = clear + 1;
    const MAX_CODE: u16 = 4096;

    let mut w = BitWriter::new();
    let mut code_size: u32 = min_code_size as u32 + 1;
    // (prefix, byte) → code. A flat table beats a HashMap here and keeps the
    // encoder allocation-stable across frames.
    let mut dict: std::collections::HashMap<(u16, u8), u16> = std::collections::HashMap::new();
    let mut next_code: u16 = end + 1;

    w.write(clear, code_size);

    if data.is_empty() {
        w.write(end, code_size);
        return w.finish();
    }

    let mut prefix: u16 = data[0] as u16;
    for &k in &data[1..] {
        match dict.get(&(prefix, k)) {
            Some(&code) => prefix = code,
            None => {
                w.write(prefix, code_size);
                if next_code < MAX_CODE {
                    dict.insert((prefix, k), next_code);
                    next_code += 1;
                    // Widen STRICTLY past the current width, not at it. A decoder
                    // learns each entry one code late (it needs the following code
                    // to know the entry's last byte), so it counts one behind us.
                    // Growing at `== 1<<code_size` writes the first wide code while
                    // every conforming decoder is still reading narrow ones — the
                    // stream then decodes as garbage and libraries reject it
                    // outright. Verified against an outside decoder, not by
                    // agreement with our own.
                    if next_code as u32 > (1u32 << code_size) && code_size < 12 {
                        code_size += 1;
                    }
                } else {
                    w.write(clear, code_size);
                    dict.clear();
                    next_code = end + 1;
                    code_size = min_code_size as u32 + 1;
                }
                prefix = k as u16;
            }
        }
    }
    w.write(prefix, code_size);
    w.write(end, code_size);
    w.finish()
}

// ─────────────────────────────────────────────────────────────────────────────
// Structural verification (Spec 812 §6)
// ─────────────────────────────────────────────────────────────────────────────

/// What a block-structure walk found. This is the honest check: scanning a GIF
/// for the `21 F9` GCE marker FALSE-POSITIVES inside LZW pixel data, because that
/// byte pair is ordinary compressed output. The only way to know how many frames
/// a GIF has is to walk it as blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GifStructure {
    pub width: u16,
    pub height: u16,
    pub palette_entries: usize,
    pub frames: usize,
    /// Per-frame delay in centiseconds, in file order.
    pub delays: Vec<u16>,
    /// Per-frame disposal method, in file order.
    pub disposals: Vec<u8>,
    pub loops_forever: bool,
}

/// Walk a GIF89a as blocks. Returns an error at the first structural surprise
/// rather than guessing — a reel that does not parse is not a reel.
pub fn parse_structure(bytes: &[u8]) -> Result<GifStructure, String> {
    let mut p = 0usize;
    let need = |p: usize, n: usize| -> Result<(), String> {
        if p + n > bytes.len() {
            Err(format!("gif: truncated at {p} (wanted {n} more)"))
        } else {
            Ok(())
        }
    };

    need(p, 6)?;
    if &bytes[0..6] != b"GIF89a" {
        return Err("gif: not a GIF89a header".into());
    }
    p += 6;

    need(p, 7)?;
    let width = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
    let height = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
    let packed = bytes[p + 4];
    p += 7;

    let mut palette_entries = 0usize;
    if packed & 0x80 != 0 {
        palette_entries = 1usize << ((packed & 0x07) + 1);
        need(p, palette_entries * 3)?;
        p += palette_entries * 3;
    }

    let mut frames = 0usize;
    let mut delays = Vec::new();
    let mut disposals = Vec::new();
    let mut loops_forever = false;
    // A GCE describes the image that follows it.
    let mut pending: Option<(u16, u8)> = None;

    loop {
        need(p, 1)?;
        match bytes[p] {
            0x3B => break, // trailer
            0x21 => {
                need(p + 1, 1)?;
                let label = bytes[p + 1];
                p += 2;
                if label == 0xF9 {
                    need(p, 1)?;
                    let len = bytes[p] as usize;
                    if len != 4 {
                        return Err(format!("gif: graphic control block is {len} bytes, expected 4"));
                    }
                    need(p + 1, 4)?;
                    let gpacked = bytes[p + 1];
                    let delay = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
                    pending = Some((delay, (gpacked >> 2) & 0x07));
                    p += 1 + len;
                    p = skip_sub_blocks(bytes, p)?;
                } else {
                    if label == 0xFF {
                        need(p, 1)?;
                        let len = bytes[p] as usize;
                        need(p + 1, len)?;
                        if &bytes[p + 1..p + 1 + len] == b"NETSCAPE2.0" {
                            loops_forever = true;
                        }
                        p += 1 + len;
                        p = skip_sub_blocks(bytes, p)?;
                    } else {
                        need(p, 1)?;
                        let len = bytes[p] as usize;
                        need(p + 1, len)?;
                        p += 1 + len;
                        p = skip_sub_blocks(bytes, p)?;
                    }
                }
            }
            0x2C => {
                need(p + 1, 9)?;
                let lpacked = bytes[p + 9];
                p += 10;
                if lpacked & 0x80 != 0 {
                    let local = 3 * (1usize << ((lpacked & 0x07) + 1));
                    need(p, local)?;
                    p += local;
                }
                need(p, 1)?;
                p += 1; // LZW minimum code size
                p = skip_sub_blocks(bytes, p)?;
                frames += 1;
                let (d, disp) = pending.take().unwrap_or((0, 0));
                delays.push(d);
                disposals.push(disp);
            }
            other => return Err(format!("gif: unexpected block 0x{other:02x} at {p}")),
        }
    }

    Ok(GifStructure {
        width,
        height,
        palette_entries,
        frames,
        delays,
        disposals,
        loops_forever,
    })
}

fn skip_sub_blocks(bytes: &[u8], mut p: usize) -> Result<usize, String> {
    loop {
        if p >= bytes.len() {
            return Err(format!("gif: sub-block chain runs off the end at {p}"));
        }
        let len = bytes[p] as usize;
        p += 1;
        if len == 0 {
            return Ok(p);
        }
        if p + len > bytes.len() {
            return Err(format!("gif: sub-block of {len} at {p} runs off the end"));
        }
        p += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An independent LZW DECODER. The encoder is only trustworthy if something
    /// that does not share its code can read it back — a byte-count assertion
    /// would pass just as happily on a stream no decoder accepts.
    fn lzw_decompress(data: &[u8], min_code_size: u8) -> Vec<u8> {
        let clear: u16 = 1 << min_code_size;
        let end: u16 = clear + 1;
        let mut code_size: u32 = min_code_size as u32 + 1;
        let mut table: Vec<Vec<u8>> = Vec::new();
        let reset = |table: &mut Vec<Vec<u8>>| {
            table.clear();
            for i in 0..=(clear + 1) {
                table.push(if i < clear { vec![i as u8] } else { Vec::new() });
            }
        };
        reset(&mut table);

        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits: u32 = 0;
        let mut pos = 0usize;
        let mut prev: Option<u16> = None;
        // The counter a conforming decoder keeps (giflib's `RunningCode`): bumped
        // once per code READ, including the first one after a clear, which adds no
        // entry. That deliberate one-ahead accounting is what keeps its width in
        // step with an encoder that widens strictly past `1 << code_size`.
        let mut running: u32 = end as u32 + 1;

        loop {
            while bits < code_size && pos < data.len() {
                acc |= (data[pos] as u32) << bits;
                bits += 8;
                pos += 1;
            }
            if bits < code_size {
                break;
            }
            let code = (acc & ((1u32 << code_size) - 1)) as u16;
            acc >>= code_size;
            bits -= code_size;

            if code == clear {
                reset(&mut table);
                code_size = min_code_size as u32 + 1;
                running = end as u32 + 1;
                prev = None;
                continue;
            }
            if code == end {
                break;
            }
            let entry: Vec<u8> = if (code as usize) < table.len() {
                table[code as usize].clone()
            } else {
                let p = prev.expect("KwKwK with no previous code");
                let mut e = table[p as usize].clone();
                e.push(table[p as usize][0]);
                e
            };
            out.extend_from_slice(&entry);
            if let Some(p) = prev {
                let mut ne = table[p as usize].clone();
                ne.push(entry[0]);
                table.push(ne);
            }
            running += 1;
            if running > (1u32 << code_size) && code_size < 12 {
                code_size += 1;
            }
            prev = Some(code);
        }
        out
    }

    fn pal16() -> Vec<[u8; 3]> {
        crate::render::COLODORE.to_vec()
    }

    /// Concatenate a frame's LZW sub-blocks back into one stream.
    fn frame_lzw_streams(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut streams = Vec::new();
        let mut p = 6 + 7;
        let packed = bytes[10];
        if packed & 0x80 != 0 {
            p += 3 * (1usize << ((packed & 0x07) + 1));
        }
        while p < bytes.len() && bytes[p] != 0x3B {
            match bytes[p] {
                0x21 => {
                    p += 2;
                    let len = bytes[p] as usize;
                    p += 1 + len;
                    while bytes[p] != 0 {
                        let l = bytes[p] as usize;
                        p += 1 + l;
                    }
                    p += 1;
                }
                0x2C => {
                    p += 10;
                    let mcs = bytes[p];
                    p += 1;
                    let mut data = Vec::new();
                    while bytes[p] != 0 {
                        let l = bytes[p] as usize;
                        data.extend_from_slice(&bytes[p + 1..p + 1 + l]);
                        p += 1 + l;
                    }
                    p += 1;
                    streams.push((mcs, data));
                }
                other => panic!("unexpected block 0x{other:02x} at {p}"),
            }
        }
        streams
    }

    fn checkerboard(w: usize, h: usize, phase: u8) -> Frame {
        let mut indices = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                indices[y * w + x] = (((x / 8 + y / 8 + phase as usize) % 16) as u8) & 0x0f;
            }
        }
        Frame { indices }
    }

    #[test]
    fn a_reel_is_a_wellformed_gif89a() {
        let frames = vec![checkerboard(384, 272, 0), checkerboard(384, 272, 3)];
        let bytes = encode(384, 272, &pal16(), &frames, 70).expect("encode");
        let s = parse_structure(&bytes).expect("parse");
        assert_eq!(s.width, 384);
        assert_eq!(s.height, 272);
        assert_eq!(s.palette_entries, 16, "16 COLODORE entries, no padding waste");
        assert_eq!(s.frames, 2);
        assert_eq!(s.delays, vec![70, 70], "one uniform delay across the reel");
        assert_eq!(s.disposals, vec![2, 2], "disposal 2 = hard cut");
        assert!(s.loops_forever);
        assert_eq!(*bytes.last().unwrap(), 0x3B, "trailer");
    }

    #[test]
    fn the_pixels_survive_the_round_trip() {
        let frames = vec![checkerboard(64, 40, 0), checkerboard(64, 40, 7)];
        let bytes = encode(64, 40, &pal16(), &frames, 50).expect("encode");
        let streams = frame_lzw_streams(&bytes);
        assert_eq!(streams.len(), 2);
        for (i, (mcs, data)) in streams.iter().enumerate() {
            let back = lzw_decompress(data, *mcs);
            assert_eq!(&back, &frames[i].indices, "frame {i} decodes to its own pixels");
        }
    }

    /// The long, low-entropy runs a C64 screen is full of push the code width up
    /// through every step to 12 bits and past a table reset. That path is where a
    /// hand-written LZW breaks, so exercise it deliberately.
    #[test]
    fn the_code_width_grows_and_the_table_resets_without_losing_a_pixel() {
        let w = 384usize;
        let h = 272usize;
        let mut indices = vec![0u8; w * h];
        // A pattern with enough novelty to fill 4096 codes and force a clear.
        for (i, px) in indices.iter_mut().enumerate() {
            *px = (((i * 7 + i / 97) % 16) as u8) & 0x0f;
        }
        let frames = vec![Frame { indices: indices.clone() }];
        let bytes = encode(w as u16, h as u16, &pal16(), &frames, 70).expect("encode");
        let streams = frame_lzw_streams(&bytes);
        let back = lzw_decompress(&streams[0].1, streams[0].0);
        assert_eq!(back, indices, "every pixel survives a mid-stream table reset");
    }

    #[test]
    fn identical_frames_encode_to_identical_bytes() {
        let frames = vec![checkerboard(128, 96, 2)];
        let a = encode(128, 96, &pal16(), &frames, 70).unwrap();
        let b = encode(128, 96, &pal16(), &frames, 70).unwrap();
        assert_eq!(a, b, "the encoder contributes nothing non-deterministic");
    }

    #[test]
    fn over_budget_drops_frames_and_says_which() {
        let frames: Vec<Frame> = (0..8).map(|p| checkerboard(384, 272, p)).collect();
        let full = encode(384, 272, &pal16(), &frames, 70).unwrap();
        let budget = full.len() * 6 / 10;
        let e = encode_within(384, 272, &pal16(), &frames, 70, budget).expect("clamp");
        assert!(e.bytes.len() <= budget, "the budget is a ceiling, not a hint");
        assert!(!e.dropped.is_empty(), "something had to go, and it is named");
        let s = parse_structure(&e.bytes).expect("still a valid GIF after dropping");
        assert_eq!(s.frames, frames.len() - e.dropped.len());
        assert!(
            !e.dropped.contains(&0) && !e.dropped.contains(&(frames.len() - 1)),
            "the opening and closing frames are the last to be dropped"
        );
    }

    #[test]
    fn a_budget_no_single_frame_fits_fails_loudly() {
        let frames = vec![checkerboard(384, 272, 0)];
        let err = encode_within(384, 272, &pal16(), &frames, 70, 32).unwrap_err();
        assert!(err.contains("nothing left to drop"), "got: {err}");
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let frames = vec![Frame { indices: vec![0u8; 10] }];
        let err = encode(384, 272, &pal16(), &frames, 70).unwrap_err();
        assert!(err.contains("expected"), "got: {err}");
    }

    /// Cross-decoder gate. The in-test decoder above shares my reading of the
    /// format, so agreeing with it proves only self-consistency. This writes a
    /// reel where a REAL decoder can read it, and is opt-in because it needs one
    /// installed:
    ///
    /// ```text
    ///   TRX64_GIF_PROBE=/tmp/probe.gif cargo test -p trx64-core --lib gif89a
    ///   python3 -c "from PIL import Image, ImageSequence; …"
    /// ```
    #[test]
    fn writes_a_probe_reel_for_an_outside_decoder() {
        let Ok(path) = std::env::var("TRX64_GIF_PROBE") else {
            return;
        };
        let w = 384usize;
        let h = 272usize;
        let mut frames = Vec::new();
        for phase in 0..3u8 {
            let mut indices = vec![0u8; w * h];
            for (i, px) in indices.iter_mut().enumerate() {
                *px = (((i * 13 + i / 31 + phase as usize * 977) % 16) as u8) & 0x0f;
            }
            frames.push(Frame { indices });
        }
        let bytes = encode(w as u16, h as u16, &pal16(), &frames, 70).unwrap();
        std::fs::write(&path, &bytes).expect("write probe");
        // Alongside it, the raw indices, so the outside decoder can be compared
        // pixel-for-pixel rather than just "it opened".
        for (i, f) in frames.iter().enumerate() {
            std::fs::write(format!("{path}.frame{i}.raw"), &f.indices).expect("write raw");
        }
        eprintln!("probe reel → {path} ({} bytes, {} frames)", bytes.len(), frames.len());
    }

    /// The trap named in the feature request: `21 F9` occurs inside compressed
    /// pixel data, so counting markers over-counts frames. The block walk must not.
    #[test]
    fn counting_gce_markers_would_overcount_but_the_block_walk_does_not() {
        let w = 384usize;
        let h = 272usize;
        let mut indices = vec![0u8; w * h];
        for (i, px) in indices.iter_mut().enumerate() {
            *px = (((i * 13 + i / 31) % 16) as u8) & 0x0f;
        }
        let frames = vec![Frame { indices: indices.clone() }, Frame { indices }];
        let bytes = encode(w as u16, h as u16, &pal16(), &frames, 70).unwrap();
        let naive = bytes.windows(2).filter(|p| p[0] == 0x21 && p[1] == 0xF9).count();
        let s = parse_structure(&bytes).unwrap();
        assert_eq!(s.frames, 2, "the block walk counts what is really there");
        assert!(
            naive >= s.frames,
            "the naive marker scan found {naive}; it is never fewer, and it is not a count"
        );
    }
}
