//! `assets/plex-couch.ico` from `static/icon.svg`: 16, 24, 32 and 48 px 32-bit BMP images
//! plus a 256 px PNG. Written by `cargo run --example make_icon`; a test checks the file on
//! disk is up to date. The exe embeds the file twice: build.rs makes it the exe's icon and
//! [`crate::system::tray`] shows it in the notification area.
//!
//! The SVG is a handful of rounded rectangles and one triangle, so they are drawn here with
//! signed distance functions and 8x8 (4x4 at 256 px) supersampling instead of an SVG
//! library; the PNG is compressed with a small built-in deflate (fixed Huffman codes). Keep
//! [`SHAPES`] in step with icon.svg.

/// The generated icon, relative to the repository root.
pub const ICO_PATH: &str = "assets/plex-couch.ico";
/// The image sizes, in file order; 256 is stored as PNG.
pub const SIZES: [u32; 5] = [16, 24, 32, 48, 256];

type Rgba = [u8; 4];
const GOLD: Rgba = [0xE5, 0xA0, 0x0D, 255];
const BLACK: Rgba = [0, 0, 0, 255];
const DY: f64 = -22.0; // the <g transform="translate(0 -22)">

#[derive(Clone, Copy)]
enum Shape {
    /// x, y, w, h, corner radius
    RoundRect(f64, f64, f64, f64, f64),
    Triangle([(f64, f64); 3]),
}

/// A shape, its fill, its stroke colour and half its stroke width, painted in order like
/// the SVG.
struct Paint {
    shape: Shape,
    fill: Rgba,
    stroke: Option<Rgba>,
    half: f64,
}

const fn rect(x: f64, y: f64, w: f64, h: f64, r: f64, stroke: Option<Rgba>) -> Paint {
    Paint { shape: Shape::RoundRect(x, y, w, h, r), fill: GOLD, stroke, half: 6.0 }
}

/// icon.svg, shape by shape.
const SHAPES: [Paint; 8] = [
    Paint { shape: Shape::RoundRect(0.0, 0.0, 512.0, 512.0, 112.0), fill: BLACK, stroke: None, half: 0.0 },
    rect(148.0, 164.0 + DY, 216.0, 140.0, 30.0, None),
    rect(136.0, 292.0 + DY, 240.0, 72.0, 14.0, Some(BLACK)),
    rect(98.0, 232.0 + DY, 72.0, 132.0, 30.0, Some(BLACK)),
    rect(342.0, 232.0 + DY, 72.0, 132.0, 30.0, Some(BLACK)),
    rect(132.0, 364.0 + DY, 22.0, 30.0, 6.0, None),
    rect(358.0, 364.0 + DY, 22.0, 30.0, 6.0, None),
    // path M238 200v52l44-26z, filled and stroked 12 px with round joins: the triangle grown by 6
    Paint { shape: Shape::Triangle([(238.0, 200.0 + DY), (238.0, 252.0 + DY), (282.0, 226.0 + DY)]), fill: BLACK, stroke: None, half: 6.0 },
];

impl Shape {
    /// Bounding box with an 8 unit margin (enough for the 6 unit strokes).
    fn bbox(&self) -> (f64, f64, f64, f64) {
        match *self {
            Shape::RoundRect(x, y, w, h, _) => (x - 8.0, y - 8.0, x + w + 8.0, y + h + 8.0),
            Shape::Triangle(p) => {
                let xs = p.map(|q| q.0);
                let ys = p.map(|q| q.1);
                let min = |v: [f64; 3]| v[0].min(v[1]).min(v[2]);
                let max = |v: [f64; 3]| v[0].max(v[1]).max(v[2]);
                (min(xs) - 8.0, min(ys) - 8.0, max(xs) + 8.0, max(ys) + 8.0)
            }
        }
    }

    /// Signed distance from (px, py): negative inside.
    fn sdf(&self, px: f64, py: f64) -> f64 {
        match *self {
            Shape::RoundRect(x, y, w, h, r) => {
                let (cx, cy, bx, by) = (x + w / 2.0, y + h / 2.0, w / 2.0, h / 2.0);
                let qx = (px - cx).abs() - bx + r;
                let qy = (py - cy).abs() - by + r;
                qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
            }
            Shape::Triangle([a, b, c]) => {
                let seg = |p: (f64, f64), q: (f64, f64)| {
                    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
                    let t = (((px - p.0) * dx + (py - p.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                    (px - p.0 - t * dx).hypot(py - p.1 - t * dy)
                };
                let cross = |p: (f64, f64), q: (f64, f64)| (q.0 - p.0) * (py - p.1) - (q.1 - p.1) * (px - p.0);
                let d = seg(a, b).min(seg(b, c)).min(seg(c, a));
                let s = [cross(a, b), cross(b, c), cross(c, a)];
                let inside = s.iter().all(|&v| v >= 0.0) || s.iter().all(|&v| v <= 0.0);
                if inside { -d } else { d }
            }
        }
    }
}

/// The colour at one point of the 512x512 canvas (None: transparent).
fn sample(px: f64, py: f64) -> Option<Rgba> {
    let mut colour = None;
    for p in &SHAPES {
        let (x0, y0, x1, y1) = p.shape.bbox();
        if !(x0 <= px && px <= x1 && y0 <= py && py <= y1) {
            continue;
        }
        let d = p.shape.sdf(px, py);
        if let Shape::Triangle(_) = p.shape {
            if d <= p.half {
                colour = Some(p.fill);
            }
            continue;
        }
        if d <= 0.0 {
            colour = Some(p.fill);
        }
        if let Some(s) = p.stroke {
            if d.abs() <= p.half {
                colour = Some(s);
            }
        }
    }
    colour
}

/// The icon at `size` x `size`, rows top to bottom, RGBA with straight alpha.
pub fn render(size: u32) -> Vec<Rgba> {
    let ss: u32 = if size <= 64 { 8 } else { 4 };
    let scale = 512.0 / f64::from(size * ss);
    let k = f64::from(ss * ss);
    let mut out = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0u64; 4];
            for sy in 0..ss {
                for sx in 0..ss {
                    let px = (f64::from(x * ss + sx) + 0.5) * scale;
                    let py = (f64::from(y * ss + sy) + 0.5) * scale;
                    if let Some(c) = sample(px, py) {
                        let a = u64::from(c[3]);
                        for i in 0..3 {
                            acc[i] += u64::from(c[i]) * a;
                        }
                        acc[3] += a;
                    }
                }
            }
            let alpha = acc[3] as f64 / k;
            if alpha <= 0.0 {
                out.push([0, 0, 0, 0]);
            } else {
                let ch = |v: u64| (v as f64 / acc[3] as f64).round_ties_even() as u8;
                out.push([ch(acc[0]), ch(acc[1]), ch(acc[2]), alpha.round_ties_even() as u8]);
            }
        }
    }
    out
}

/// A 32-bit BMP icon image: BITMAPINFOHEADER, BGRA rows bottom-up, then the AND mask.
fn bmp(size: u32, px: &[Rgba]) -> Vec<u8> {
    let n = size as usize;
    let mut out = Vec::new();
    for v in [40u32, size, size * 2] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&[0; 24]); // compression .. colours important
    for row in px.chunks(n).rev() {
        for &[r, g, b, a] in row {
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    let stride = n.div_ceil(32) * 4;
    for row in px.chunks(n).rev() {
        let mut bits = vec![0u8; stride];
        for (x, p) in row.iter().enumerate() {
            if p[3] == 0 {
                bits[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&bits);
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    /// `n` bits of `v`, least significant first.
    fn put(&mut self, v: u32, n: u32) {
        self.acc |= v << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code (sent most significant bit first).
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn literal(&mut self, v: u32) {
        match v {
            0..=143 => self.code(0x30 + v, 8),
            144..=255 => self.code(0x190 + v - 144, 9),
            256..=279 => self.code(v - 256, 7),
            _ => self.code(0xC0 + v - 280, 8),
        }
    }
}

const LEN_BASE: [u32; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193,
    12289, 16385, 24577,
];

/// zlib stream: one deflate block with the fixed Huffman codes, greedy LZ77 matches.
fn zlib(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32768;
    const MAX_CHAIN: usize = 128;
    let mut w = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
    w.put(1, 1); // final block
    w.put(1, 2); // fixed Huffman codes
    let hash = |i: usize| (usize::from(data[i]) << 10 ^ usize::from(data[i + 1]) << 5 ^ usize::from(data[i + 2])) & 0x7FFF;
    let mut head = vec![usize::MAX; 0x8000];
    let mut prev = vec![usize::MAX; data.len()];
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + 2 < data.len() {
            let h = hash(i);
            prev[i] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + 2 < data.len() {
            let mut cand = head[hash(i)];
            let mut chain = 0;
            let max = (data.len() - i).min(258);
            while cand != usize::MAX && i - cand <= WINDOW && chain < MAX_CHAIN {
                let l = (0..max).take_while(|&k| data[cand + k] == data[i + k]).count();
                if l > best_len {
                    (best_len, best_dist) = (l, i - cand);
                    if l == max {
                        break;
                    }
                }
                cand = prev[cand];
                chain += 1;
            }
        }
        if best_len >= 3 {
            let li = LEN_BASE.iter().rposition(|&b| b as usize <= best_len).unwrap_or(0);
            w.literal(257 + li as u32);
            w.put(best_len as u32 - LEN_BASE[li], LEN_EXTRA[li]);
            let di = DIST_BASE.iter().rposition(|&b| b as usize <= best_dist).unwrap_or(0);
            w.code(di as u32, 5);
            w.put(best_dist as u32 - DIST_BASE[di], if di < 4 { 0 } else { (di as u32 - 2) / 2 });
            for j in i..i + best_len {
                insert(j, &mut head, &mut prev);
            }
            i += best_len;
        } else {
            w.literal(u32::from(data[i]));
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    w.literal(256);
    if w.n > 0 {
        w.out.push(w.acc as u8);
    }
    w.out.extend_from_slice(&adler32(data).to_be_bytes());
    w.out
}

fn png(size: u32, px: &[Rgba]) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut raw = Vec::with_capacity(px.len() * 4 + size as usize);
    for row in px.chunks(size as usize) {
        raw.push(0); // filter: none
        for p in row {
            raw.extend_from_slice(p);
        }
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The whole `.ico` file.
pub fn ico() -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| {
            let px = render(s);
            (s, if s >= 256 { png(s, &px) } else { bmp(s, &px) })
        })
        .collect();
    let mut out = Vec::new();
    for v in [0u16, 1, images.len() as u16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn asset_is_up_to_date() {
        let on_disk = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(ICO_PATH)).unwrap();
        assert!(on_disk == ico(), "{ICO_PATH} is out of date: run `cargo run --example make_icon`");
    }
}
