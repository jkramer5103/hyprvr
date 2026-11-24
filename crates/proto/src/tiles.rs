//! Lossless tile codec for screen content.
//!
//! Payload of `StreamMsg::Tiles` = zstd( record* ), each record:
//! ```text
//! x: u16 LE, y: u16 LE, w: u16 LE, h: u16 LE, kind: u8
//! kind 0 (Solid): r, g, b
//! kind 1 (Delta): w*h*3 bytes RGB, row-major; every byte is
//!        (value - predictor) mod 256 per channel, predictor = left pixel,
//!        or for column 0 the pixel above (0 for the very first pixel).
//! ```
//! The decoder outputs RGBA (alpha = 255).

use crate::msg::Rect;
use crate::TILE_SIZE;

/// Byte order of a 32-bit pixel in memory (wl_shm XRGB8888 = `Bgrx` on little endian,
/// XBGR8888 = `Rgbx`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Rgbx,
}

/// A borrowed 32-bit-per-pixel frame.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

impl Frame<'_> {
    #[inline]
    fn row(&self, y: u32, x: u32, w: u32) -> &[u8] {
        let start = y as usize * self.stride + x as usize * 4;
        &self.data[start..start + w as usize * 4]
    }
}

const KIND_SOLID: u8 = 0;
const KIND_DELTA: u8 = 1;

/// All tile rects of a `width x height` screen intersecting `r`.
pub fn tiles_covering(width: u32, height: u32, r: Rect) -> Vec<Rect> {
    let mut out = Vec::new();
    if r.w == 0 || r.h == 0 {
        return out;
    }
    let x1 = (r.x + r.w).min(width);
    let y1 = (r.y + r.h).min(height);
    let mut ty = r.y / TILE_SIZE * TILE_SIZE;
    while ty < y1 {
        let mut tx = r.x / TILE_SIZE * TILE_SIZE;
        while tx < x1 {
            out.push(Rect { x: tx, y: ty, w: TILE_SIZE.min(width - tx), h: TILE_SIZE.min(height - ty) });
            tx += TILE_SIZE;
        }
        ty += TILE_SIZE;
    }
    out
}

/// Remembers the last frame and finds tiles whose pixels actually changed.
pub struct TileDiffer {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    prev: Vec<u8>, // tightly packed 4 bytes/pixel, same byte order as input
    valid: Vec<bool>,
}

impl TileDiffer {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        TileDiffer {
            width,
            height,
            cols,
            rows,
            prev: vec![0; width as usize * height as usize * 4],
            valid: vec![false; (cols * rows) as usize],
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Forget everything: the next `diff` reports all tiles it examines.
    pub fn invalidate(&mut self) {
        self.valid.iter_mut().for_each(|v| *v = false);
    }

    /// Mark the tiles intersecting `r` as unknown (e.g. after video covered them).
    pub fn invalidate_rect(&mut self, r: Rect) {
        for t in tiles_covering(self.width, self.height, r) {
            let i = ((t.y / TILE_SIZE) * self.cols + t.x / TILE_SIZE) as usize;
            self.valid[i] = false;
        }
    }

    /// Compare `frame` against the stored frame within `damage` (or everywhere if `None`),
    /// update the stored copy and return the tiles that changed (or were invalid).
    pub fn diff(&mut self, frame: &Frame, damage: Option<&[Rect]>) -> Vec<Rect> {
        assert_eq!((frame.width, frame.height), (self.width, self.height), "frame size changed");
        let mut candidates = vec![false; (self.cols * self.rows) as usize];
        match damage {
            None => candidates.iter_mut().for_each(|c| *c = true),
            Some(rects) => {
                for r in rects {
                    for t in tiles_covering(self.width, self.height, *r) {
                        candidates[((t.y / TILE_SIZE) * self.cols + t.x / TILE_SIZE) as usize] = true;
                    }
                }
                // Invalid tiles are always re-examined.
                for (c, v) in candidates.iter_mut().zip(&self.valid) {
                    *c |= !*v;
                }
            }
        }
        let mut out = Vec::new();
        let pw = self.width as usize * 4;
        for row in 0..self.rows {
            for col in 0..self.cols {
                let i = (row * self.cols + col) as usize;
                if !candidates[i] {
                    continue;
                }
                let t = Rect {
                    x: col * TILE_SIZE,
                    y: row * TILE_SIZE,
                    w: TILE_SIZE.min(self.width - col * TILE_SIZE),
                    h: TILE_SIZE.min(self.height - row * TILE_SIZE),
                };
                let mut changed = !self.valid[i];
                for y in t.y..t.y + t.h {
                    let src = frame.row(y, t.x, t.w);
                    let off = y as usize * pw + t.x as usize * 4;
                    let dst = &mut self.prev[off..off + src.len()];
                    if changed || dst != src {
                        changed = true;
                        dst.copy_from_slice(src);
                    }
                }
                self.valid[i] = true;
                if changed {
                    out.push(t);
                }
            }
        }
        out
    }
}

#[inline]
fn rgb(px: &[u8], f: PixelFormat) -> [u8; 3] {
    match f {
        PixelFormat::Bgrx => [px[2], px[1], px[0]],
        PixelFormat::Rgbx => [px[0], px[1], px[2]],
    }
}

/// Encode the given tile rects of `frame` into a `StreamMsg::Tiles` payload.
pub fn encode_tiles(frame: &Frame, tiles: &[Rect], zstd_level: i32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(tiles.len() * 64);
    for t in tiles {
        encode_tile(frame, *t, &mut raw);
    }
    zstd::bulk::compress(&raw, zstd_level).expect("zstd compression of in-memory buffer")
}

fn encode_tile(frame: &Frame, t: Rect, out: &mut Vec<u8>) {
    debug_assert!(t.x + t.w <= frame.width && t.y + t.h <= frame.height);
    for v in [t.x, t.y, t.w, t.h] {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    // Solid check.
    let first = rgb(frame.row(t.y, t.x, 1), frame.format);
    let solid = (t.y..t.y + t.h).all(|y| frame.row(y, t.x, t.w).chunks_exact(4).all(|p| rgb(p, frame.format) == first));
    if solid {
        out.push(KIND_SOLID);
        out.extend_from_slice(&first);
        return;
    }
    out.push(KIND_DELTA);
    out.reserve(t.w as usize * t.h as usize * 3);
    let mut above = [0u8; 3];
    for y in t.y..t.y + t.h {
        let row = frame.row(y, t.x, t.w);
        let mut pred = above;
        for (i, p) in row.chunks_exact(4).enumerate() {
            let c = rgb(p, frame.format);
            out.extend_from_slice(&[c[0].wrapping_sub(pred[0]), c[1].wrapping_sub(pred[1]), c[2].wrapping_sub(pred[2])]);
            if i == 0 {
                above = c;
            }
            pred = c;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TileError {
    #[error("zstd: {0}")]
    Zstd(#[from] std::io::Error),
    #[error("truncated tile payload")]
    Truncated,
    #[error("bad tile kind {0}")]
    BadKind(u8),
    #[error("tile out of bounds")]
    OutOfBounds,
}

/// Decompressed size cap (a 4K screen in raw RGB plus headers fits comfortably).
const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// Decode a payload, calling `f(rect, rgba)` per tile with `rect.w*rect.h*4` RGBA bytes.
pub fn decode_tiles(payload: &[u8], mut f: impl FnMut(Rect, &[u8])) -> Result<(), TileError> {
    let raw = zstd::bulk::decompress(payload, MAX_DECOMPRESSED)?;
    let mut p = &raw[..];
    let mut rgba = Vec::new();
    while !p.is_empty() {
        if p.len() < 9 {
            return Err(TileError::Truncated);
        }
        let rd = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]) as u32;
        let r = Rect { x: rd(0), y: rd(2), w: rd(4), h: rd(6) };
        let kind = p[8];
        p = &p[9..];
        let n = r.w as usize * r.h as usize;
        if n > (TILE_SIZE * TILE_SIZE) as usize {
            return Err(TileError::OutOfBounds);
        }
        rgba.clear();
        rgba.reserve(n * 4);
        match kind {
            KIND_SOLID => {
                if p.len() < 3 {
                    return Err(TileError::Truncated);
                }
                let c = [p[0], p[1], p[2], 255];
                p = &p[3..];
                for _ in 0..n {
                    rgba.extend_from_slice(&c);
                }
            }
            KIND_DELTA => {
                if p.len() < n * 3 {
                    return Err(TileError::Truncated);
                }
                let (data, rest) = p.split_at(n * 3);
                p = rest;
                let mut above = [0u8; 3];
                for row in data.chunks_exact(r.w as usize * 3) {
                    let mut pred = above;
                    for (i, d) in row.chunks_exact(3).enumerate() {
                        let c = [d[0].wrapping_add(pred[0]), d[1].wrapping_add(pred[1]), d[2].wrapping_add(pred[2])];
                        rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
                        if i == 0 {
                            above = c;
                        }
                        pred = c;
                    }
                }
            }
            k => return Err(TileError::BadKind(k)),
        }
        f(r, &rgba);
    }
    Ok(())
}

/// Convenience: decode straight into an RGBA canvas. Returns the updated rects.
pub fn apply_tiles(payload: &[u8], canvas: &mut [u8], width: u32, height: u32) -> Result<Vec<Rect>, TileError> {
    let mut rects = Vec::new();
    let mut err = None;
    decode_tiles(payload, |r, rgba| {
        if r.x + r.w > width || r.y + r.h > height {
            err = Some(TileError::OutOfBounds);
            return;
        }
        for row in 0..r.h {
            let dst = ((r.y + row) * width + r.x) as usize * 4;
            let src = (row * r.w) as usize * 4;
            canvas[dst..dst + r.w as usize * 4].copy_from_slice(&rgba[src..src + r.w as usize * 4]);
        }
        rects.push(r);
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(rects),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                // Mostly flat "background" with some "text".
                let text = (x / 7 + y / 11 + seed) % 5 == 0 && y > 100;
                let c = if text { [(x * 3) as u8, (y + seed) as u8, 200] } else { [30, 32, 40] };
                v[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 0xff]);
            }
        }
        v
    }

    #[test]
    fn roundtrip_and_diff() {
        let (w, h) = (300u32, 200u32); // non-multiple of 64 to hit edge tiles
        let a = test_frame(w, h, 0);
        let fa = Frame { data: &a, stride: (w * 4) as usize, width: w, height: h, format: PixelFormat::Bgrx };
        let mut differ = TileDiffer::new(w, h);
        let tiles = differ.diff(&fa, None);
        assert_eq!(tiles.len(), 5 * 4);
        let payload = encode_tiles(&fa, &tiles, 1);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        apply_tiles(&payload, &mut canvas, w, h).unwrap();
        for i in (0..a.len()).step_by(4) {
            // Bgrx -> RGBA
            assert_eq!([canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]], [a[i + 2], a[i + 1], a[i], 255]);
        }
        // Unchanged frame -> no tiles.
        assert!(differ.diff(&fa, None).is_empty());
        // Change one pixel -> one tile, only if damage covers it.
        let mut b = a.clone();
        let off = ((150 * w + 70) * 4) as usize;
        b[off] ^= 0xff;
        let fb = Frame { data: &b, ..fa };
        assert!(differ.diff(&fb, Some(&[Rect { x: 0, y: 0, w: 10, h: 10 }])).is_empty());
        let t = differ.diff(&fb, Some(&[Rect { x: 70, y: 150, w: 1, h: 1 }]));
        assert_eq!(t, vec![Rect { x: 64, y: 128, w: 64, h: 64 }]);
    }

    #[test]
    fn padded_stride() {
        let (w, h) = (64u32, 64u32);
        let stride = 64 * 4 + 32;
        let mut buf = vec![7u8; stride * h as usize];
        buf[stride + 4] = 1;
        let f = Frame { data: &buf, stride, width: w, height: h, format: PixelFormat::Rgbx };
        let p = encode_tiles(&f, &[Rect { x: 0, y: 0, w, h }], 3);
        let mut c = vec![0u8; (w * h * 4) as usize];
        apply_tiles(&p, &mut c, w, h).unwrap();
        assert_eq!(&c[(64 + 1) * 4..(64 + 1) * 4 + 4], &[1, 7, 7, 255]);
        assert_eq!(&c[0..4], &[7, 7, 7, 255]);
    }

    #[test]
    fn rejects_garbage() {
        let p = zstd::bulk::compress(&[1, 0, 1, 0, 64, 0, 64, 0, 9], 1).unwrap();
        assert!(decode_tiles(&p, |_, _| {}).is_err());
    }
}
