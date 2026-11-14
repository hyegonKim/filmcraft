//! Clean-room, pure-Rust H.265 / HEVC encoder, written from ITU-T Rec. H.265 (ISO/IEC 23008-2).
//!
//! - Main (8-bit) and Main 10 (10-bit) profiles, 4:2:0, 32x32 coding tree units.
//! - Coding quadtree 32 / 16 / 8 decided per CTU; intra coding units (11 candidate modes with
//!   most-probable-mode coding) and, in P pictures, skip / merge and AMVP inter coding units with
//!   a hexagon motion search refined to quarter samples. One transform unit per coding unit
//!   (DCT 8..32, chroma 4..16), CABAC throughout.
//! - IDR pictures every keyframe interval, P pictures predicted from the previous picture in
//!   between (no reordering: decode order is output order).
//! - Rate control: constant QP, or a one-pass average bitrate with a ceiling.
//! - Slices of whole CTU rows coded in parallel (feature `threads`, on by default).
//! - The deblocking filter and SAO are off, so the encoder's reconstruction is exactly what
//!   decoders output; the colour description (BT.709, or BT.2020 with PQ / HLG) goes in the VUI,
//!   and HDR10 static metadata (mastering display colour volume, content light level) in a prefix
//!   SEI with every IDR picture.
//! - The level is the lowest whose Main tier limits take the picture size, frame rate, bitrate
//!   ceiling and slice count.
//!
//! Output is MP4-style: each packet holds 4-byte length-prefixed NAL units, and the VPS / SPS / PPS
//! are available for the `hvcC` record ([`Encoder::parameter_sets`]).
//!
//! ```no_run
//! use filmcraft_hevcenc::{Encoder, EncoderConfig, Frame, RateControl};
//! let mut cfg = EncoderConfig::new(1280, 720, 30, 1);
//! cfg.rate = RateControl::Average { kbps: 4000, max_kbps: 6000 };
//! let mut enc = Encoder::new(cfg).unwrap();
//! let (y, u, v) = (vec![128u16; 1280 * 720], vec![128u16; 640 * 360], vec![128u16; 640 * 360]);
//! let packet = enc.encode(&Frame { y: &y, u: &u, v: &v, y_stride: 1280, uv_stride: 640 }).unwrap();
//! assert!(packet.key);
//! ```

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod cabac;
mod intra;
mod mc;
mod nal;
mod quant;
mod ratecontrol;
mod sample;
mod slice;
// generated from the text of the standard (the decoder's tables): the encoder uses part of them
#[allow(dead_code)]
mod spec_tables;
mod tables;
mod transform;

pub use nal::ColorDescription;
pub use ratecontrol::RateControl;

use mc::{PAD, Padded};
use nal::{NAL_IDR_W_RADL, NAL_PREFIX_SEI, NAL_TRAIL_R, SeqParams, SliceHeader};
use ratecontrol::RateController;
use slice::{Band, Blk, LOG2_CTB, Shared, SliceEncoder};

/// Encoder errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid picture size {0}x{1} (must be even, non-zero and at most 16384 each way)")]
    InvalidDimensions(u32, u32),
    #[error("invalid frame rate {0}/{1}")]
    InvalidFrameRate(u32, u32),
    #[error("unsupported bit depth {0} (8 or 10)")]
    BitDepth(u32),
    #[error("input frame planes too small for the configured size")]
    BadFrame,
}

/// What to encode and how.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// Picture size in luma samples: even, at most 16384 each way.
    pub width: u32,
    pub height: u32,
    /// Frame rate as fps_num / fps_den.
    pub fps_num: u32,
    pub fps_den: u32,
    /// 8 (Main) or 10 (Main 10).
    pub bit_depth: u32,
    /// Frames from one IDR picture to the next (0 or 1: every picture).
    pub keyint: u32,
    pub rate: RateControl,
    pub color: ColorDescription,
    /// Slices (and threads) per picture; 0 picks one per available core, which makes the stream
    /// depend on the machine.
    pub slices: usize,
    /// HDR10 static metadata written as SEI in every IDR access unit: the 24-byte
    /// mastering_display_colour_volume payload (the `mdcv` box layout) and (MaxCLL, MaxFALL).
    pub mastering_display: Option<[u8; 24]>,
    pub content_light: Option<(u16, u16)>,
    /// Sample (pixel) aspect ratio written in the VUI, width : height (square by default).
    pub sar: (u16, u16),
}

impl EncoderConfig {
    /// 8-bit BT.709 at QP 28 with a two-second keyframe interval.
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32) -> EncoderConfig {
        let fps = if fps_den > 0 { f64::from(fps_num) / f64::from(fps_den) } else { 30.0 };
        EncoderConfig {
            width,
            height,
            fps_num,
            fps_den,
            bit_depth: 8,
            keyint: (fps * 2.0).round().max(1.0) as u32,
            rate: RateControl::ConstantQp(28),
            color: ColorDescription::default(),
            slices: 0,
            mastering_display: None,
            content_light: None,
            sar: (1, 1),
        }
    }
}

/// One input picture: planar Y'CbCr 4:2:0 samples of the configured bit depth (8-bit values in
/// 0..=255), with strides in samples.
pub struct Frame<'a> {
    pub y: &'a [u16],
    pub u: &'a [u16],
    pub v: &'a [u16],
    pub y_stride: usize,
    pub uv_stride: usize,
}

/// One coded picture: 4-byte length-prefixed NAL units (an access unit).
#[derive(Clone, Debug)]
pub struct Packet {
    pub data: Vec<u8>,
    /// An IDR picture (a sync sample).
    pub key: bool,
    /// The picture number (0 = first).
    pub index: u64,
}

pub struct Encoder {
    cfg: EncoderConfig,
    seq: SeqParams,
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    rc: RateController,
    /// The previous picture's reconstruction, padded (luma, Cb, Cr).
    refp: Option<[Padded; 3]>,
    /// The picture being coded's reconstruction (luma, Cb, Cr at the coded size) and block state,
    /// kept from picture to picture rather than allocated for each.
    rec: [Vec<u16>; 3],
    blk: Vec<Blk>,
    index: u64,
    poc: i32,
    slices: usize,
    /// The HDR10 static metadata SEI NAL unit sent with each IDR picture.
    sei: Option<Vec<u8>>,
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Encoder, Error> {
        let (w, h) = (cfg.width, cfg.height);
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || w > 16384 || h > 16384 {
            return Err(Error::InvalidDimensions(w, h));
        }
        if cfg.fps_num == 0 || cfg.fps_den == 0 {
            return Err(Error::InvalidFrameRate(cfg.fps_num, cfg.fps_den));
        }
        if cfg.bit_depth != 8 && cfg.bit_depth != 10 {
            return Err(Error::BitDepth(cfg.bit_depth));
        }
        let fps = f64::from(cfg.fps_num) / f64::from(cfg.fps_den);
        let (coded_w, coded_h) = (w.div_ceil(8) * 8, h.div_ceil(8) * 8);
        let rows = coded_h.div_ceil(1 << LOG2_CTB) as usize;
        let slices = if cfg.slices > 0 { cfg.slices } else { default_slices() }.clamp(1, rows.max(1));
        let max_kbps = match cfg.rate {
            RateControl::ConstantQp(_) => 0,
            RateControl::Average { kbps, max_kbps } => kbps.max(max_kbps),
        };
        let seq = SeqParams {
            coded_w,
            coded_h,
            width: w,
            height: h,
            bit_depth: cfg.bit_depth,
            log2_ctb: LOG2_CTB,
            level_idc: nal::level_for(coded_w, coded_h, fps, max_kbps, slices),
            color: cfg.color,
            fps_num: cfg.fps_num,
            fps_den: cfg.fps_den,
            sar: cfg.sar,
        };
        let sei = nal::sei_hdr(cfg.mastering_display.as_ref(), cfg.content_light).map(|rbsp| nal::nal(NAL_PREFIX_SEI, &rbsp));
        Ok(Encoder {
            vps: nal::vps(&seq),
            sps: nal::sps(&seq),
            pps: nal::pps(),
            rc: RateController::new(cfg.rate, w, h, fps),
            seq,
            cfg,
            refp: None,
            rec: [Vec::new(), Vec::new(), Vec::new()],
            blk: Vec::new(),
            index: 0,
            poc: 0,
            slices,
            sei,
        })
    }

    /// The VPS, SPS and PPS NAL units (without length prefix), for the `hvcC` record.
    pub fn parameter_sets(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.vps, &self.sps, &self.pps)
    }

    /// The `hvcC` record (HEVCDecoderConfigurationRecord) for an MP4 / QuickTime sample entry.
    pub fn hvcc(&self) -> Vec<u8> {
        nal::hvcc(&self.seq, &self.vps, &self.sps, &self.pps)
    }

    /// general_profile_idc (1 Main, 2 Main 10) and general_level_idc.
    pub fn profile_level(&self) -> (u8, u8) {
        (self.seq.profile_idc(), self.seq.level_idc)
    }

    /// The configuration this encoder was created with.
    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// The last coded picture as a decoder reconstructs it (Y, Cb, Cr at the output size): what
    /// round-trip tests compare a decoder's output with.
    pub fn reconstruction(&self) -> Option<[Vec<u16>; 3]> {
        let r = self.refp.as_ref()?;
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        let crop = |p: &Padded, pw: usize, ph: usize| -> Vec<u16> { (0..ph).flat_map(|y| (0..pw).map(move |x| p.at(x as isize, y as isize))).collect() };
        Some([crop(&r[0], w, h), crop(&r[1], w / 2, h / 2), crop(&r[2], w / 2, h / 2)])
    }

    /// Code the picture `src` (planes at the coded size) at `qp` into its access unit, leaving
    /// its reconstruction in `self.rec`.
    fn code_picture(&mut self, src: [&[u16]; 3], key: bool, qp: i32) -> Vec<u8> {
        let (cdw, cdh) = (self.seq.coded_w as usize, self.seq.coded_h as usize);
        // every coded sample of the reconstruction is written by the slices, so the buffers are
        // only sized (once), not cleared; the block state starts afresh with each picture
        for (plane, len) in self.rec.iter_mut().zip([cdw * cdh, (cdw / 2) * (cdh / 2), (cdw / 2) * (cdh / 2)]) {
            plane.resize(len, 0);
        }
        let w4 = cdw.div_ceil(4);
        self.blk.clear();
        self.blk.resize(w4 * cdh.div_ceil(4), Blk::default());
        let shared = Shared { src, refp: if key { None } else { self.refp.as_ref() }, coded_w: cdw, coded_h: cdh, bd: self.cfg.bit_depth, qp, intra: key };
        // slices: bands of whole CTU rows
        let wctb = self.seq.width_in_ctbs() as usize;
        let hctb = self.seq.height_in_ctbs() as usize;
        let ctb = 1usize << LOG2_CTB;
        let mut bounds = Vec::with_capacity(self.slices);
        for s in 0..self.slices {
            let (r0, r1) = (s * hctb / self.slices, (s + 1) * hctb / self.slices);
            if r1 > r0 {
                bounds.push((r0, r1));
            }
        }
        let mut bands = Vec::with_capacity(bounds.len());
        {
            let [ry, rcb, rcr] = &mut self.rec;
            let (mut ly, mut lcb, mut lcr, mut lb) = (ry.as_mut_slice(), rcb.as_mut_slice(), rcr.as_mut_slice(), self.blk.as_mut_slice());
            for &(r0, r1) in &bounds {
                let (y0, y1) = (r0 * ctb, (r1 * ctb).min(cdh));
                let rows = y1 - y0;
                let (a, rest) = ly.split_at_mut((rows * cdw).min(ly.len()));
                ly = rest;
                let crow = (rows / 2) * (cdw / 2);
                let (b, rest) = lcb.split_at_mut(crow.min(lcb.len()));
                lcb = rest;
                let (c, rest) = lcr.split_at_mut(crow.min(lcr.len()));
                lcr = rest;
                let brow = rows.div_ceil(4) * w4;
                let (d, rest) = lb.split_at_mut(brow.min(lb.len()));
                lb = rest;
                bands.push((r0, r1, Band { y0, h: rows, rec: [a, b, c], blk: d }));
            }
        }
        let init_type = if key { 0 } else { 1 };
        let ctx = cabac::init_contexts(qp, init_type);
        let code_slice = |(r0, r1, band): (usize, usize, Band)| -> (usize, Vec<u8>) {
            let mut se = SliceEncoder::new(&shared, band, ctx);
            for ry in r0..r1 {
                for rx in 0..wctb {
                    se.encode_ctu((rx, ry));
                    se.end_ctu(ry + 1 == r1 && rx + 1 == wctb);
                }
            }
            (r0, se.finish())
        };
        #[cfg(feature = "threads")]
        let datas: Vec<(usize, Vec<u8>)> = {
            use rayon::prelude::*;
            bands.into_par_iter().map(code_slice).collect()
        };
        #[cfg(not(feature = "threads"))]
        let datas: Vec<(usize, Vec<u8>)> = bands.into_iter().map(code_slice).collect();
        let pic_ctbs = (wctb * hctb) as u32;
        let address_bits = 32 - pic_ctbs.saturating_sub(1).leading_zeros();
        let mut data = Vec::new();
        if let (true, Some(sei)) = (key, &self.sei) {
            data.extend_from_slice(&(sei.len() as u32).to_be_bytes());
            data.extend_from_slice(sei);
        }
        for (r0, slice_data) in datas {
            let mut bw = filmcraft_bitstream::BitWriter::new();
            let header = SliceHeader { first_in_pic: r0 == 0, idr: key, address: (r0 * wctb) as u32, address_bits, intra: key, poc: self.poc, qp };
            header.write(&mut bw);
            let mut rbsp = bw.finish();
            rbsp.extend_from_slice(&slice_data);
            let unit = nal::nal(if key { NAL_IDR_W_RADL } else { NAL_TRAIL_R }, &rbsp);
            data.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            data.extend_from_slice(&unit);
        }
        data
    }

    /// Code one picture.
    pub fn encode(&mut self, f: &Frame) -> Result<Packet, Error> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        let (cw, ch) = (w / 2, h / 2);
        let fits = |p: &[u16], stride: usize, pw: usize, ph: usize| stride >= pw && p.len() >= stride.saturating_mul(ph - 1).saturating_add(pw);
        if !fits(f.y, f.y_stride, w, h) || !fits(f.u, f.uv_stride, cw, ch) || !fits(f.v, f.uv_stride, cw, ch) {
            return Err(Error::BadFrame);
        }
        let (cdw, cdh) = (self.seq.coded_w as usize, self.seq.coded_h as usize);
        let max = (1u16 << self.cfg.bit_depth) - 1;
        let src = [
            coded_plane(f.y, f.y_stride, w, h, cdw, cdh, max),
            coded_plane(f.u, f.uv_stride, cw, ch, cdw / 2, cdh / 2, max),
            coded_plane(f.v, f.uv_stride, cw, ch, cdw / 2, cdh / 2, max),
        ];
        let keyint = u64::from(self.cfg.keyint.max(1));
        let key = self.refp.is_none() || self.index.is_multiple_of(keyint);
        if key {
            self.poc = 0;
        }
        let src = [&*src[0], &*src[1], &*src[2]];
        // a picture that would drain the decoder buffer (one the rate control could not foresee,
        // such as the first of a far busier scene) is coded again at a coarser QP, at most twice
        let mut qp = self.rc.frame_qp(key);
        let mut retries = 0;
        let data = loop {
            let data = self.code_picture(src, key, qp);
            match self.rc.retry_qp((data.len() * 8) as f64, qp) {
                Some(coarser) if retries < 2 => {
                    qp = coarser;
                    retries += 1;
                }
                _ => break data,
            }
        };
        self.rc.update((data.len() * 8) as f64, key, qp);
        // the reconstruction becomes the next picture's reference, in the reference's own buffers
        let rec = &self.rec;
        match &mut self.refp {
            Some(r) => r.iter_mut().zip(rec).for_each(|(p, plane)| p.refill(plane)),
            None => {
                self.refp = Some([
                    Padded::new(&rec[0], cdw, cdh, PAD),
                    Padded::new(&rec[1], cdw / 2, cdh / 2, PAD / 2),
                    Padded::new(&rec[2], cdw / 2, cdh / 2, PAD / 2),
                ])
            }
        }
        let packet = Packet { data, key, index: self.index };
        self.index += 1;
        self.poc += 1;
        Ok(packet)
    }
}

fn default_slices() -> usize {
    #[cfg(feature = "threads")]
    {
        rayon::current_num_threads().max(1)
    }
    #[cfg(not(feature = "threads"))]
    {
        1
    }
}

/// A `w` x `h` plane (stride `stride`) at the coded size `cw` x `ch`: the plane itself when it
/// already is (tight rows, no sample above `max`), else a padded, clamped copy ([`pad_plane`]).
fn coded_plane(p: &[u16], stride: usize, w: usize, h: usize, cw: usize, ch: usize, max: u16) -> std::borrow::Cow<'_, [u16]> {
    match p.get(..w * h) {
        Some(tight) if (w, h) == (cw, ch) && stride == w && tight.iter().all(|&s| s <= max) => std::borrow::Cow::Borrowed(tight),
        _ => std::borrow::Cow::Owned(pad_plane(p, stride, w, h, cw, ch, max)),
    }
}

/// A `w` x `h` plane (stride `stride`) copied to `cw` x `ch`, the extra columns and rows replicated
/// from the last ones and samples clamped to `max`.
fn pad_plane(p: &[u16], stride: usize, w: usize, h: usize, cw: usize, ch: usize, max: u16) -> Vec<u16> {
    let mut out = vec![0u16; cw * ch];
    for y in 0..ch {
        let sy = y.min(h - 1);
        let src = &p[sy * stride..sy * stride + w];
        let row = &mut out[y * cw..y * cw + cw];
        for (x, o) in row.iter_mut().enumerate() {
            *o = src[x.min(w - 1)].min(max);
        }
    }
    out
}
