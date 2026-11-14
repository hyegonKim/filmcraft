//! One slice: CTU mode decision and coding (coding quadtree, intra / skip / merge / AMVP inter
//! coding units with one transform unit each), residual coding, and the reconstruction a decoder
//! makes, which later coding units predict from. Slices are bands of whole CTU rows, coded
//! independently (and in parallel).

use crate::cabac::{CabacEncoder, Contexts};
use crate::intra::{self, Refs};
use crate::mc::{self, Padded};
use crate::quant;
use crate::spec_tables::{
    ABS_MVD_GT0, ABS_MVD_GT1, CBF_CHROMA, CBF_LUMA, CODED_SUB_BLOCK, CU_SKIP, GT1, GT2, INTRA_CHROMA, LAST_X_PREFIX, LAST_Y_PREFIX, MERGE_FLAG, MVP_FLAG,
    PART_MODE, PRED_MODE, PREV_INTRA_LUMA, RQT_ROOT_CBF, SIG_COEFF, SPLIT_CU,
};
use crate::tables::{CTX_IDX_MAP, SCAN_4, qpc_420, scan_order};

/// CTU size (log2) and the minimum coding block (8x8).
pub const LOG2_CTB: u32 = 5;
const MIN_CB_LOG2: u32 = 3;
/// Intra modes tried in mode decision: planar, DC, the axes, the diagonals and four in between.
const INTRA_MODES: [u32; 11] = [0, 1, 10, 26, 2, 18, 34, 6, 14, 22, 30];
/// Integer motion search range around each start point (luma samples).
const SEARCH_RANGE: i32 = 48;

/// Motion and mode of each 4x4 luma block of the picture being coded.
#[derive(Clone, Copy, Default)]
pub struct Blk {
    pub intra: bool,
    pub skip: bool,
    pub depth: u8,
    pub ipm: u8,
    pub mv: [i16; 2],
}

/// Read-only state shared by every slice of a picture.
pub struct Shared<'a> {
    /// Source planes at the coded size (luma, Cb, Cr; strides = widths).
    pub src: [&'a [u16]; 3],
    /// The reference picture (P slices).
    pub refp: Option<&'a [Padded; 3]>,
    pub coded_w: usize,
    pub coded_h: usize,
    pub bd: u32,
    pub qp: i32,
    pub intra: bool,
}

/// The rows of the picture one slice owns, mutable: luma rows `y0..y0 + h` (chroma halved) and
/// their 4x4 block records.
pub struct Band<'a> {
    pub y0: usize,
    pub h: usize,
    pub rec: [&'a mut [u16]; 3],
    pub blk: &'a mut [Blk],
}

/// A decided coding unit.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Intra(u32),
    /// Merge with the one merge candidate (coded as skip when no residual remains).
    Merge,
    /// AMVP: the motion vector and the predictor index.
    Inter([i32; 2], usize),
}

enum Node {
    Split(Box<[Option<Node>; 4]>),
    Cu(Mode),
}

pub struct SliceEncoder<'s, 'a> {
    sh: &'s Shared<'a>,
    band: Band<'s>,
    cab: CabacEncoder,
    lambda: f64,
    qp_c: i32,
    /// Scratch.
    pred: Vec<u16>,
    resid: Vec<i32>,
    coef: Vec<i32>,
    lev: Vec<i32>,
    recon: Vec<i32>,
    refs: Refs,
}

#[inline]
fn zorder(x: i32, y: i32) -> u32 {
    let (mut ux, mut uy) = (((x >> 2) & 7) as u32, ((y >> 2) & 7) as u32);
    let mut z = 0;
    for bit in 0..3 {
        z |= ((ux & 1) << (2 * bit)) | ((uy & 1) << (2 * bit + 1));
        ux >>= 1;
        uy >>= 1;
    }
    z
}

impl<'s, 'a> SliceEncoder<'s, 'a> {
    pub fn new(sh: &'s Shared<'a>, band: Band<'s>, ctx: Contexts) -> Self {
        let qp = sh.qp;
        let lambda_sse = 0.57 * 2f64.powf(f64::from(qp - 12) / 3.0);
        let lambda = lambda_sse.sqrt() * f64::from(1u32 << (sh.bd - 8));
        let off = 6 * (sh.bd as i32 - 8);
        let qpi = (qp).clamp(-off, 57);
        let qp_c = qpc_420(qpi) + off;
        SliceEncoder {
            sh,
            band,
            cab: CabacEncoder::new(ctx),
            lambda,
            qp_c,
            pred: vec![0; 32 * 32],
            resid: vec![0; 32 * 32],
            coef: vec![0; 32 * 32],
            lev: vec![0; 32 * 32],
            recon: vec![0; 32 * 32],
            refs: Refs::default(),
        }
    }

    // ------------------------------------------------------------------------------------------
    // geometry and availability
    // ------------------------------------------------------------------------------------------

    fn w4(&self) -> usize {
        self.sh.coded_w.div_ceil(4)
    }

    /// z-scan availability (6.4.1) of luma location (xn, yn) for the block at (xc, yc): inside the
    /// picture and this slice, and coded before it.
    fn avail(&self, (xc, yc): (i32, i32), (xn, yn): (i32, i32)) -> bool {
        if xn < 0 || yn < 0 || xn as usize >= self.sh.coded_w || yn as usize >= self.sh.coded_h {
            return false;
        }
        let y0 = self.band.y0 as i32;
        if yn < y0 || yn >= y0 + self.band.h as i32 {
            return false;
        }
        let s = LOG2_CTB;
        let (cxc, cyc, cxn, cyn) = (xc >> s, yc >> s, xn >> s, yn >> s);
        if cxc == cxn && cyc == cyn {
            return zorder(xn, yn) <= zorder(xc, yc);
        }
        (cyn, cxn) < (cyc, cxc)
    }

    fn blk(&self, x: i32, y: i32) -> Blk {
        let by = (y as usize - self.band.y0) >> 2;
        self.band.blk.get(by * self.w4() + (x as usize >> 2)).copied().unwrap_or_default()
    }

    fn set_blk(&mut self, (x, y): (i32, i32), n: usize, b: Blk) {
        let w4 = self.w4();
        let (bx, by) = (x as usize >> 2, (y as usize - self.band.y0) >> 2);
        for r in by..by + n / 4 {
            if let Some(row) = self.band.blk.get_mut(r * w4 + bx..r * w4 + bx + n / 4) {
                row.fill(b);
            }
        }
    }

    /// Plane geometry of component `c`: (width, stride of the band, band origin row).
    fn plane_geo(&self, c: usize) -> (usize, usize) {
        if c == 0 { (self.sh.coded_w, self.band.y0) } else { (self.sh.coded_w / 2, self.band.y0 / 2) }
    }

    fn rec_at(&self, c: usize, x: usize, y: usize) -> u16 {
        let (w, oy) = self.plane_geo(c);
        self.band.rec[c].get((y - oy) * w + x).copied().unwrap_or(0)
    }

    fn src_at(&self, c: usize, x: usize, y: usize) -> u16 {
        let w = if c == 0 { self.sh.coded_w } else { self.sh.coded_w / 2 };
        self.sh.src[c].get(y * w + x).copied().unwrap_or(0)
    }

    // ------------------------------------------------------------------------------------------
    // prediction
    // ------------------------------------------------------------------------------------------

    /// Intra prediction of an n x n block of component `c` at component position (x0, y0) into
    /// `self.pred` (stride n), from the reconstruction (`from_rec`) or the source (mode decision).
    fn intra_predict(&mut self, c: usize, (x0, y0): (usize, usize), log2: u32, mode: u32, from_rec: bool) {
        self.intra_refs(c, (x0, y0), log2, from_rec);
        self.intra_from_refs(c, log2, mode);
    }

    /// The reference samples of an n x n block (8.4.4.2.2: gathered and substituted) into
    /// `self.refs`, the same for every mode.
    fn intra_refs(&mut self, c: usize, (x0, y0): (usize, usize), log2: u32, from_rec: bool) {
        let n = 1usize << log2;
        let sc = if c == 0 { 1 } else { 2 };
        let (xl, yl) = ((x0 * sc) as i32, (y0 * sc) as i32);
        let mut al = [false; 129];
        let mut at = [false; 129];
        let get = |s: &Self, x: usize, y: usize| if from_rec { s.rec_at(c, x, y) } else { s.src_at(c, x, y) };
        let mut r = std::mem::take(&mut self.refs);
        if x0 > 0 && y0 > 0 && self.avail((xl, yl), (xl - sc as i32, yl - sc as i32)) {
            al[0] = true;
            at[0] = true;
            let v = get(self, x0 - 1, y0 - 1);
            r.left[0] = v;
            r.top[0] = v;
        }
        for k in 0..2 * n {
            if x0 > 0 && self.avail((xl, yl), (xl - sc as i32, yl + (k * sc) as i32)) {
                al[1 + k] = true;
                r.left[1 + k] = get(self, x0 - 1, y0 + k);
            }
            if y0 > 0 && self.avail((xl, yl), (xl + (k * sc) as i32, yl - sc as i32)) {
                at[1 + k] = true;
                r.top[1 + k] = get(self, x0 + k, y0 - 1);
            }
        }
        intra::substitute(&mut r, &al, &at, n, self.sh.bd);
        self.refs = r;
    }

    /// Predict an n x n block of component `c` with `mode` from `self.refs` into `self.pred`
    /// (luma references filtered as the mode requires, 8.4.4.2.3).
    fn intra_from_refs(&mut self, c: usize, log2: u32, mode: u32) {
        let n = 1usize << log2;
        let bd = self.sh.bd;
        if c == 0 {
            let mut r = self.refs.clone();
            intra::filter(&mut r, mode, n, true, true, bd);
            intra::predict(&r, mode, n, true, bd, &mut self.pred[..n * n], n);
        } else {
            intra::predict(&self.refs, mode, n, false, bd, &mut self.pred[..n * n], n);
        }
    }

    // ------------------------------------------------------------------------------------------
    // motion vector prediction (one reference picture, list 0)
    // ------------------------------------------------------------------------------------------

    /// The motion of an available inter-coded neighbour (6.4.2), if any.
    fn nb_mv(&self, cur: (i32, i32), nb: (i32, i32)) -> Option<[i32; 2]> {
        if !self.avail(cur, nb) {
            return None;
        }
        let b = self.blk(nb.0, nb.1);
        if b.intra { None } else { Some([i32::from(b.mv[0]), i32::from(b.mv[1])]) }
    }

    /// The single merge candidate (MaxNumMergeCand = 1): the first available of A1, B1, B0, A0,
    /// B2, else the zero vector (8.5.3.2.2 - 8.5.3.2.5).
    fn merge_cand(&self, (x, y): (i32, i32), n: i32) -> [i32; 2] {
        let cur = (x, y);
        let a1 = self.nb_mv(cur, (x - 1, y + n - 1));
        let b1 = self.nb_mv(cur, (x + n - 1, y - 1));
        let b0 = self.nb_mv(cur, (x + n, y - 1));
        let a0 = self.nb_mv(cur, (x - 1, y + n));
        let four = [a1, b1, b0, a0].iter().filter(|m| m.is_some()).count() == 4;
        let b2 = if four { None } else { self.nb_mv(cur, (x - 1, y - 1)) };
        a1.or(b1).or(b0).or(a0).or(b2).unwrap_or([0, 0])
    }

    /// The two AMVP predictors (8.5.3.2.6 / 8.5.3.2.7) for one short-term reference.
    fn amvp(&self, (x, y): (i32, i32), n: i32) -> [[i32; 2]; 2] {
        let cur = (x, y);
        let a0 = self.nb_mv(cur, (x - 1, y + n));
        let a1 = self.nb_mv(cur, (x - 1, y + n - 1));
        let is_scaled = a0.is_some() || a1.is_some();
        let mut a = a0.or(a1);
        let b0 = self.nb_mv(cur, (x + n, y - 1));
        let b1 = self.nb_mv(cur, (x + n - 1, y - 1));
        let b2 = self.nb_mv(cur, (x - 1, y - 1));
        let mut b = b0.or(b1).or(b2);
        if !is_scaled && b.is_some() {
            a = b;
        }
        if !is_scaled {
            b = b0.or(b1).or(b2);
        }
        let mut list = [[0i32; 2]; 2];
        let mut k = 0;
        if let Some(a) = a {
            list[k] = a;
            k += 1;
            if let Some(b) = b
                && b != a
            {
                list[k] = b;
            }
        } else if let Some(b) = b {
            list[k] = b;
        }
        list
    }

    // ------------------------------------------------------------------------------------------
    // mode decision (open loop: intra from the source, inter from the reference reconstruction)
    // ------------------------------------------------------------------------------------------

    fn luma_src_block(&self, x: usize, y: usize, n: usize, out: &mut [u16]) {
        let w = self.sh.coded_w;
        for r in 0..n {
            out[r * n..r * n + n].copy_from_slice(&self.sh.src[0][(y + r) * w + x..(y + r) * w + x + n]);
        }
    }

    fn mv_bits(&self, mv: [i32; 2], mvp: [i32; 2]) -> f64 {
        let bits = |d: i32| -> f64 {
            let a = d.unsigned_abs();
            if a == 0 { 1.0 } else { 2.0 + 2.0 * f64::from(32 - a.leading_zeros()) }
        };
        bits(mv[0] - mvp[0]) + bits(mv[1] - mvp[1])
    }

    /// The best intra mode of an n x n luma block and its cost.
    fn decide_intra(&mut self, (x, y): (usize, usize), log2: u32, src: &[u16]) -> (u32, f64) {
        let n = 1usize << log2;
        let mut best = (1u32, f64::MAX);
        self.intra_refs(0, (x, y), log2, false);
        for &m in &INTRA_MODES {
            self.intra_from_refs(0, log2, m);
            let d = f64::from(mc::satd(src, n, &self.pred, n, n, n));
            let cost = d + self.lambda * if m <= 1 { 3.0 } else { 6.0 };
            if cost < best.1 {
                best = (m, cost);
            }
        }
        best
    }

    /// Motion search for an n x n block: integer hexagon search from the predictors, then half- and
    /// quarter-sample refinement. Returns (mv, mvp index, cost).
    fn decide_inter(&mut self, refp: &[Padded; 3], (x, y): (usize, usize), n: usize, src: &[u16]) -> Option<([i32; 2], usize, f64)> {
        let (xi, yi) = (x as i32, y as i32);
        let mvps = self.amvp((xi, yi), n as i32);
        let luma = &refp[0];
        let int_cost = |s: &Self, mv: [i32; 2]| -> Option<f64> {
            if !mc::mv_inside(luma, (xi, yi), (n, n), mv, 2, 4) {
                return None;
            }
            let (rx, ry) = ((xi + (mv[0] >> 2)) as isize, (yi + (mv[1] >> 2)) as isize);
            let o = ((ry + luma.pad as isize) as usize) * luma.stride + (rx + luma.pad as isize) as usize;
            let d = mc::sad(src, n, luma.data.get(o..)?, luma.stride, n, n);
            let bits = s.mv_bits(mv, mvps[0]).min(s.mv_bits(mv, mvps[1]));
            Some(f64::from(d) + s.lambda * bits)
        };
        // start points: both predictors and zero, rounded to whole samples
        let mut best: Option<([i32; 2], f64)> = None;
        for start in [mvps[0], mvps[1], [0, 0]] {
            let s = [start[0] & !3, start[1] & !3];
            if let Some(c) = int_cost(self, s)
                && best.is_none_or(|b| c < b.1)
            {
                best = Some((s, c));
            }
        }
        let (mut bmv, mut bcost) = best?;
        let center = bmv;
        const HEX: [[i32; 2]; 6] = [[-2, 0], [-1, 2], [1, 2], [2, 0], [1, -2], [-1, -2]];
        for _ in 0..SEARCH_RANGE {
            let mut moved = false;
            for d in HEX {
                let cand = [bmv[0] + d[0] * 4, bmv[1] + d[1] * 4];
                if (cand[0] - center[0]).abs() > SEARCH_RANGE * 4 || (cand[1] - center[1]).abs() > SEARCH_RANGE * 4 {
                    continue;
                }
                if let Some(c) = int_cost(self, cand)
                    && c < bcost
                {
                    (bmv, bcost, moved) = (cand, c, true);
                }
            }
            if !moved {
                break;
            }
        }
        for d in [[-1, 0], [1, 0], [0, -1], [0, 1], [-1, -1], [1, -1], [-1, 1], [1, 1]] {
            let cand = [bmv[0] + d[0] * 4, bmv[1] + d[1] * 4];
            if let Some(c) = int_cost(self, cand)
                && c < bcost
            {
                (bmv, bcost) = (cand, c);
            }
        }
        // sub-sample refinement with SATD of the interpolated prediction
        let sub_cost = |s: &mut Self, mv: [i32; 2]| -> Option<f64> {
            if !mc::mv_inside(luma, (xi, yi), (n, n), mv, 2, 4) {
                return None;
            }
            mc::predict(luma, 0, (xi, yi), mv, (n, n), s.sh.bd, &mut s.pred[..n * n]);
            let d = mc::satd(src, n, &s.pred, n, n, n);
            let bits = s.mv_bits(mv, mvps[0]).min(s.mv_bits(mv, mvps[1]));
            Some(f64::from(d) + s.lambda * bits)
        };
        let mut bsub = sub_cost(self, bmv)?;
        for step in [2, 1] {
            let base = bmv;
            for d in [[-1, 0], [1, 0], [0, -1], [0, 1], [-1, -1], [1, -1], [-1, 1], [1, 1]] {
                let cand = [base[0] + d[0] * step, base[1] + d[1] * step];
                if let Some(c) = sub_cost(self, cand)
                    && c < bsub
                {
                    (bmv, bsub) = (cand, c);
                }
            }
        }
        let idx = usize::from(self.mv_bits(bmv, mvps[1]) < self.mv_bits(bmv, mvps[0]));
        Some((bmv, idx, bsub + self.lambda * 3.0))
    }

    /// Cost of predicting an n x n luma block with merge.
    fn merge_cost(&mut self, refp: &[Padded; 3], (x, y): (usize, usize), n: usize, src: &[u16]) -> Option<f64> {
        let mv = self.merge_cand((x as i32, y as i32), n as i32);
        if !mc::mv_inside(&refp[0], (x as i32, y as i32), (n, n), mv, 2, 4) || !mc::mv_inside(&refp[1], (x as i32 / 2, y as i32 / 2), (n / 2, n / 2), mv, 3, 2)
        {
            return None;
        }
        mc::predict(&refp[0], 0, (x as i32, y as i32), mv, (n, n), self.sh.bd, &mut self.pred[..n * n]);
        Some(f64::from(mc::satd(src, n, &self.pred, n, n, n)) + self.lambda * 1.5)
    }

    /// Decide the coding quadtree of the CU at (x, y) of size 2^log2 (crossing the picture edge:
    /// split; entirely outside: None).
    fn decide(&mut self, (x, y): (usize, usize), log2: u32) -> Option<(Node, f64)> {
        let n = 1usize << log2;
        if x >= self.sh.coded_w || y >= self.sh.coded_h {
            return None;
        }
        let fits = x + n <= self.sh.coded_w && y + n <= self.sh.coded_h;
        let mut whole: Option<(Mode, f64)> = None;
        if fits {
            let mut src = vec![0u16; n * n];
            self.luma_src_block(x, y, n, &mut src);
            let (m, c) = self.decide_intra((x, y), log2, &src);
            whole = Some((Mode::Intra(m), c));
            if let (false, Some(refp)) = (self.sh.intra, self.sh.refp) {
                if let Some(c) = self.merge_cost(refp, (x, y), n, &src)
                    && whole.as_ref().is_none_or(|w| c < w.1)
                {
                    whole = Some((Mode::Merge, c));
                }
                if let Some((mv, idx, c)) = self.decide_inter(refp, (x, y), n, &src)
                    && mv_inside_chroma(refp, (x, y), n, mv)
                    && whole.as_ref().is_none_or(|w| c < w.1)
                {
                    whole = Some((Mode::Inter(mv, idx), c));
                }
            }
        }
        if log2 > MIN_CB_LOG2 && (!fits || whole.as_ref().is_some_and(|w| w.1 > self.lambda * 8.0)) {
            let h = n / 2;
            let mut kids: [Option<Node>; 4] = [None, None, None, None];
            let mut total = self.lambda * 1.0;
            for (k, (dx, dy)) in [(0, 0), (h, 0), (0, h), (h, h)].into_iter().enumerate() {
                if let Some((node, c)) = self.decide((x + dx, y + dy), log2 - 1) {
                    kids[k] = Some(node);
                    total += c;
                }
            }
            if !fits || whole.as_ref().is_none_or(|w| total < w.1) {
                return Some((Node::Split(Box::new(kids)), total));
            }
        }
        whole.map(|(m, c)| (Node::Cu(m), c))
    }

    // ------------------------------------------------------------------------------------------
    // coding (closed loop) and syntax
    // ------------------------------------------------------------------------------------------

    pub fn encode_ctu(&mut self, (cx, cy): (usize, usize)) {
        let (x, y) = (cx << LOG2_CTB, cy << LOG2_CTB);
        if let Some((node, _)) = self.decide((x, y), LOG2_CTB) {
            self.code_node((x, y), LOG2_CTB, 0, &node);
        }
    }

    fn code_node(&mut self, (x, y): (usize, usize), log2: u32, depth: u8, node: &Node) {
        let n = 1usize << log2;
        if x >= self.sh.coded_w || y >= self.sh.coded_h {
            return;
        }
        let fits = x + n <= self.sh.coded_w && y + n <= self.sh.coded_h;
        let split = matches!(node, Node::Split(_));
        if log2 > MIN_CB_LOG2 && fits {
            // split_cu_flag: context from the depths of the left and above neighbours
            let cur = (x as i32, y as i32);
            let mut inc = 0;
            if self.avail(cur, (x as i32 - 1, y as i32)) && self.blk(x as i32 - 1, y as i32).depth > depth {
                inc += 1;
            }
            if self.avail(cur, (x as i32, y as i32 - 1)) && self.blk(x as i32, y as i32 - 1).depth > depth {
                inc += 1;
            }
            self.cab.encode(SPLIT_CU + inc, u32::from(split));
        }
        match node {
            Node::Split(kids) => {
                let h = n / 2;
                for (k, (dx, dy)) in [(0, 0), (h, 0), (0, h), (h, h)].into_iter().enumerate() {
                    if let Some(kid) = &kids[k] {
                        self.code_node((x + dx, y + dy), log2 - 1, depth + 1, kid);
                    }
                }
            }
            Node::Cu(mode) => self.code_cu((x, y), log2, depth, *mode),
        }
    }

    fn code_cu(&mut self, (x, y): (usize, usize), log2: u32, depth: u8, mode: Mode) {
        let n = 1usize << log2;
        let cur = (x as i32, y as i32);
        let refp = self.sh.refp;
        // inter prediction (merge / AMVP) of all three planes, and whether a residual remains
        let (mut mv, mut mvp_idx, mut merge, intra_mode) = match mode {
            Mode::Intra(m) => ([0, 0], 0, false, Some(m)),
            Mode::Merge => (self.merge_cand(cur, n as i32), 0, true, None),
            Mode::Inter(mv, idx) => (mv, idx, false, None),
        };
        let mut intra_mode = intra_mode;
        let refp = match (intra_mode, refp) {
            (None, Some(r)) => Some(r),
            _ => {
                intra_mode = intra_mode.or(Some(1));
                None
            }
        };
        if let Some(r) = refp
            && (!mc::mv_inside(&r[0], cur, (n, n), mv, 2, 4) || !mv_inside_chroma(r, (x, y), n, mv))
        {
            // a merge candidate (from the neighbours as finally coded) reaching outside the padded
            // reference: code the zero vector explicitly instead, which is always inside
            mv = [0, 0];
            mvp_idx = 0;
            merge = false;
        }
        let mut res = ResidualCu::default();
        if let Some(r) = refp {
            self.inter_residuals(r, (x, y), n, mv, &mut res);
        }
        let skip = refp.is_some() && merge && !res.any();
        if !self.sh.intra {
            let mut inc = 0;
            if self.avail(cur, (x as i32 - 1, y as i32)) && self.blk(x as i32 - 1, y as i32).skip {
                inc += 1;
            }
            if self.avail(cur, (x as i32, y as i32 - 1)) && self.blk(x as i32, y as i32 - 1).skip {
                inc += 1;
            }
            self.cab.encode(CU_SKIP + inc, u32::from(skip));
        }
        if skip {
            self.write_rec_inter(&res, (x, y), n);
            self.set_blk(cur, n, Blk { intra: false, skip: true, depth, ipm: 1, mv: [mv[0] as i16, mv[1] as i16] });
            return;
        }
        if !self.sh.intra {
            self.cab.encode(PRED_MODE, u32::from(intra_mode.is_some()));
        }
        if let Some(m) = intra_mode {
            if log2 == MIN_CB_LOG2 {
                self.cab.encode(PART_MODE, 1); // PART_2Nx2N
            }
            self.code_intra_mode(cur, m);
            self.cab.encode(INTRA_CHROMA, 0); // intra_chroma_pred_mode 4: chroma follows luma
            self.code_intra_cu((x, y), log2, depth, m);
            return;
        }
        // inter, PART_2Nx2N
        self.cab.encode(PART_MODE, 1);
        self.cab.encode(MERGE_FLAG, u32::from(merge));
        if !merge {
            let mvp = self.amvp(cur, n as i32)[mvp_idx];
            self.code_mvd([mv[0] - mvp[0], mv[1] - mvp[1]]);
            self.cab.encode(MVP_FLAG, mvp_idx as u32);
            self.cab.encode(RQT_ROOT_CBF, u32::from(res.any()));
        }
        if res.any() {
            self.code_transform_unit(&res, (x, y), log2, false, 1);
        }
        self.write_rec_inter(&res, (x, y), n);
        self.set_blk(cur, n, Blk { intra: false, skip: false, depth, ipm: 1, mv: [mv[0] as i16, mv[1] as i16] });
    }

    /// prev_intra_luma_pred_flag, mpm_idx / rem_intra_luma_pred_mode (8.4.2).
    fn code_intra_mode(&mut self, (x, y): (i32, i32), mode: u32) {
        let cand_a = if self.avail((x, y), (x - 1, y)) {
            let b = self.blk(x - 1, y);
            if b.intra { u32::from(b.ipm) } else { 1 }
        } else {
            1
        };
        let cand_b = if self.avail((x, y), (x, y - 1)) && y > ((y >> LOG2_CTB) << LOG2_CTB) {
            let b = self.blk(x, y - 1);
            if b.intra { u32::from(b.ipm) } else { 1 }
        } else {
            1
        };
        let mut list = if cand_a == cand_b {
            if cand_a < 2 { [0, 1, 26] } else { [cand_a, 2 + ((cand_a + 29) % 32), 2 + ((cand_a - 2 + 1) % 32)] }
        } else {
            let c = if cand_a != 0 && cand_b != 0 {
                0
            } else if cand_a != 1 && cand_b != 1 {
                1
            } else {
                26
            };
            [cand_a, cand_b, c]
        };
        if let Some(idx) = list.iter().position(|&m| m == mode) {
            self.cab.encode(PREV_INTRA_LUMA, 1);
            match idx {
                0 => self.cab.bypass(0),
                1 => self.cab.bypass_bits(0b10, 2),
                _ => self.cab.bypass_bits(0b11, 2),
            }
        } else {
            self.cab.encode(PREV_INTRA_LUMA, 0);
            list.sort_unstable();
            let rem = mode - list.iter().filter(|&&m| m < mode).count() as u32;
            self.cab.bypass_bits(rem, 5);
        }
    }

    fn code_mvd(&mut self, mvd: [i32; 2]) {
        let a = [mvd[0].unsigned_abs(), mvd[1].unsigned_abs()];
        for &v in &a {
            self.cab.encode(ABS_MVD_GT0, u32::from(v > 0));
        }
        for &v in &a {
            if v > 0 {
                self.cab.encode(ABS_MVD_GT1, u32::from(v > 1));
            }
        }
        for k in 0..2 {
            if a[k] > 0 {
                if a[k] > 1 {
                    self.exp_golomb(a[k] - 2, 1);
                }
                self.cab.bypass(u32::from(mvd[k] < 0));
            }
        }
    }

    /// k-th order Exp-Golomb in bypass bins.
    fn exp_golomb(&mut self, mut v: u32, mut k: u32) {
        while k < 31 && v >= (1 << k) {
            self.cab.bypass(1);
            v -= 1 << k;
            k += 1;
        }
        self.cab.bypass(0);
        self.cab.bypass_bits(v, k);
    }

    /// Prediction residuals of an inter CU (all three planes), transformed and quantised; the
    /// prediction is kept for the reconstruction.
    fn inter_residuals(&mut self, refp: &[Padded; 3], (x, y): (usize, usize), n: usize, mv: [i32; 2], res: &mut ResidualCu) {
        let bd = self.sh.bd;
        for c in 0..3 {
            let (bn, bx, by) = if c == 0 { (n, x, y) } else { (n / 2, x / 2, y / 2) };
            let mut pred = vec![0u16; bn * bn];
            mc::predict(&refp[c], c, (bx as i32, by as i32), mv, (bn, bn), bd, &mut pred);
            let qp = if c == 0 { self.sh.qp + 6 * (bd as i32 - 8) } else { self.qp_c };
            self.transform_block(c, (bx, by), bn, &pred, false, false, qp, &mut res.planes[c]);
            res.preds[c] = pred;
        }
    }

    /// Residual of the source against `pred` for an n x n block of component `c`, transformed and
    /// quantised (levels) and reconstructed (decoder-exact residual).
    #[allow(clippy::too_many_arguments)]
    fn transform_block(&mut self, c: usize, (x, y): (usize, usize), n: usize, pred: &[u16], dst: bool, intra: bool, qp: i32, out: &mut Coded) {
        let bd = self.sh.bd;
        for r in 0..n {
            for k in 0..n {
                self.resid[r * n + k] = i32::from(self.src_at(c, x + k, y + r)) - i32::from(pred[r * n + k]);
            }
        }
        quant::forward(&self.resid[..n * n], n, dst, bd, &mut self.coef[..n * n]);
        let nz = quant::quantize(&self.coef[..n * n], n, qp, bd, intra, &mut self.lev[..n * n]);
        out.levels.clear();
        out.levels.extend_from_slice(&self.lev[..n * n]);
        out.nonzero = nz > 0;
        out.recon.clear();
        if nz > 0 {
            quant::reconstruct(&self.lev[..n * n], n, dst, qp, bd, &mut self.recon[..n * n]);
            out.recon.extend_from_slice(&self.recon[..n * n]);
        }
    }

    /// Write prediction + residual of an inter CU into the reconstruction.
    fn write_rec_inter(&mut self, res: &ResidualCu, (x, y): (usize, usize), n: usize) {
        let max = (1i32 << self.sh.bd) - 1;
        for c in 0..3 {
            let (bn, bx, by) = if c == 0 { (n, x, y) } else { (n / 2, x / 2, y / 2) };
            let (w, oy) = self.plane_geo(c);
            let pred = &res.preds[c];
            let rec = &res.planes[c].recon;
            for r in 0..bn {
                let row = (by + r - oy) * w + bx;
                for k in 0..bn {
                    let p = i32::from(pred.get(r * bn + k).copied().unwrap_or(0));
                    let v = if rec.is_empty() { p } else { (p + rec[r * bn + k]).clamp(0, max) };
                    if let Some(s) = self.band.rec[c].get_mut(row + k) {
                        *s = v as u16;
                    }
                }
            }
        }
    }

    /// An intra CU: luma then chroma, each predicted from the reconstruction, coded and
    /// reconstructed in turn (chroma uses the luma mode).
    fn code_intra_cu(&mut self, (x, y): (usize, usize), log2: u32, depth: u8, mode: u32) {
        let n = 1usize << log2;
        let bd = self.sh.bd;
        let mut res = ResidualCu::default();
        // luma
        self.intra_predict(0, (x, y), log2, mode, true);
        let pred_l = self.pred[..n * n].to_vec();
        let qp_l = self.sh.qp + 6 * (bd as i32 - 8);
        self.transform_block(0, (x, y), n, &pred_l, n == 4, true, qp_l, &mut res.planes[0]);
        self.write_block(0, (x, y), n, &pred_l, &res.planes[0].recon);
        // the CU's blocks are marked before chroma prediction only for the mode; availability of
        // chroma neighbours does not involve this CU's own samples
        for c in 1..3 {
            self.intra_predict(c, (x / 2, y / 2), log2 - 1, mode, true);
            let pred_c = self.pred[..(n / 2) * (n / 2)].to_vec();
            let qp_c = self.qp_c;
            self.transform_block(c, (x / 2, y / 2), n / 2, &pred_c, false, true, qp_c, &mut res.planes[c]);
            self.write_block(c, (x / 2, y / 2), n / 2, &pred_c, &res.planes[c].recon);
        }
        self.code_transform_unit(&res, (x, y), log2, true, mode);
        self.set_blk((x as i32, y as i32), n, Blk { intra: true, skip: false, depth, ipm: mode as u8, mv: [0, 0] });
    }

    fn write_block(&mut self, c: usize, (x, y): (usize, usize), n: usize, pred: &[u16], recon: &[i32]) {
        let max = (1i32 << self.sh.bd) - 1;
        let (w, oy) = self.plane_geo(c);
        for r in 0..n {
            let row = (y + r - oy) * w + x;
            for k in 0..n {
                let p = i32::from(pred[r * n + k]);
                let v = if recon.is_empty() { p } else { (p + recon[r * n + k]).clamp(0, max) };
                if let Some(s) = self.band.rec[c].get_mut(row + k) {
                    *s = v as u16;
                }
            }
        }
    }

    /// transform_tree() with one transform unit at depth 0: cbf_cb, cbf_cr, cbf_luma, then the
    /// residual blocks.
    fn code_transform_unit(&mut self, res: &ResidualCu, (_x, _y): (usize, usize), log2: u32, intra: bool, mode: u32) {
        let (cb, cr, l) = (res.planes[1].nonzero, res.planes[2].nonzero, res.planes[0].nonzero);
        self.cab.encode(CBF_CHROMA, u32::from(cb));
        self.cab.encode(CBF_CHROMA, u32::from(cr));
        if intra || cb || cr {
            self.cab.encode(CBF_LUMA + 1, u32::from(l));
        }
        let scan = |log2: u32, c: usize| -> usize {
            if !(intra && (log2 == 2 || (log2 == 3 && c == 0))) {
                0
            } else if (6..=14).contains(&mode) {
                2
            } else if (22..=30).contains(&mode) {
                1
            } else {
                0
            }
        };
        if l {
            self.code_residual(&res.planes[0].levels, log2, 0, scan(log2, 0));
        }
        if cb {
            self.code_residual(&res.planes[1].levels, log2 - 1, 1, scan(log2 - 1, 1));
        }
        if cr {
            self.code_residual(&res.planes[2].levels, log2 - 1, 2, scan(log2 - 1, 2));
        }
    }

    /// residual_coding() (7.3.8.11) of a block of quantised levels (row-major), at least one non-zero.
    fn code_residual(&mut self, levels: &[i32], log2: u32, c: usize, scan_idx: usize) {
        let n = 1usize << log2;
        let log2sb = log2 - 2;
        let sb_scan = scan_order(log2sb, scan_idx);
        let pos_scan = &SCAN_4[scan_idx];
        let sb_w = 1usize << log2sb;
        let at = |xs: usize, ys: usize, np: usize| -> i32 {
            let (xp, yp) = pos_scan[np];
            levels.get((ys * 4 + yp as usize) * n + xs * 4 + xp as usize).copied().unwrap_or(0)
        };
        // the last significant coefficient in scan order
        let mut last: Option<(usize, usize)> = None;
        'find: for i in (0..sb_scan.len()).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            for np in (0..16).rev() {
                if at(xs, ys, np) != 0 {
                    last = Some((i, np));
                    break 'find;
                }
            }
        }
        let Some((last_sb, last_pos)) = last else { return };
        let (lxs, lys) = (sb_scan[last_sb].0 as u32, sb_scan[last_sb].1 as u32);
        let (lx, ly) = (lxs * 4 + u32::from(pos_scan[last_pos].0), lys * 4 + u32::from(pos_scan[last_pos].1));
        let (cx, cy) = if scan_idx == 2 { (ly, lx) } else { (lx, ly) };
        self.code_last_pos((cx, cy), log2, c);
        let mut csbf = [0u8; 64];
        let mut prev_g1: Option<u32> = None;
        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let right = if xs + 1 < sb_w { csbf[ys * 8 + xs + 1] } else { 0 };
            let below = if ys + 1 < sb_w { csbf[(ys + 1) * 8 + xs] } else { 0 };
            let any = (0..16).any(|np| at(xs, ys, np) != 0);
            let mut infer_dc = false;
            if i < last_sb && i > 0 {
                let inc = (right | below).min(1) as usize + if c > 0 { 2 } else { 0 };
                self.cab.encode(CODED_SUB_BLOCK + inc, u32::from(any));
                csbf[ys * 8 + xs] = u8::from(any);
                infer_dc = true;
            } else {
                csbf[ys * 8 + xs] = 1;
            }
            // significance map, positions in descending scan order
            let mut sig: Vec<usize> = Vec::with_capacity(16);
            let start: i32 = if i == last_sb {
                sig.push(last_pos);
                last_pos as i32 - 1
            } else {
                15
            };
            if csbf[ys * 8 + xs] != 0 {
                let prev_csbf = right + (below << 1);
                let mut np = start;
                while np >= 0 {
                    let p = np as usize;
                    let s = at(xs, ys, p) != 0;
                    if p > 0 || !infer_dc {
                        let inc = sig_ctx_inc(log2, c, scan_idx, (xs, ys), prev_csbf, (usize::from(pos_scan[p].0), usize::from(pos_scan[p].1)));
                        self.cab.encode(SIG_COEFF + if c == 0 { 0 } else { 27 } + inc, u32::from(s));
                        if s {
                            sig.push(p);
                            infer_dc = false;
                        }
                    } else {
                        // DC inferred significant
                        sig.push(0);
                    }
                    np -= 1;
                }
            }
            if sig.is_empty() {
                continue;
            }
            let vals: Vec<i32> = sig.iter().map(|&p| at(xs, ys, p)).collect();
            let mut ctx_set = if i == 0 || c > 0 { 0 } else { 2 };
            if prev_g1 == Some(0) {
                ctx_set += 1;
            }
            prev_g1 = Some(self.code_levels(&vals, ctx_set, c));
        }
    }

    /// coeff_abs_level_greater1 / greater2 flags, the signs (no sign data hiding) and
    /// coeff_abs_level_remaining of one sub-block's significant levels (`vals`, in descending scan
    /// order) with context set `ctx_set`. Returns the final greater1Ctx, which picks the context
    /// set of the next sub-block.
    fn code_levels(&mut self, vals: &[i32], ctx_set: usize, c: usize) -> u32 {
        let abs: Vec<u32> = vals.iter().map(|v| v.unsigned_abs()).collect();
        let mut g1ctx = 1u32;
        let c_off_g1 = if c > 0 { 16 } else { 0 };
        let mut gt1 = [0u32; 16];
        let mut first_g1: Option<usize> = None;
        for k in 0..abs.len().min(8) {
            let flag = u32::from(abs[k] > 1);
            self.cab.encode(GT1 + ctx_set * 4 + g1ctx.min(3) as usize + c_off_g1, flag);
            gt1[k] = flag;
            if flag == 1 {
                g1ctx = 0;
                if first_g1.is_none() {
                    first_g1 = Some(k);
                }
            } else if g1ctx > 0 {
                g1ctx += 1;
            }
        }
        let mut gt2 = 0;
        if let Some(f) = first_g1 {
            gt2 = u32::from(abs[f] > 2);
            self.cab.encode(GT2 + ctx_set + if c > 0 { 4 } else { 0 }, gt2);
        }
        for &v in vals {
            self.cab.bypass(u32::from(v < 0));
        }
        // remaining levels
        let mut rice = 0u32;
        for k in 0..abs.len() {
            let mut base = 1 + if k < 8 { gt1[k] } else { 0 };
            if Some(k) == first_g1 {
                base += gt2;
            }
            let thresh = if k < 8 { if Some(k) == first_g1 { 3 } else { 2 } } else { 1 };
            if base == thresh {
                let rem = abs[k] - base;
                self.coeff_abs_level_remaining(rem, rice);
                if abs[k] > 3 * (1 << rice) {
                    rice = (rice + 1).min(4);
                }
            }
        }
        g1ctx
    }

    /// last_sig_coeff_{x,y}_prefix (context coded, truncated unary) then the suffixes (bypass).
    fn code_last_pos(&mut self, (x, y): (u32, u32), log2: u32, c: usize) {
        let cmax = (log2 << 1) - 1;
        let (off, shift) = if c == 0 { (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2) } else { (15, log2 - 2) };
        let split = |v: u32| -> (u32, u32, u32) {
            if v < 4 {
                return (v, 0, 0);
            }
            // prefix p: v in [min(p), min(p + 1)) with min(p) = (2 + (p & 1)) << ((p >> 1) - 1)
            let mut p = 4;
            while p < cmax {
                let next = (2 + ((p + 1) & 1)) << (((p + 1) >> 1) - 1);
                if v < next {
                    break;
                }
                p += 1;
            }
            let min = (2 + (p & 1)) << ((p >> 1) - 1);
            (p, v - min, (p >> 1) - 1)
        };
        let (px, sx, nx) = split(x);
        let (py, sy, ny) = split(y);
        for (prefix, ctx) in [(px, LAST_X_PREFIX), (py, LAST_Y_PREFIX)] {
            for b in 0..prefix {
                self.cab.encode(ctx + (off + (b >> shift)) as usize, 1);
            }
            if prefix < cmax {
                self.cab.encode(ctx + (off + (prefix >> shift)) as usize, 0);
            }
        }
        if px > 3 {
            self.cab.bypass_bits(sx, nx);
        }
        if py > 3 {
            self.cab.bypass_bits(sy, ny);
        }
    }

    /// coeff_abs_level_remaining (9.3.3.11): Rice prefix up to 3, then an escape with Exp-Golomb
    /// order rice + 1, mirroring the decoder's parse.
    fn coeff_abs_level_remaining(&mut self, v: u32, rice: u32) {
        if v < (4 << rice) {
            let prefix = v >> rice;
            for _ in 0..prefix {
                self.cab.bypass(1);
            }
            self.cab.bypass(0);
            self.cab.bypass_bits(v & ((1 << rice) - 1), rice);
        } else {
            let mut p = 4u32;
            while p < 31 && v >= (((1u32 << (p + 1 - 3)) + 2) << rice) {
                p += 1;
            }
            let base = ((1u32 << (p - 3)) + 2) << rice;
            for _ in 0..p {
                self.cab.bypass(1);
            }
            self.cab.bypass(0);
            self.cab.bypass_bits(v - base, p - 3 + rice);
        }
    }

    /// end_of_slice_segment_flag after a CTU.
    pub fn end_ctu(&mut self, last: bool) {
        self.cab.terminate(u32::from(last));
    }

    pub fn finish(self) -> Vec<u8> {
        self.cab.finish()
    }
}

/// Whether the chroma prediction of an n x n luma CU at (x, y) with `mv` stays inside the padding.
fn mv_inside_chroma(refp: &[Padded; 3], (x, y): (usize, usize), n: usize, mv: [i32; 2]) -> bool {
    mc::mv_inside(&refp[1], (x as i32 / 2, y as i32 / 2), (n / 2, n / 2), mv, 3, 2)
}

/// sigCtx (9.3.4.2.5) of sig_coeff_flag at position (xp, yp) of sub-block (xs, ys).
fn sig_ctx_inc(log2: u32, c: usize, scan_idx: usize, (xs, ys): (usize, usize), prev_csbf: u8, (xp, yp): (usize, usize)) -> usize {
    let (xc, yc) = (xs * 4 + xp, ys * 4 + yp);
    if log2 == 2 {
        return CTX_IDX_MAP[(yc << 2) + xc] as usize;
    }
    if xc + yc == 0 {
        return 0;
    }
    let mut s = match prev_csbf {
        0 => {
            if xp + yp == 0 {
                2
            } else if xp + yp < 3 {
                1
            } else {
                0
            }
        }
        1 => {
            if yp == 0 {
                2
            } else if yp == 1 {
                1
            } else {
                0
            }
        }
        2 => {
            if xp == 0 {
                2
            } else if xp == 1 {
                1
            } else {
                0
            }
        }
        _ => 2,
    };
    if c == 0 {
        if xs + ys > 0 {
            s += 3;
        }
        s += if log2 == 3 { if scan_idx == 0 { 9 } else { 15 } } else { 21 };
    } else {
        s += if log2 == 3 { 9 } else { 12 };
    }
    s
}

/// One plane's coded block: quantised levels, whether any is non-zero, and the reconstructed
/// residual (empty when all levels are zero).
#[derive(Default)]
struct Coded {
    levels: Vec<i32>,
    nonzero: bool,
    recon: Vec<i32>,
}

#[derive(Default)]
struct ResidualCu {
    planes: [Coded; 3],
    preds: [Vec<u16>; 3],
}

impl ResidualCu {
    fn any(&self) -> bool {
        self.planes.iter().any(|p| p.nonzero)
    }
}
