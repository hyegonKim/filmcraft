//! CABAC arithmetic encoding (9.3.4.3 run backwards, as in the informative encoder description of
//! H.264 9.3.4.x, which H.265 shares) and context initialisation (9.3.2.2).

use crate::spec_tables::{CABAC_INIT, NUM_CTX};

/// Table 9-52 rangeTabLPS[pStateIdx][qCodIRangeIdx].
#[rustfmt::skip]
static RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [95, 116, 137, 158], [90, 110, 130, 150], [85, 104, 123, 142], [81, 99, 117, 135],
    [77, 94, 111, 128], [73, 89, 105, 122], [69, 85, 100, 116], [66, 80, 95, 110],
    [62, 76, 90, 104], [59, 72, 86, 99], [56, 69, 81, 94], [53, 65, 77, 89],
    [51, 62, 73, 85], [48, 59, 69, 80], [46, 56, 66, 76], [43, 53, 63, 72],
    [41, 50, 59, 69], [39, 48, 56, 65], [37, 45, 54, 62], [35, 43, 51, 59],
    [33, 41, 48, 56], [32, 39, 46, 53], [30, 37, 43, 50], [29, 35, 41, 48],
    [27, 33, 39, 45], [26, 31, 37, 43], [24, 30, 35, 41], [23, 28, 33, 39],
    [22, 27, 32, 37], [21, 26, 30, 35], [20, 24, 29, 33], [19, 23, 27, 31],
    [18, 22, 26, 30], [17, 21, 25, 28], [16, 20, 23, 27], [15, 19, 22, 25],
    [14, 18, 21, 24], [14, 17, 20, 23], [13, 16, 19, 22], [12, 15, 18, 21],
    [12, 14, 17, 20], [11, 14, 16, 19], [11, 13, 15, 18], [10, 12, 15, 17],
    [10, 12, 14, 16], [9, 11, 13, 15], [9, 11, 12, 14], [8, 10, 12, 14],
    [8, 9, 11, 13], [7, 9, 11, 12], [7, 9, 10, 12], [7, 8, 10, 11],
    [6, 8, 9, 11], [6, 7, 9, 10], [6, 7, 8, 9], [2, 2, 2, 2],
];

/// Table 9-53 transIdxLPS.
#[rustfmt::skip]
static TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// Context states: `pStateIdx << 1 | valMps`.
pub type Contexts = [u8; NUM_CTX];

/// Initialise every context for a slice of `init_type` (0 I, 1 P, 2 B) at `slice_qp` (9.3.2.2).
pub fn init_contexts(slice_qp: i32, init_type: usize) -> Contexts {
    let mut ctx = [0u8; NUM_CTX];
    let qp = slice_qp.clamp(0, 51);
    let table = CABAC_INIT.get(init_type).unwrap_or(&CABAC_INIT[0]);
    for (c, &v) in ctx.iter_mut().zip(table.iter()) {
        let slope = i32::from(v >> 4);
        let offset = i32::from(v & 15);
        let m = slope * 5 - 45;
        let n = (offset << 3) - 16;
        let pre = (((m * qp) >> 4) + n).clamp(1, 126);
        *c = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
    }
    ctx
}

/// The arithmetic encoder of one slice segment, writing into its own byte buffer (the slice data).
pub struct CabacEncoder {
    low: u32,
    range: u32,
    outstanding: u32,
    first: bool,
    /// Output bits being collected (MSB first) and their count.
    cur: u32,
    ncur: u32,
    out: Vec<u8>,
    pub ctx: Contexts,
}

impl CabacEncoder {
    pub fn new(ctx: Contexts) -> Self {
        CabacEncoder { low: 0, range: 510, outstanding: 0, first: true, cur: 0, ncur: 0, out: Vec::new(), ctx }
    }

    #[inline]
    fn write_bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b & 1);
        self.ncur += 1;
        if self.ncur == 8 {
            self.out.push(self.cur as u8);
            self.cur = 0;
            self.ncur = 0;
        }
    }

    #[inline]
    fn put_bit(&mut self, b: u32) {
        if self.first {
            self.first = false;
        } else {
            self.write_bit(b);
        }
        while self.outstanding > 0 {
            self.write_bit(1 - b);
            self.outstanding -= 1;
        }
    }

    #[inline]
    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    /// One bin with context `ci`.
    #[inline]
    pub fn encode(&mut self, ci: usize, bin: u32) {
        let Some(&st) = self.ctx.get(ci) else { return };
        let (state, mps) = ((st >> 1) as usize, u32::from(st & 1));
        let lps = u32::from(RANGE_TAB_LPS[state][((self.range >> 6) & 3) as usize]);
        self.range -= lps;
        let next = if bin & 1 != mps {
            self.low += self.range;
            self.range = lps;
            let mps = if state == 0 { 1 - mps } else { mps };
            (TRANS_IDX_LPS[state] << 1) | mps as u8
        } else {
            // transIdxMps: one state up, saturating at 62 (63 is reserved for terminate)
            (((state + 1).min(62)) << 1) as u8 | mps as u8
        };
        self.ctx[ci] = next;
        self.renorm();
    }

    /// One equiprobable bin.
    #[inline]
    pub fn bypass(&mut self, bin: u32) {
        self.low <<= 1;
        if bin & 1 != 0 {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.put_bit(1);
            self.low -= 1024;
        } else if self.low < 512 {
            self.put_bit(0);
        } else {
            self.low -= 512;
            self.outstanding += 1;
        }
    }

    /// `n` bypass bins of `v`, most significant first.
    pub fn bypass_bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bypass((v >> i) & 1);
        }
    }

    /// end_of_slice_segment_flag and the like (9.3.4.3.5).
    pub fn terminate(&mut self, bin: u32) {
        self.range -= 2;
        if bin != 0 {
            self.low += self.range;
            self.flush();
        } else {
            self.renorm();
        }
    }

    fn flush(&mut self) {
        self.range = 2;
        self.renorm();
        self.put_bit((self.low >> 9) & 1);
        let v = ((self.low >> 7) & 3) | 1;
        self.write_bit(v >> 1);
        self.write_bit(v & 1);
    }

    /// The slice data after the final terminate bin: the arithmetic code plus
    /// rbsp_slice_segment_trailing_bits (a stop bit and zero alignment).
    pub fn finish(mut self) -> Vec<u8> {
        // the last bit written by `flush` is the rbsp stop bit (1); pad with zeros to a byte
        while self.ncur != 0 {
            self.write_bit(0);
        }
        self.out
    }
}
