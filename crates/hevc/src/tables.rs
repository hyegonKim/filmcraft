//! Derived tables: scan orders (6.5.3 - 6.5.5), chroma QP mapping and small helper tables.

pub use crate::spec_tables::*;

/// Up-right diagonal scan (6.5.3) of a `N x N` block as (x, y) pairs.
const fn diag<const L: usize>(n: usize) -> [(u8, u8); L] {
    let mut out = [(0u8, 0u8); L];
    let mut i = 0;
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    while i < L {
        while y >= 0 {
            if (x as usize) < n && (y as usize) < n {
                out[i] = (x as u8, y as u8);
                i += 1;
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

/// Horizontal (6.5.4, `vertical == false`) or vertical (6.5.5) scan.
const fn hv<const L: usize>(n: usize, vertical: bool) -> [(u8, u8); L] {
    let mut out = [(0u8, 0u8); L];
    let mut i = 0;
    while i < L {
        let (a, b) = ((i % n) as u8, (i / n) as u8);
        out[i] = if vertical { (b, a) } else { (a, b) };
        i += 1;
    }
    out
}

pub static SCAN_1: [[(u8, u8); 1]; 3] = [[(0, 0)], [(0, 0)], [(0, 0)]];
pub static SCAN_2: [[(u8, u8); 4]; 3] = [diag::<4>(2), hv::<4>(2, false), hv::<4>(2, true)];
pub static SCAN_4: [[(u8, u8); 16]; 3] = [diag::<16>(4), hv::<16>(4, false), hv::<16>(4, true)];
pub static SCAN_8: [[(u8, u8); 64]; 3] = [diag::<64>(8), hv::<64>(8, false), hv::<64>(8, true)];

/// ScanOrder[log2BlockSize][scanIdx] (log2BlockSize 0..=3).
#[inline]
pub fn scan_order(log2: u32, scan_idx: usize) -> &'static [(u8, u8)] {
    match log2 {
        0 => &SCAN_1[scan_idx],
        1 => &SCAN_2[scan_idx],
        2 => &SCAN_4[scan_idx],
        _ => &SCAN_8[scan_idx],
    }
}

/// Up-right diagonal scan for log2 size 2 or 3.
pub fn scan_diag(log2: u32) -> &'static [(u8, u8)] {
    scan_order(log2, 0)
}

/// Table 8-10: QpC as a function of qPi (ChromaArrayType 1).
#[inline]
pub fn qpc_420(qpi: i32) -> i32 {
    if qpi < 30 {
        qpi
    } else if qpi > 43 {
        qpi - 6
    } else {
        QPC_30_43[(qpi - 30) as usize] as i32
    }
}

/// QpC for a ChromaArrayType (8.6.1, 8.7.2.5.5): Table 8-10 for 4:2:0, `Min(qPi, 51)` otherwise.
#[inline]
pub fn qpc(qpi: i32, chroma_array_type: u32) -> i32 {
    if chroma_array_type == 1 { qpc_420(qpi) } else { qpi.min(51) }
}

/// Table 8-3: the 4:2:2 mapping of the chroma intra prediction mode (modeIdc to IntraPredModeC),
/// which compensates for chroma being stretched vertically against luma.
pub const INTRA_MODE_422: [u8; 35] =
    [0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24, 25, 25, 26, 27, 27, 28, 28, 29, 29, 30, 31];

/// Table 9-50 ctxIdxMap for 4x4 sig_coeff_flag contexts.
pub static CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// levelScale (8-309).
pub static LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

/// DST-VII 4x4 matrix (8.6.4.2, trType 1).
pub static DST_MATRIX: [[i8; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_scan_4x4() {
        let s = &SCAN_4[0];
        assert_eq!(&s[..6], &[(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(s[15], (3, 3));
        assert_eq!(SCAN_2[0], [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(SCAN_4[2][1], (0, 1));
        assert_eq!(SCAN_4[1][1], (1, 0));
    }

    #[test]
    fn dct_matrix_properties() {
        // rows are nearly orthogonal with norm ~ 64^2 * 32
        for a in 0..32 {
            for b in 0..32 {
                let d: i64 = (0..32).map(|k| TRANS_MATRIX[a][k] as i64 * TRANS_MATRIX[b][k] as i64).sum();
                if a == b {
                    assert!((d - 131072).abs() < 1000, "row {a} norm {d}");
                } else {
                    assert!(d.abs() < 1000, "rows {a},{b}: {d}");
                }
            }
        }
        // even basis functions are symmetric, odd ones antisymmetric
        for n in 0..32 {
            for m in 0..16 {
                let (a, b) = (TRANS_MATRIX[n][m] as i32, TRANS_MATRIX[n][31 - m] as i32);
                assert_eq!(a, if n % 2 == 0 { b } else { -b }, "{n} {m}");
            }
        }
    }

    /// 4:2:0 maps qPi through Table 8-10; 4:2:2 (any ChromaArrayType but 1) caps it at 51.
    #[test]
    fn qpc_for_each_chroma_layout() {
        for qpi in -12..=57 {
            assert_eq!(qpc(qpi, 1), qpc_420(qpi), "{qpi}");
            assert_eq!(qpc(qpi, 2), qpi.min(51), "{qpi}");
        }
    }

    /// The 4:2:2 chroma intra mode mapping follows the angles: chroma is halved across, so a
    /// horizontal-family angle doubles and a vertical-family one halves, then rounds to the nearest
    /// mode (a near-diagonal horizontal angle becomes a vertical-family one). Entries 11 and 14
    /// (12 and 17) were measured against ffmpeg on x265 4:2:2 streams: a block predicted with 16
    /// instead of 17 put whole chroma regions off while luma stayed bit-exact.
    #[test]
    fn intra_mode_422_follows_the_angles() {
        // intraPredAngle of modes 2..=34
        const ANGLE: [i32; 33] =
            [32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32];
        assert_eq!((INTRA_MODE_422[0], INTRA_MODE_422[1]), (0, 1), "planar and DC stay");
        assert_eq!((INTRA_MODE_422[11], INTRA_MODE_422[14]), (12, 17));
        let angle = |m: u8| ANGLE[m as usize - 2];
        for x in 2..=34usize {
            let (a, m) = (ANGLE[x - 2], INTRA_MODE_422[x]);
            if x <= 17 {
                // horizontal: (32, a) becomes (16, a)
                if 2 * a.abs() <= 32 {
                    assert!((2..=17).contains(&m) && (angle(m) - 2 * a).abs() <= 2, "{x} -> {m}");
                } else if a > 0 {
                    assert_eq!(m, 2, "{x}");
                } else {
                    // past the diagonal: a vertical-family angle of -512 / |a|
                    let v = 512.0 / f64::from(a.abs());
                    assert!((18..=21).contains(&m) && (f64::from(-angle(m)) - v).abs() < 4.5, "{x} -> {m}");
                }
            } else {
                // vertical: (a, 32) becomes (a / 2, 32)
                assert!((18..=34).contains(&m) && (f64::from(angle(m)) - f64::from(a) / 2.0).abs() <= 2.5, "{x} -> {m}");
            }
        }
    }

    #[test]
    fn qpc_table() {
        assert_eq!(qpc_420(29), 29);
        assert_eq!(qpc_420(30), 29);
        assert_eq!(qpc_420(43), 37);
        assert_eq!(qpc_420(51), 45);
    }
}
