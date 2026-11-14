//! Frame-level rate control: a constant QP, or a one-pass average bitrate that steers the QP of
//! each frame from the bits the previous frames took (keyframes a few steps finer), within a
//! one-second decoder buffer (VBV) filled at the ceiling. With the ceiling at the target that is
//! constant bitrate, as the H.264 encoder's CBR.

/// How the bitrate is controlled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateControl {
    /// Every frame at this QP (0..=51).
    ConstantQp(u8),
    /// An average bitrate in kilobits per second, with a ceiling: the rate that fills a one-second
    /// decoder buffer, which no frame may drain. `max_kbps == kbps` is constant bitrate.
    Average { kbps: u32, max_kbps: u32 },
}

pub struct RateController {
    mode: RateControl,
    /// Target bits per frame.
    target: f64,
    /// Ceiling bits per frame (one second at max_kbps spread over the frames): what the decoder
    /// buffer gains per frame.
    ceiling: f64,
    /// The P-frame QP as a real number.
    qp: f64,
    /// Bits spent minus bits budgeted so far.
    debt: f64,
    frames: u64,
    /// The decoder buffer (VBV): its size (one second at the ceiling) and how full it is before
    /// the next frame is taken out, in bits.
    vbv_size: f64,
    vbv_fill: f64,
    /// The bits and QP of the last P frame and the last keyframe (indexed by `key`): what the next
    /// frame's size is estimated from.
    last: [Option<(f64, i32)>; 2],
}

/// The share of the buffer a frame may plan to take: the rest is headroom for the estimate's error.
const VBV_HEADROOM: f64 = 0.9;

impl RateController {
    pub fn new(mode: RateControl, width: u32, height: u32, fps: f64) -> RateController {
        let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
        let (target, ceiling, qp) = match mode {
            RateControl::ConstantQp(q) => (0.0, 0.0, f64::from(q.min(51))),
            RateControl::Average { kbps, max_kbps } => {
                let target = f64::from(kbps.max(1)) * 1000.0 / fps;
                let ceiling = f64::from(max_kbps.max(kbps).max(1)) * 1000.0 / fps;
                // starting point from the bits per pixel: about 0.1 bpp at QP 30, 6 QP per halving
                let bpp = target / (f64::from(width.max(1)) * f64::from(height.max(1)));
                let qp = 30.0 - 6.0 * (bpp / 0.1).log2();
                (target, ceiling, qp.clamp(10.0, 46.0))
            }
        };
        let vbv_size = ceiling * fps;
        RateController { mode, target, ceiling, qp, debt: 0.0, frames: 0, vbv_size, vbv_fill: vbv_size * VBV_HEADROOM, last: [None, None] }
    }

    /// The estimated size of the next frame at `qp`: the last frame of its kind (or of the other
    /// kind, a keyframe counted as four P frames) scaled by 6 QP per halving.
    fn estimate(&self, key: bool, qp: i32) -> Option<f64> {
        let (bits, at) = match (self.last[usize::from(key)], self.last[usize::from(!key)]) {
            (Some(same), _) => same,
            (None, Some((bits, at))) => (if key { bits * 4.0 } else { bits / 4.0 }, at),
            (None, None) => return None,
        };
        Some(bits * 2f64.powf(f64::from(at - qp) / 6.0))
    }

    /// The QP of the next frame.
    pub fn frame_qp(&self, key: bool) -> i32 {
        match self.mode {
            RateControl::ConstantQp(q) => i32::from(q.min(51)),
            RateControl::Average { .. } => {
                let q = if key { self.qp - 3.0 } else { self.qp };
                let mut q = (q.round() as i32).clamp(1, 51);
                // the frame must fit in what the decoder buffer holds: raise the QP until the
                // estimate does
                let room = self.vbv_fill * VBV_HEADROOM;
                while q < 51 && self.estimate(key, q).is_some_and(|bits| bits > room) {
                    q += 1;
                }
                q
            }
        }
    }

    /// The coarser QP to code a frame again at when it came out at `bits` bits at `qp` and would
    /// drain the decoder buffer: aimed at the share of the buffer a frame may plan to take, at
    /// least one step up. `None` when the frame fits, the QP is already 51, or there is no buffer
    /// (constant QP).
    pub fn retry_qp(&self, bits: f64, qp: i32) -> Option<i32> {
        if !matches!(self.mode, RateControl::Average { .. }) || bits <= self.vbv_fill || qp >= 51 {
            return None;
        }
        let room = (self.vbv_fill * VBV_HEADROOM).max(1.0);
        let up = (6.0 * (bits / room).log2()).ceil() as i32;
        Some((qp + up.max(1)).min(51))
    }

    /// Account for a coded frame of `bits` bits, coded at `qp`.
    pub fn update(&mut self, bits: f64, key: bool, qp: i32) {
        if let RateControl::Average { .. } = self.mode {
            self.frames += 1;
            // keyframes are budgeted at four times a P frame
            let budget = if key { self.target * 4.0 } else { self.target };
            self.debt += bits - self.target;
            let ratio = (bits / budget.max(1.0)).max(1e-3);
            // move towards the QP that would have hit the budget, plus a correction for the debt
            // accumulated over about two seconds of frames
            let step = 6.0 * ratio.log2();
            let debt_step = 6.0 * ((self.debt / (self.target * 50.0)) + 1.0).max(0.25).log2();
            let mut delta = (0.5 * step + 0.5 * debt_step).clamp(-3.0, 3.0);
            if bits > self.ceiling * if key { 4.0 } else { 1.0 } {
                delta = delta.max(1.0);
            }
            self.qp = (self.qp + delta).clamp(1.0, 51.0);
            // the decoder takes the frame out of its buffer, which then fills at the ceiling
            self.vbv_fill = ((self.vbv_fill - bits).max(0.0) + self.ceiling).min(self.vbv_size);
            self.last[usize::from(key)] = Some((bits, qp));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic coder: `complexity` x 40 kbit at QP 30, halving every 6 QP.
    fn coded_bits(complexity: f64, qp: i32) -> f64 {
        complexity * 40_000.0 * 2f64.powf(f64::from(30 - qp) / 6.0)
    }

    /// Constant bitrate (ceiling = target) through a scene that suddenly needs 8 times the bits
    /// (about as much as QP 51 still fits in the budget), starting on a keyframe: with the frames that would drain it coded again (as the encoder
    /// does), the one-second decoder buffer never runs dry, and ten seconds average close to the
    /// target.
    #[test]
    fn constant_bitrate_keeps_the_decoder_buffer() {
        let (kbps, fps) = (1000u32, 25.0);
        let mut rc = RateController::new(RateControl::Average { kbps, max_kbps: kbps }, 640, 360, fps);
        let size = f64::from(kbps) * 1000.0;
        let mut fill = size * VBV_HEADROOM;
        let mut total = 0.0;
        for i in 0..250 {
            let key = i % 50 == 0;
            let complexity = if (100..150).contains(&i) { 8.0 } else { 1.0 } * if key { 4.0 } else { 1.0 };
            let mut qp = rc.frame_qp(key);
            let mut bits = coded_bits(complexity, qp);
            for _ in 0..2 {
                let Some(coarser) = rc.retry_qp(bits, qp) else { break };
                qp = coarser;
                bits = coded_bits(complexity, qp);
            }
            assert!(bits <= fill, "frame {i} ({bits:.0} bits at QP {qp}) drains the buffer ({fill:.0} bits)");
            fill = (fill - bits + size / fps).min(size);
            total += bits;
            rc.update(bits, key, qp);
        }
        let average_kbps = total / 10.0 / 1000.0;
        assert!((average_kbps - f64::from(kbps)).abs() < 0.15 * f64::from(kbps), "{average_kbps:.0} kbps");
    }

    /// Only a frame that would drain the buffer is coded again, and constant QP never is.
    #[test]
    fn only_frames_that_drain_the_buffer_are_coded_again() {
        let rc = RateController::new(RateControl::Average { kbps: 1000, max_kbps: 1000 }, 640, 360, 25.0);
        let fill = rc.vbv_fill;
        assert_eq!(rc.retry_qp(fill, 30), None);
        let coarser = rc.retry_qp(fill * 4.0, 30).unwrap();
        assert!(coarser >= 30 + 12, "four times too big is at least two halvings: {coarser}");
        assert_eq!(rc.retry_qp(fill * 4.0, 51), None);
        assert_eq!(RateController::new(RateControl::ConstantQp(30), 640, 360, 25.0).retry_qp(1e12, 30), None);
    }

    /// Without a frame to estimate from, and in constant-QP mode, the buffer does not change the QP.
    #[test]
    fn nothing_to_estimate_from_leaves_the_qp() {
        let rc = RateController::new(RateControl::Average { kbps: 1000, max_kbps: 1000 }, 640, 360, 25.0);
        let first = rc.frame_qp(true);
        assert_eq!(first, (rc.qp - 3.0).round() as i32);
        let rc = RateController::new(RateControl::ConstantQp(30), 640, 360, 25.0);
        assert_eq!(rc.frame_qp(false), 30);
    }
}
