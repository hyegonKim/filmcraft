//! Forward transform and quantisation, and the decoder-exact reconstruction of the residual
//! (scaling 8.6.3 + the inverse transform of [`crate::transform`]).

use crate::tables::{DST_MATRIX, LEVEL_SCALE, TRANS_MATRIX};
use crate::transform;

/// Forward quantisation scale per QP % 6 (the inverse of LEVEL_SCALE, 2^20 / levelScale / 16).
const QUANT_SCALE: [i64; 6] = [26214, 23302, 20560, 18396, 16384, 14564];

/// Basis function `k` of the n-point transform, over its first n samples.
#[inline]
fn basis(n: usize, dst: bool, k: usize) -> &'static [i8] {
    if dst { &DST_MATRIX[k][..] } else { &TRANS_MATRIX[k * (32 / n)][..n] }
}

#[inline]
fn dot(b: &[i8], v: &[i32]) -> i32 {
    b.iter().zip(v).map(|(&m, &x)| i32::from(m) * x).sum()
}

/// The 2-D forward transform of an n x n residual block (row-major) into `out`, scaled as the
/// inverse transform expects: first stage shift log2(n) + bitDepth - 9, second log2(n) + 6. Sums
/// fit in i32: |residual| < 2^12, |basis| <= 90, n <= 32 (first stage < 2^24; the second stage
/// input is at most 2^15 after the first shift).
pub fn forward(res: &[i32], n: usize, dst: bool, bd: u32, out: &mut [i32]) {
    let log2 = n.trailing_zeros();
    let shift1 = (log2 + bd).saturating_sub(9);
    let shift2 = log2 + 6;
    let r1 = if shift1 > 0 { 1i32 << (shift1 - 1) } else { 0 };
    let r2 = 1i32 << (shift2 - 1);
    let mut tmp = [0i32; 32 * 32];
    // horizontal: tmp[k][y] (coefficient k of row y)
    for y in 0..n {
        let row = &res[y * n..y * n + n];
        for k in 0..n {
            tmp[k * n + y] = (dot(basis(n, dst, k), row) + r1) >> shift1;
        }
    }
    // vertical
    for k in 0..n {
        let col = &tmp[k * n..k * n + n];
        for l in 0..n {
            out[l * n + k] = ((dot(basis(n, dst, l), col) + r2) >> shift2).clamp(-32768, 32767);
        }
    }
}

/// Quantise transform coefficients at `qp` (QpY or QpC plus the bit-depth offset, as the scaling
/// process uses it) with a dead zone (a third of a step rounds up for intra, a sixth for inter).
/// Returns the number of non-zero levels.
pub fn quantize(coef: &[i32], n: usize, qp: i32, bd: u32, intra: bool, levels: &mut [i32]) -> usize {
    let log2 = n.trailing_zeros() as i32;
    let qp = qp.max(0);
    let transform_shift = 15 - bd as i32 - log2;
    let qbits = 14 + qp / 6 + transform_shift;
    let scale = QUANT_SCALE[(qp % 6) as usize];
    let offset = (if intra { 171i64 } else { 85 }) << (qbits - 9).max(0);
    let mut nz = 0;
    for (l, &c) in levels.iter_mut().zip(coef).take(n * n) {
        let a = ((i64::from(c).abs() * scale + offset) >> qbits).min(32767);
        *l = if c < 0 { -(a as i32) } else { a as i32 };
        nz += usize::from(a != 0);
    }
    nz
}

/// The residual a decoder reconstructs from `levels` (8.6.2 scaling with the flat matrix, then the
/// inverse transform): written to `out`, which must hold n * n values.
pub fn reconstruct(levels: &[i32], n: usize, dst: bool, qp: i32, bd: u32, out: &mut [i32]) {
    let log2 = n.trailing_zeros();
    let bd_shift = bd + log2 - 5;
    let round = 1i64 << (bd_shift - 1);
    let qp = qp.max(0);
    let scale = i64::from(LEVEL_SCALE[(qp % 6) as usize]) << (qp / 6);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    for y in 0..n {
        for x in 0..n {
            let v = levels[y * n + x];
            let d = if v == 0 { 0 } else { ((i64::from(v) * 16 * scale + round) >> bd_shift).clamp(-32768, 32767) as i32 };
            out[y * n + x] = d;
            if v != 0 {
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    transform::inverse_transform(&mut out[..n * n], n, dst, max_x, max_y, 20 - bd);
}
