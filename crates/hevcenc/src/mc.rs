//! Motion compensation exactly as the decoder does it (8.5.3.3.3: 8-tap luma, 4-tap chroma
//! interpolation, default weighted uni-prediction) from edge-padded reference pictures, plus the
//! block metrics the motion search and mode decision use (SAD, Hadamard SATD).

use crate::tables::{CHROMA_FILTER, LUMA_FILTER};

/// Padding of reference luma planes in samples on every side (chroma: half). Motion vectors are kept
/// inside it, so every interpolation tap reads a padded sample (equal to the decoder's clamped one).
pub const PAD: usize = 80;

/// A plane with `pad` replicated samples around a `w` x `h` picture.
#[derive(Clone)]
pub struct Padded {
    pub data: Vec<u16>,
    pub stride: usize,
    pub w: usize,
    pub h: usize,
    pub pad: usize,
}

impl Padded {
    /// Copy a `w` x `h` plane (stride `w`) and replicate its edges by `pad` samples.
    pub fn new(src: &[u16], w: usize, h: usize, pad: usize) -> Padded {
        let stride = w + 2 * pad;
        let mut p = Padded { data: vec![0u16; stride * (h + 2 * pad)], stride, w, h, pad };
        p.refill(src);
        p
    }

    /// Copy a new picture of the same size (stride `w`) in, edges replicated again, reusing the
    /// buffer: the next reference picture without a new allocation.
    pub fn refill(&mut self, src: &[u16]) {
        let Padded { ref mut data, stride, w, h, pad } = *self;
        for y in 0..h + 2 * pad {
            let sy = y.saturating_sub(pad).min(h.saturating_sub(1));
            let row = &src[sy * w..sy * w + w];
            let dst = &mut data[y * stride..(y + 1) * stride];
            let (left, rest) = dst.split_at_mut(pad);
            let (mid, right) = rest.split_at_mut(w);
            left.fill(row[0]);
            mid.copy_from_slice(row);
            right.fill(row[w - 1]);
        }
    }

    /// The sample at picture coordinates (x, y), which must lie within the padding.
    #[inline]
    pub fn at(&self, x: isize, y: isize) -> u16 {
        let (px, py) = ((x + self.pad as isize).max(0) as usize, (y + self.pad as isize).max(0) as usize);
        self.data.get(py * self.stride + px).copied().unwrap_or(0)
    }

    #[inline]
    fn row(&self, x: isize, y: isize, n: usize) -> &[u16] {
        let (px, py) = ((x + self.pad as isize).max(0) as usize, (y + self.pad as isize).max(0) as usize);
        let o = py * self.stride + px;
        self.data.get(o..o + n).unwrap_or(&[])
    }
}

/// Whether a `w` x `h` block at (x, y) displaced by `mv` (in 1 / `frac` samples) keeps every
/// interpolation tap (`taps` before, `taps + 1` after) inside the padding of `p`.
pub fn mv_inside(p: &Padded, (x, y): (i32, i32), (w, h): (usize, usize), mv: [i32; 2], frac_log2: u32, taps: i32) -> bool {
    let pad = p.pad as i32 - 1;
    let (ix, iy) = (x + (mv[0] >> frac_log2), y + (mv[1] >> frac_log2));
    ix - taps >= -pad && iy - taps >= -pad && ix + w as i32 + taps < p.w as i32 + pad && iy + h as i32 + taps < p.h as i32 + pad
}

/// The predicted samples of a `w` x `h` block of component `c` (0 luma) at (x, y) with `mv` (quarter
/// luma samples; chroma uses the same vector as eighth samples) and default uni-prediction weighting,
/// written to `out` (stride `w`).
pub fn predict(p: &Padded, c: usize, (x, y): (i32, i32), mv: [i32; 2], (w, h): (usize, usize), bd: u32, out: &mut [u16]) {
    if c == 0 {
        let f = |k: usize| LUMA_FILTER[k].map(i32::from);
        interpolate::<8>(p, (x, y), mv, 2, (w, h), bd, [f(0), f(1), f(2), f(3), [0; 8], [0; 8], [0; 8], [0; 8]], out);
    } else {
        let f = |k: usize| CHROMA_FILTER[k].map(i32::from);
        interpolate::<4>(p, (x, y), mv, 3, (w, h), bd, [f(0), f(1), f(2), f(3), f(4), f(5), f(6), f(7)], out);
    }
}

/// [`predict`] with an `T`-tap filter bank (`filters[frac]`) and vectors in 1 / 2^`fl` samples.
#[allow(clippy::too_many_arguments)]
fn interpolate<const T: usize>(
    p: &Padded,
    (x, y): (i32, i32),
    mv: [i32; 2],
    fl: u32,
    (w, h): (usize, usize),
    bd: u32,
    filters: [[i32; T]; 8],
    out: &mut [u16],
) {
    let mask = (1i32 << fl) - 1;
    let (fx, fy) = ((mv[0] & mask) as usize, (mv[1] & mask) as usize);
    let (ix, iy) = ((x + (mv[0] >> fl)) as isize, (y + (mv[1] >> fl)) as isize);
    let shift1 = bd.min(12) - 8;
    let shift3 = 14 - bd;
    let fshift = 14 - bd;
    let foff = 1i32 << (fshift - 1);
    let max = (1i32 << bd) - 1;
    let back = (T / 2 - 1) as isize;
    let put = |v: i32| -> u16 { ((v + foff) >> fshift).clamp(0, max) as u16 };
    let (hf, vf) = (filters[fx], filters[fy]);
    let tap = |row: &[u16], i: usize, f: &[i32; T]| -> i32 {
        let Some(s) = row.get(i..i + T) else { return 0 };
        let mut acc = 0;
        for k in 0..T {
            acc += f[k] * i32::from(s[k]);
        }
        acc
    };
    if fx == 0 && fy == 0 {
        for r in 0..h {
            let row = p.row(ix, iy + r as isize, w);
            for (o, &s) in out[r * w..r * w + w].iter_mut().zip(row) {
                *o = put(i32::from(s) << shift3);
            }
        }
        return;
    }
    if fy == 0 {
        for r in 0..h {
            let row = p.row(ix - back, iy + r as isize, w + T);
            for (i, o) in out[r * w..r * w + w].iter_mut().enumerate() {
                *o = put(tap(row, i, &hf) >> shift1);
            }
        }
        return;
    }
    // the rows the vertical filter reads (horizontally filtered unless fx == 0)
    let rows = h + T - 1;
    let mut tmp = vec![0i32; rows * w];
    for r in 0..rows {
        let row = p.row(ix - back, iy - back + r as isize, w + T);
        let t = &mut tmp[r * w..r * w + w];
        if fx == 0 {
            for (i, v) in t.iter_mut().enumerate() {
                *v = i32::from(row.get(i + back as usize).copied().unwrap_or(0));
            }
        } else {
            for (i, v) in t.iter_mut().enumerate() {
                *v = tap(row, i, &hf) >> shift1;
            }
        }
    }
    // vertical: (sum f * tmp) >> shift1 for full-sample columns (8-230), >> 6 after the horizontal pass
    let vshift = if fx == 0 { shift1 } else { 6 };
    for r in 0..h {
        for (i, o) in out[r * w..r * w + w].iter_mut().enumerate() {
            let mut acc = 0;
            for k in 0..T {
                acc += vf[k] * tmp[(r + k) * w + i];
            }
            *o = put(acc >> vshift);
        }
    }
}

/// Sum of absolute differences between `a` (stride `sa`) and `b` (stride `sb`) over w x h.
pub fn sad(a: &[u16], sa: usize, b: &[u16], sb: usize, w: usize, h: usize) -> u32 {
    let mut s = 0u32;
    for r in 0..h {
        let (ra, rb) = (&a[r * sa..r * sa + w], &b[r * sb..r * sb + w]);
        s += ra.iter().zip(rb).map(|(&x, &y)| u32::from(x.abs_diff(y))).sum::<u32>();
    }
    s
}

/// Hadamard SATD of the difference of two blocks (4x4 transforms; w and h multiples of 4).
pub fn satd(a: &[u16], sa: usize, b: &[u16], sb: usize, w: usize, h: usize) -> u32 {
    let mut total = 0u32;
    for by in (0..h).step_by(4) {
        for bx in (0..w).step_by(4) {
            let mut d = [0i32; 16];
            for r in 0..4 {
                for k in 0..4 {
                    d[r * 4 + k] = i32::from(a[(by + r) * sa + bx + k]) - i32::from(b[(by + r) * sb + bx + k]);
                }
            }
            total += hadamard4(&d);
        }
    }
    total
}

fn hadamard4(d: &[i32; 16]) -> u32 {
    let mut m = [0i32; 16];
    for r in 0..4 {
        let (a, b, c, e) = (d[r * 4], d[r * 4 + 1], d[r * 4 + 2], d[r * 4 + 3]);
        let (s0, s1, d0, d1) = (a + b, c + e, a - b, c - e);
        m[r * 4] = s0 + s1;
        m[r * 4 + 1] = s0 - s1;
        m[r * 4 + 2] = d0 + d1;
        m[r * 4 + 3] = d0 - d1;
    }
    let mut sum = 0u32;
    for k in 0..4 {
        let (a, b, c, e) = (m[k], m[4 + k], m[8 + k], m[12 + k]);
        let (s0, s1, d0, d1) = (a + b, c + e, a - b, c - e);
        sum += (s0 + s1).unsigned_abs() + (s0 - s1).unsigned_abs() + (d0 + d1).unsigned_abs() + (d0 - d1).unsigned_abs();
    }
    (sum + 1) >> 1
}
