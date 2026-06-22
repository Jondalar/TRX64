// Minimal, dependency-free PNG codec for the render gate.
//
// decodePng(buf) -> { width, height, rgba: Uint8Array (RGBA, w*h*4) }
//   Supports the subset both encoders here emit: 8-bit, color type 2 (RGB) or
//   6 (RGBA), no interlace, with the 5 standard scanline filters. Uses Node's
//   built-in zlib for the IDAT inflate — no external deps (ADR: gate tooling
//   must be self-contained).
//
// This is a PIXEL comparator helper: we decode the TS golden PNG and the TRX64
// PNG to raw RGBA and diff pixels, NOT PNG container bytes (zlib output differs
// between the Rust `png` crate and Node's encoder, so a byte diff would be a
// spurious RED — see the vic-render gate note).

import zlib from "node:zlib";

function paeth(a, b, c) {
  const p = a + b - c;
  const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
  if (pa <= pb && pa <= pc) return a;
  if (pb <= pc) return b;
  return c;
}

export function decodePng(buf) {
  const sig = [137, 80, 78, 71, 13, 10, 26, 10];
  for (let i = 0; i < 8; i++) if (buf[i] !== sig[i]) throw new Error("not a PNG");
  let off = 8;
  let width = 0, height = 0, bitDepth = 0, colorType = 0, interlace = 0;
  const idat = [];
  while (off < buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8];
      colorType = data[9];
      interlace = data[12];
    } else if (type === "IDAT") {
      idat.push(Buffer.from(data));
    } else if (type === "IEND") {
      break;
    }
    off += 12 + len; // len + type(4) + data + crc(4)
  }
  if (bitDepth !== 8) throw new Error(`unsupported bitDepth ${bitDepth}`);
  if (interlace !== 0) throw new Error("interlace unsupported");
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : null;
  if (channels === null) throw new Error(`unsupported colorType ${colorType}`);

  const raw = zlib.inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const out = new Uint8Array(width * height * 4);
  const recon = new Uint8Array(height * stride);
  let pos = 0;
  for (let y = 0; y < height; y++) {
    const filter = raw[pos++];
    const rowStart = y * stride;
    for (let x = 0; x < stride; x++) {
      const rawByte = raw[pos++];
      const a = x >= channels ? recon[rowStart + x - channels] : 0;
      const b = y > 0 ? recon[rowStart - stride + x] : 0;
      const c = (x >= channels && y > 0) ? recon[rowStart - stride + x - channels] : 0;
      let val;
      switch (filter) {
        case 0: val = rawByte; break;
        case 1: val = rawByte + a; break;
        case 2: val = rawByte + b; break;
        case 3: val = rawByte + ((a + b) >> 1); break;
        case 4: val = rawByte + paeth(a, b, c); break;
        default: throw new Error(`bad filter ${filter}`);
      }
      recon[rowStart + x] = val & 0xff;
    }
    for (let x = 0; x < width; x++) {
      const si = rowStart + x * channels;
      const di = (y * width + x) * 4;
      out[di] = recon[si];
      out[di + 1] = recon[si + 1];
      out[di + 2] = recon[si + 2];
      out[di + 3] = channels === 4 ? recon[si + 3] : 0xff;
    }
  }
  return { width, height, rgba: out };
}

// Compare two RGBA buffers. Returns null if identical, else first divergence
// { x, y, expected: [r,g,b,a], got: [r,g,b,a], totalDiff }.
export function diffRgba(aw, ah, a, bw, bh, b) {
  if (aw !== bw || ah !== bh) {
    return { dim: true, expected: [aw, ah], got: [bw, bh] };
  }
  let first = null, totalDiff = 0;
  for (let y = 0; y < ah; y++) {
    for (let x = 0; x < aw; x++) {
      const i = (y * aw + x) * 4;
      if (a[i] !== b[i] || a[i + 1] !== b[i + 1] || a[i + 2] !== b[i + 2] || a[i + 3] !== b[i + 3]) {
        totalDiff++;
        if (!first) {
          first = {
            x, y,
            expected: [a[i], a[i + 1], a[i + 2], a[i + 3]],
            got: [b[i], b[i + 1], b[i + 2], b[i + 3]],
          };
        }
      }
    }
  }
  if (!first) return null;
  first.totalDiff = totalDiff;
  return first;
}
