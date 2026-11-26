//! Effects that look at other frames: Posterize Time, Echo, Auto Reframe and Warp Stabilizer.
//!
//! Warp Stabilizer is a 2-D (similarity / translation) stabilizer: on first use it analyses the
//! clip — every frame decoded small, Shi–Tomasi features tracked frame to frame with pyramidal
//! Lucas–Kanade and a RANSAC similarity fit (the mask tracker in [`crate::track`]) — and caches
//! the per-frame steps in memory. Rendering smooths the accumulated camera path with a Gaussian
//! (Smoothness) or holds it fixed (No Motion), warps each frame onto the smoothed path and frames
//! the result (crop / auto-scale / synthesise edges). Perspective and Subspace Warp use the
//! similarity model (no per-region warping).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use filmcraft_project::{EffectInstance, TrackMethod};
use rayon::prelude::*;

use super::{Affine, FxCtx, FxEnv, Image, Vec2, affine_warp, bv, chv, fv, luma, offv, over};
use crate::track::{Gray, Prepared, track_step};

/// Posterize Time: the clip shows a new frame only at the effect's frame rate.
pub fn posterize_time(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let Some(env) = cx.env else { return };
    let rate = fv(e, "rate", cx).max(0.01) as f64;
    let t = cx.seconds;
    let q = (t * rate + 1e-6).floor() / rate;
    let dt = q - t;
    if dt.abs() < 1e-6 {
        return;
    }
    if let Some(f) = env.frame(dt, e) {
        *img = f;
    }
}

/// Echo: composites copies of the clip from earlier (negative Echo Time) or later frames.
pub fn echo(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let Some(env) = cx.env else { return };
    let dt = fv(e, "time", cx) as f64;
    let n = fv(e, "count", cx).clamp(0.0, 30.0) as usize;
    if n == 0 || dt.abs() < 1e-6 {
        return;
    }
    let start = fv(e, "start", cx).clamp(0.0, 1.0);
    let decay = fv(e, "decay", cx).max(0.0);
    let op = chv(e, "operator");
    let mut layers: Vec<(Image, f32)> = vec![(img.clone(), start)];
    for k in 1..=n {
        if let Some(f) = env.frame(dt * k as f64, e) {
            layers.push((f, start * decay.powi(k as i32)));
        }
    }
    *img = combine_echoes(&layers, op);
}

/// Combine echo layers (index 0 = the current frame) with an Echo Operator.
pub(crate) fn combine_echoes(layers: &[(Image, f32)], op: u32) -> Image {
    let (w, h) = (layers[0].0.w, layers[0].0.h);
    let mut out = Image::new(w, h);
    let wsum: f32 = layers.iter().map(|l| l.1).sum::<f32>().max(1e-6);
    out.px.par_chunks_mut(4).enumerate().for_each(|(i, o)| {
        let px = |l: &(Image, f32)| -> [f32; 4] {
            if l.0.w != w || l.0.h != h {
                return [0.0; 4];
            }
            let p = &l.0.px[i * 4..i * 4 + 4];
            [p[0] * l.1, p[1] * l.1, p[2] * l.1, p[3] * l.1]
        };
        let mut acc = match op {
            2 => [f32::INFINITY; 4],
            3 => [1.0; 4],
            _ => [0.0; 4],
        };
        match op {
            4 => {
                // composite in back: the current frame in front, older echoes behind
                for l in layers.iter().rev() {
                    over(&mut acc, px(l));
                }
            }
            5 => {
                // composite in front: echoes over the current frame
                for l in layers {
                    over(&mut acc, px(l));
                }
            }
            _ => {
                for l in layers {
                    let p = px(l);
                    for k in 0..4 {
                        acc[k] = match op {
                            1 => acc[k].max(p[k]),
                            2 => acc[k].min(p[k]),
                            3 => acc[k] * (1.0 - p[k].min(1.0)),
                            6 => acc[k] + p[k] / wsum,
                            _ => acc[k] + p[k],
                        };
                    }
                }
                if op == 3 {
                    acc = acc.map(|v| 1.0 - v);
                }
            }
        }
        if op == 0 {
            acc[3] = acc[3].min(1.0);
        }
        o.copy_from_slice(&acc.map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 }));
    });
    out
}

// ------------------------------------------------------------------ analysis helpers

fn gray_of(img: &Image) -> Gray {
    let px = img.px.par_chunks(4).map(|p| filmcraft_color::linear_to_srgb(luma([p[0], p[1], p[2]]).clamp(0.0, 1.0))).collect();
    Gray::new(img.w, img.h, px)
}

/// Saliency centroid (0..1 of the frame): gradient energy weighted, mildly centre-biased.
pub(crate) fn saliency_centroid(img: &Image) -> (f32, f32) {
    let g = gray_of(img);
    let (w, h) = (g.w, g.h);
    let (mut sx, mut sy, mut sw) = (0.0f64, 0.0f64, 0.0f64);
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let gx = g.px[y * w + x + 1] - g.px[y * w + x - 1];
            let gy = g.px[(y + 1) * w + x] - g.px[(y - 1) * w + x];
            let (u, v) = (x as f32 / w as f32 - 0.5, y as f32 / h as f32 - 0.5);
            let bias = 1.0 - (u * u + v * v) * 0.8;
            let e = ((gx * gx + gy * gy) * bias.max(0.1)) as f64;
            let e = e * e;
            sx += e * x as f64;
            sy += e * y as f64;
            sw += e;
        }
    }
    if sw <= 1e-12 {
        return (0.5, 0.5);
    }
    (((sx / sw + 0.5) / w as f64) as f32, ((sy / sw + 0.5) / h as f64) as f32)
}

type CentroidCache = Mutex<HashMap<(u64, i64), (f32, f32)>>;
fn centroid_cache() -> &'static CentroidCache {
    static C: OnceLock<CentroidCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Auto Reframe: keeps the subject (a saliency centroid averaged over a time window set by the
/// Motion Preset) in frame while zooming the clip to fill the sequence (or a chosen aspect).
pub fn auto_reframe(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let (lw, lh) = (img.w as f64, img.h as f64);
    // the frame the layer must fill, in layer pixels (Motion at its defaults places the layer
    // centred in the sequence 1:1)
    let target = match (chv(e, "aspect"), cx.env) {
        (0, Some(env)) => {
            let (sw, sh) = env.sequence_size();
            let (src_w, _) = env.source_size();
            let k = lw / src_w.max(1) as f64;
            (sw as f64 * k, sh as f64 * k)
        }
        (0, None) => return,
        (a, _) => {
            let r = [16.0 / 9.0, 9.0 / 16.0, 1.0, 4.0 / 5.0, 16.0 / 9.0][a.min(4) as usize];
            if lw / lh > r { (lh * r, lh) } else { (lw, lw / r) }
        }
    };
    let zoom_user = (fv(e, "zoom", cx) as f64 / 100.0).max(1.0);
    let z = (target.0 / lw).max(target.1 / lh).max(1.0) * zoom_user;
    // window of the source that stays visible
    let (vw, vh) = (target.0.min(lw * z) / z, target.1.min(lh * z) / z);
    let (half, step) = [(1.0, 0.25), (0.5, 0.125), (0.15, 0.05)][chv(e, "preset").min(2) as usize];
    let subject = match cx.env {
        Some(env) => {
            let key = env.clip_key();
            let fps = env.frame_rate().max(1.0);
            let mut acc = (0.0f64, 0.0f64, 0.0f64);
            let mut k = -half;
            while k <= half + 1e-9 {
                let t = cx.seconds + k;
                if t >= -1e-9 && t <= env.clip_seconds() + 1e-9 {
                    let id = (t * fps).round() as i64;
                    let cached = centroid_cache().lock().ok().and_then(|c| c.get(&(key, id)).copied());
                    let c = cached.or_else(|| {
                        let f = env.source_frame(id as f64 / fps - cx.seconds, (160.0 / env.source_size().0.max(1) as f32).min(1.0))?;
                        let c = saliency_centroid(&f);
                        if let Ok(mut m) = centroid_cache().lock() {
                            if m.len() > 100_000 {
                                m.clear();
                            }
                            m.insert((key, id), c);
                        }
                        Some(c)
                    });
                    if let Some((u, v)) = c {
                        let wgt = 1.0 - (k / (half + step)).abs();
                        acc = (acc.0 + u as f64 * wgt, acc.1 + v as f64 * wgt, acc.2 + wgt);
                    }
                }
                k += step;
            }
            if acc.2 > 0.0 { (acc.0 / acc.2, acc.1 / acc.2) } else { (0.5, 0.5) }
        }
        None => {
            let (u, v) = saliency_centroid(&img.downsample2());
            (u as f64, v as f64)
        }
    };
    let off = offv(e, "offset", cx);
    let sx = (subject.0 * lw + off.x).clamp(vw / 2.0, lw - vw / 2.0);
    let sy = (subject.1 * lh + off.y).clamp(vh / 2.0, lh - vh / 2.0);
    // output(p) = input(subject + (p − centre) / z)
    let m = Affine::translate(lw / 2.0, lh / 2.0).then_apply(&Affine::scale(z, z)).then_apply(&Affine::translate(-sx, -sy));
    if (z - 1.0).abs() < 1e-9 && (sx - lw / 2.0).abs() < 1e-6 && (sy - lh / 2.0).abs() < 1e-6 {
        return;
    }
    affine_warp(img, &m);
}

// ------------------------------------------------------------------ Warp Stabilizer

/// A clip's analysed camera motion: per-frame steps (frame k → k+1) in analysis pixels.
#[derive(Clone, Debug)]
pub struct StabilizerPath {
    pub w: usize,
    pub h: usize,
    pub steps: Vec<Affine>,
    pub fps: f64,
}

type PathCache = Mutex<HashMap<u64, Arc<StabilizerPath>>>;
fn path_cache() -> &'static PathCache {
    static C: OnceLock<PathCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Forget every cached stabilizer analysis (e.g. after the media changed).
pub fn clear_stabilizer_cache() {
    if let Ok(mut c) = path_cache().lock() {
        c.clear();
    }
}

/// Frames decoded, prepared and tracked together; the analysis holds this many (plus one carried
/// over from the block before) at a time, whatever the clip's length.
pub(super) const ANALYSIS_BLOCK: usize = 64;

/// The step between each pair of consecutive frames of `n`, and the size of the frames. `decode`
/// makes frame `k` (`None` when it can't be read: its steps are the identity), `size` is a frame's
/// (width, height), `step` the motion from one frame to the next given the size of the first frame
/// that could be read. Frames are made `block` at a time, in parallel, and dropped once the steps
/// they take part in are known; only the last frame of a block stays for the next one. `None` when
/// cancelled or when no frame could be read.
pub(super) fn chunked_steps<T: Send + Sync>(
    n: usize,
    block: usize,
    decode: impl Fn(usize) -> Option<T> + Sync,
    size: impl Fn(&T) -> (usize, usize) + Sync,
    step: impl Fn(&T, &T, (usize, usize)) -> Affine + Sync,
    cancelled: impl Fn() -> bool + Sync,
) -> Option<(Vec<Affine>, (usize, usize))> {
    let block = block.max(1);
    let mut steps = Vec::with_capacity(n.saturating_sub(1));
    let mut dims = None;
    let mut carried: Option<Option<T>> = None;
    let mut next = 0;
    while next < n {
        if cancelled() {
            return None;
        }
        let end = (next + block).min(n);
        let fresh: Vec<Option<T>> = (next..end).into_par_iter().map(|k| if cancelled() { None } else { decode(k) }).collect();
        if cancelled() {
            return None;
        }
        let mut frames: Vec<Option<T>> = Vec::with_capacity(fresh.len() + 1);
        frames.extend(carried.take());
        frames.extend(fresh);
        if dims.is_none() {
            dims = frames.iter().flatten().next().map(&size);
        }
        let pairs: Vec<Affine> = (0..frames.len().saturating_sub(1))
            .into_par_iter()
            .map(|i| match (&frames[i], &frames[i + 1], dims) {
                (Some(a), Some(b), Some(d)) if size(a).0 == size(b).0 => step(a, b, d),
                _ => Affine::IDENTITY,
            })
            .collect();
        steps.extend(pairs);
        carried = frames.pop();
        next = end;
    }
    Some((steps, dims?))
}

/// Analyse (or fetch the cached analysis of) the clip `env` describes.
pub fn stabilizer_path(env: &dyn FxEnv, now: f64, method: TrackMethod, detailed: bool) -> Option<Arc<StabilizerPath>> {
    let key = env.clip_key() ^ (method as u64).wrapping_mul(0x9E37) ^ (detailed as u64).wrapping_mul(0x51_7CC1);
    if let Some(p) = path_cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Some(p);
    }
    let fps = env.frame_rate().max(1.0);
    let n = ((env.clip_seconds() * fps).round() as usize).clamp(1, 20_000);
    let src_w = env.source_size().0.max(1) as f32;
    let target_w = if detailed { 960.0 } else { 480.0 };
    let scale = (target_w / src_w).min(1.0);
    // A few frames at a time: holding the pyramid of every frame of a long clip (a 10-minute clip
    // at 30 fps is 18,000 of them) took tens of gigabytes and ended the process (#1381).
    let (steps, (w, h)) = chunked_steps(
        n,
        ANALYSIS_BLOCK,
        |k| env.source_frame(k as f64 / fps - now, scale).map(|f| Prepared::new(gray_of(&f))),
        |p: &Prepared| (p.base().w, p.base().h),
        |a: &Prepared, b: &Prepared, (w, h)| {
            let m = 8.0;
            let region = [Vec2::new(m, m), Vec2::new(w as f64 - m, m), Vec2::new(w as f64 - m, h as f64 - m), Vec2::new(m, h as f64 - m)];
            track_step(a, b, &region, method).map(|s| s.transform).unwrap_or(Affine::IDENTITY)
        },
        filmcraft_media::cancel::cancelled,
    )?;
    let path = Arc::new(StabilizerPath { w, h, steps, fps });
    if let Ok(mut c) = path_cache().lock() {
        if c.len() > 64 {
            c.clear();
        }
        c.insert(key, path.clone());
    }
    Some(path)
}

/// Similarity parameters (tx, ty, angle, log-scale) of an affine.
fn decompose(m: &Affine) -> [f64; 4] {
    let s = (m.a * m.a + m.b * m.b).sqrt().max(1e-9);
    [m.e, m.f, m.b.atan2(m.a), s.ln()]
}
fn compose(p: [f64; 4]) -> Affine {
    let s = p[3].exp();
    let (sn, cs) = p[2].sin_cos();
    Affine { a: s * cs, b: s * sn, c: -s * sn, d: s * cs, e: p[0], f: p[1] }
}

/// Widest smoothing kernel (Gaussian sigma, in frames) Warp Stabilizer applies: 50 s at 60 fps.
const MAX_SMOOTH_SIGMA: f64 = 3_000.0;

/// Per-frame correction transforms (current frame px → stabilised px, analysis pixels).
pub(crate) fn corrections(path: &StabilizerPath, smooth_frames: f64, no_motion: bool, preserve_scale: bool) -> Vec<Affine> {
    let n = path.steps.len() + 1;
    // cumulative: frame 0 → frame k, about the frame centre for stable angle/scale decomposition
    let (cxp, cyp) = (path.w as f64 / 2.0, path.h as f64 / 2.0);
    let to_c = Affine::translate(-cxp, -cyp);
    let from_c = Affine::translate(cxp, cyp);
    let mut cum = vec![Affine::IDENTITY; n];
    for k in 1..n {
        cum[k] = path.steps[k - 1].then_apply(&cum[k - 1]);
    }
    let params: Vec<[f64; 4]> = cum.iter().map(|c| decompose(&to_c.then_apply(c).then_apply(&from_c))).collect();
    // unwrap angles
    let mut params = params;
    for k in 1..n {
        let d = params[k][2] - params[k - 1][2];
        let wrap = (d / std::f64::consts::TAU).round() * std::f64::consts::TAU;
        params[k][2] -= wrap;
    }
    let smoothed: Vec<[f64; 4]> = if no_motion {
        let mean = params.iter().fold([0.0; 4], |a, p| [a[0] + p[0], a[1] + p[1], a[2] + p[2], a[3] + p[3]]).map(|v| v / n as f64);
        vec![mean; n]
    } else {
        // the loop below runs 6 sigma + 1 times per frame: a project-supplied radius must not size it
        // a NaN smoothness falls back to the minimum (`clamp` would pass the NaN on)
        let sigma = if smooth_frames.is_nan() { 0.01 } else { smooth_frames.clamp(0.01, MAX_SMOOTH_SIGMA) };
        let r = (sigma * 3.0).ceil() as isize;
        (0..n as isize)
            .map(|k| {
                let mut acc = [0.0; 4];
                let mut ws = 0.0;
                for j in -r..=r {
                    let i = (k + j).clamp(0, n as isize - 1) as usize;
                    let w = (-(j as f64).powi(2) / (2.0 * sigma * sigma)).exp();
                    for c in 0..4 {
                        acc[c] += params[i][c] * w;
                    }
                    ws += w;
                }
                acc.map(|v| v / ws)
            })
            .collect()
    };
    (0..n)
        .map(|k| {
            let mut sp = smoothed[k];
            if preserve_scale {
                sp[3] = params[k][3];
            }
            // in centred coordinates: smoothed ∘ raw⁻¹
            let raw = compose(params[k]);
            let sm = compose(sp);
            let corr = raw.inverse().map(|ri| sm.then_apply(&ri)).unwrap_or(Affine::IDENTITY);
            from_c.then_apply(&corr).then_apply(&to_c)
        })
        .collect()
}

/// The corrections of one analysed path for one set of Smoothness / Result / Preserve Scale
/// settings, and the zoom that hides the borders of the worst of them (per `max` zoom allowed).
pub(crate) struct Corrected {
    pub(crate) corr: Vec<Affine>,
    /// Keeps the path the corrections came from alive, so its address can't be reused by another.
    _path: Arc<StabilizerPath>,
    worst: Mutex<Vec<(u64, f64)>>,
}

impl Corrected {
    /// The largest [`cover_scale`] of any correction, with at most `max` zoom: what Crop Less
    /// Smooth Motion framing and Auto-scale need, the same for every frame of the clip.
    pub(crate) fn worst_cover(&self, w: f64, h: f64, max: f64) -> f64 {
        let key = max.to_bits();
        if let Some(v) = self.worst.lock().ok().and_then(|l| l.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)) {
            return v;
        }
        let v = self.corr.iter().map(|c| cover_scale(c, w, h, max)).fold(1.0, f64::max);
        if let Ok(mut l) = self.worst.lock() {
            l.push((key, v));
        }
        v
    }
}

type CorrectedKey = (usize, u64, bool, bool);

/// Corrections already worked out, by path and settings.
#[derive(Default)]
pub(crate) struct CorrectedCache(Mutex<HashMap<CorrectedKey, Arc<Corrected>>>);

impl CorrectedCache {
    pub(crate) fn get(&self, path: &Arc<StabilizerPath>, smooth_frames: f64, no_motion: bool, preserve_scale: bool) -> Arc<Corrected> {
        let key = (Arc::as_ptr(path) as usize, smooth_frames.to_bits(), no_motion, preserve_scale);
        if let Some(c) = self.0.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return c;
        }
        let c = Arc::new(Corrected { corr: corrections(path, smooth_frames, no_motion, preserve_scale), _path: path.clone(), worst: Mutex::new(Vec::new()) });
        if let Ok(mut m) = self.0.lock() {
            // a keyframed Smoothness makes a new key for every frame: don't keep them all
            if m.len() >= 16 {
                m.clear();
            }
            m.insert(key, c.clone());
        }
        c
    }
}

/// [`corrections`] for `path`, computed once per set of settings instead of for every rendered
/// frame: smoothing the whole path and finding its worst border took 0.8 ms per frame for a 30 s
/// clip, 7 ms for 2 minutes and 15 ms for 10 minutes, on every frame of playback and export.
pub(crate) fn corrected(path: &Arc<StabilizerPath>, smooth_frames: f64, no_motion: bool, preserve_scale: bool) -> Arc<Corrected> {
    static CACHE: OnceLock<CorrectedCache> = OnceLock::new();
    CACHE.get_or_init(Default::default).get(path, smooth_frames, no_motion, preserve_scale)
}
/// Smallest zoom about the centre (≥ 1) that hides the borders a correction exposes.
pub(crate) fn cover_scale(corr: &Affine, w: f64, h: f64, max: f64) -> f64 {
    let Some(inv) = corr.inverse() else { return 1.0 };
    let (cx, cy) = (w / 2.0, h / 2.0);
    let covered = |z: f64| {
        [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].iter().all(|&(x, y)| {
            let q = inv.apply(Vec2::new(cx + (x - cx) / z, cy + (y - cy) / z));
            q.x >= -0.5 && q.y >= -0.5 && q.x <= w + 0.5 && q.y <= h + 0.5
        })
    };
    if covered(1.0) {
        return 1.0;
    }
    let (mut lo, mut hi) = (1.0, max.max(1.0));
    if !covered(hi) {
        return hi;
    }
    for _ in 0..30 {
        let mid = (lo + hi) / 2.0;
        if covered(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

/// Warp Stabilizer: see the module docs.
pub fn warp_stabilizer(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let Some(env) = cx.env else { return };
    let method_idx = chv(e, "method");
    if method_idx >= 3 && subspace_warp_stabilizer(img, e, cx, env, method_idx == 4) {
        return;
    }
    let method = if method_idx == 0 { TrackMethod::Position } else { TrackMethod::PositionScaleRotation };
    let Some(path) = stabilizer_path(env, cx.seconds, method, bv(e, "detailed")) else { return };
    let n = path.steps.len() + 1;
    let k = ((cx.seconds * path.fps).round().max(0.0) as usize).min(n - 1);
    let smooth = fv(e, "smoothness", cx) as f64 / 100.0 * path.fps;
    let crop_less = fv(e, "crop_less", cx) as f64 / 100.0;
    let corrected = corrected(&path, smooth * (0.5 + crop_less), chv(e, "result") == 1, bv(e, "preserve_scale"));
    let corr = &corrected.corr;
    let framing = chv(e, "framing");
    let (aw, ah) = (path.w as f64, path.h as f64);
    let max_scale = (fv(e, "max_scale", cx) as f64 / 100.0).max(1.0);
    let safe = 1.0 + fv(e, "action_safe", cx) as f64 / 100.0;
    let extra = fv(e, "additional_scale", cx) as f64 / 100.0;
    let zoom = match framing {
        2 => corrected.worst_cover(aw, ah, max_scale) * safe,
        _ => 1.0,
    } * extra;
    // analysis px → layer px
    let s = img.w as f64 / aw;
    let to_layer = Affine::scale(s, s);
    let from_layer = Affine::scale(1.0 / s, 1.0 / s);
    let (lw, lh) = (img.w as f64, img.h as f64);
    let z = Affine::translate(lw / 2.0, lh / 2.0).then_apply(&Affine::scale(zoom, zoom)).then_apply(&Affine::translate(-lw / 2.0, -lh / 2.0));
    let m = z.then_apply(&to_layer).then_apply(&corr[k]).then_apply(&from_layer);
    let src = img.clone();
    affine_warp(img, &m);
    match framing {
        1 => {
            // crop to the area every frame covers
            let worst = corrected.worst_cover(aw, ah, 4.0);
            let (mx, my) = (lw * (1.0 - 1.0 / worst) / 2.0, lh * (1.0 - 1.0 / worst) / 2.0);
            crate::effects::crop(img, (mx / lw) as f32, (my / lh) as f32, (mx / lw) as f32, (my / lh) as f32, 0.0);
        }
        3 => {
            // synthesise edges: fill uncovered pixels from the clamped source
            if let Some(inv) = m.inverse() {
                let w = img.w;
                img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let p = &mut row[x * 4..x * 4 + 4];
                        if p[3] < 0.999 {
                            let q = inv.apply(Vec2::new(x as f64 + 0.5, y as f64 + 0.5));
                            let fill = src.sample_bilinear_clamped(q.x as f32, q.y as f32);
                            let mut b = fill;
                            over(&mut b, [p[0], p[1], p[2], p[3]]);
                            p.copy_from_slice(&b);
                        }
                    }
                });
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ 3D Subspace Warp Stabilizer

const SUB_GRID: usize = 4;
const SUB_TILES: usize = SUB_GRID * SUB_GRID;

#[derive(Clone, Debug)]
struct SubspacePath {
    w: usize,
    h: usize,
    steps: Vec<[[f64; 2]; SUB_TILES]>,
    fps: f64,
}

type SubPathCache = Mutex<HashMap<u64, Arc<SubspacePath>>>;
fn subpath_cache() -> &'static SubPathCache {
    static C: OnceLock<SubPathCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn tile_steps(a: &Prepared, b: &Prepared, (w, h): (usize, usize)) -> [[f64; 2]; SUB_TILES] {
    let mut out = [[0.0f64; 2]; SUB_TILES];
    let global = track_step(
        a,
        b,
        &[Vec2::new(6.0, 6.0), Vec2::new(w as f64 - 6.0, 6.0), Vec2::new(w as f64 - 6.0, h as f64 - 6.0), Vec2::new(6.0, h as f64 - 6.0)],
        TrackMethod::PositionScaleRotation,
    )
    .map(|s| s.transform)
    .unwrap_or(Affine::IDENTITY);
    let (tw, th) = (w as f64 / SUB_GRID as f64, h as f64 / SUB_GRID as f64);
    let pad_x = tw * 0.25;
    let pad_y = th * 0.25;
    for ty in 0..SUB_GRID {
        for tx in 0..SUB_GRID {
            let idx = ty * SUB_GRID + tx;
            let x0 = (tx as f64 * tw - pad_x).max(4.0);
            let y0 = (ty as f64 * th - pad_y).max(4.0);
            let x1 = ((tx + 1) as f64 * tw + pad_x).min(w as f64 - 4.0);
            let y1 = ((ty + 1) as f64 * th + pad_y).min(h as f64 - 4.0);
            let cx_t = (x0 + x1) * 0.5;
            let cy_t = (y0 + y1) * 0.5;
            let fallback = global.apply(Vec2::new(cx_t, cy_t)) - Vec2::new(cx_t, cy_t);
            let poly = [Vec2::new(x0, y0), Vec2::new(x1, y0), Vec2::new(x1, y1), Vec2::new(x0, y1)];
            let d = track_step(a, b, &poly, TrackMethod::Position).map(|s| Vec2::new(s.transform.e, s.transform.f)).unwrap_or(fallback);
            out[idx] = [d.x * 0.7 + fallback.x * 0.3, d.y * 0.7 + fallback.y * 0.3];
        }
    }
    out
}

fn subspace_path(env: &dyn FxEnv, now: f64, detailed: bool) -> Option<Arc<SubspacePath>> {
    let key = env.clip_key() ^ 0x3D5B_50AC_u64 ^ (detailed as u64).wrapping_mul(0x9E37_79B9);
    if let Some(p) = subpath_cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Some(p);
    }
    let fps = env.frame_rate().max(1.0);
    let n = ((env.clip_seconds() * fps).round() as usize).clamp(1, 20_000);
    let src_w = env.source_size().0.max(1) as f32;
    let scale = ((if detailed { 720.0 } else { 400.0 }) / src_w).min(1.0);
    let mut steps = Vec::with_capacity(n.saturating_sub(1));
    let mut dims = None;
    let mut carried: Option<Option<Prepared>> = None;
    let mut next = 0;
    while next < n {
        if filmcraft_media::cancel::cancelled() {
            return None;
        }
        let end = (next + ANALYSIS_BLOCK).min(n);
        let fresh: Vec<Option<Prepared>> =
            (next..end).into_par_iter().map(|k| env.source_frame(k as f64 / fps - now, scale).map(|f| Prepared::new(gray_of(&f)))).collect();
        let mut frames = Vec::with_capacity(fresh.len() + 1);
        frames.extend(carried.take());
        frames.extend(fresh);
        if dims.is_none() {
            dims = frames.iter().flatten().next().map(|p| (p.base().w, p.base().h));
        }
        let pair_steps: Vec<[[f64; 2]; SUB_TILES]> = (0..frames.len().saturating_sub(1))
            .into_par_iter()
            .map(|i| match (&frames[i], &frames[i + 1], dims) {
                (Some(a), Some(b), Some(d)) if a.base().w == b.base().w => tile_steps(a, b, d),
                _ => [[0.0; 2]; SUB_TILES],
            })
            .collect();
        steps.extend(pair_steps);
        carried = frames.pop();
        next = end;
    }
    let (w, h) = dims?;
    let path = Arc::new(SubspacePath { w, h, steps, fps });
    if let Ok(mut c) = subpath_cache().lock() {
        if c.len() > 32 {
            c.clear();
        }
        c.insert(key, path.clone());
    }
    Some(path)
}

/// Project cumulative `2 * SUB_TILES` trajectories onto a rank-`rank` 3D motion subspace via
/// power iteration, smooth temporal basis coefficients, and return the frame `k` grid offsets.
fn subspace_frame_offsets(path: &SubspacePath, k: usize, smooth_frames: f64, no_motion: bool, rank: usize) -> [[f64; 2]; SUB_TILES] {
    let n = path.steps.len() + 1;
    if n <= 1 {
        return [[0.0; 2]; SUB_TILES];
    }
    let rows = SUB_TILES * 2;
    let mut mat = vec![vec![0.0f64; n]; rows];
    for t in 1..n {
        let s = &path.steps[t - 1];
        for tile in 0..SUB_TILES {
            mat[tile * 2][t] = mat[tile * 2][t - 1] + s[tile][0];
            mat[tile * 2 + 1][t] = mat[tile * 2 + 1][t - 1] + s[tile][1];
        }
    }
    let mut means = vec![0.0f64; rows];
    for r in 0..rows {
        means[r] = mat[r].iter().sum::<f64>() / n as f64;
        for v in &mut mat[r] {
            *v -= means[r];
        }
    }
    let mut residual = mat.clone();
    let mut recon_raw = vec![0.0f64; rows];
    let mut recon_smooth = vec![0.0f64; rows];
    let sigma = if smooth_frames.is_nan() { 0.5 } else { smooth_frames.clamp(0.1, MAX_SMOOTH_SIGMA) };
    let rad = (sigma * 3.0).ceil() as isize;

    for mode in 0..rank.min(rows).min(n) {
        let mut u = vec![0.0f64; rows];
        for (r, uv) in u.iter_mut().enumerate() {
            *uv = if r % (mode + 2) == 0 { 1.0 } else { 0.35 };
        }
        let mut v = vec![0.0f64; n];
        for _ in 0..12 {
            for (t, vt) in v.iter_mut().enumerate() {
                *vt = (0..rows).map(|r| residual[r][t] * u[r]).sum();
            }
            let vn = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            if vn <= 1e-12 {
                break;
            }
            for vt in &mut v {
                *vt /= vn;
            }
            for (r, ur) in u.iter_mut().enumerate() {
                *ur = (0..n).map(|t| residual[r][t] * v[t]).sum();
            }
        }
        let un = u.iter().map(|x| x * x).sum::<f64>().sqrt();
        if un <= 1e-12 {
            break;
        }
        let v_sm_k = if no_motion {
            0.0
        } else {
            let mut acc = 0.0;
            let mut ws = 0.0;
            for j in -rad..=rad {
                let ti = (k as isize + j).clamp(0, n as isize - 1) as usize;
                let w = (-(j as f64).powi(2) / (2.0 * sigma * sigma)).exp();
                acc += v[ti] * w;
                ws += w;
            }
            acc / ws.max(1e-12)
        };
        for r in 0..rows {
            recon_raw[r] += u[r] * v[k];
            recon_smooth[r] += u[r] * v_sm_k;
            for t in 0..n {
                residual[r][t] -= u[r] * v[t];
            }
        }
    }

    let mut raw_grid = [[0.0f64; 2]; SUB_TILES];
    for tile in 0..SUB_TILES {
        raw_grid[tile] = [recon_smooth[tile * 2] - recon_raw[tile * 2], recon_smooth[tile * 2 + 1] - recon_raw[tile * 2 + 1]];
    }
    // Spatial Laplacian regularization so adjacent tiles deform smoothly without folding.
    let mut reg = raw_grid;
    for ty in 0..SUB_GRID {
        for tx in 0..SUB_GRID {
            let mut sx = raw_grid[ty * SUB_GRID + tx][0] * 2.0;
            let mut sy = raw_grid[ty * SUB_GRID + tx][1] * 2.0;
            let mut wt = 2.0;
            for (dx, dy) in [(-1isize, 0), (1, 0), (0, -1isize), (0, 1)] {
                let nx = tx as isize + dx;
                let ny = ty as isize + dy;
                if nx >= 0 && nx < SUB_GRID as isize && ny >= 0 && ny < SUB_GRID as isize {
                    let nb = raw_grid[ny as usize * SUB_GRID + nx as usize];
                    sx += nb[0];
                    sy += nb[1];
                    wt += 1.0;
                }
            }
            reg[ty * SUB_GRID + tx] = [sx / wt, sy / wt];
        }
    }
    reg
}

fn sample_subspace_grid(offsets: &[[f64; 2]; SUB_TILES], u: f64, v: f64) -> [f64; 2] {
    let gx = (u * SUB_GRID as f64 - 0.5).clamp(0.0, (SUB_GRID - 1) as f64);
    let gy = (v * SUB_GRID as f64 - 0.5).clamp(0.0, (SUB_GRID - 1) as f64);
    let x0 = (gx.floor() as usize).min(SUB_GRID - 1);
    let y0 = (gy.floor() as usize).min(SUB_GRID - 1);
    let x1 = (x0 + 1).min(SUB_GRID - 1);
    let y1 = (y0 + 1).min(SUB_GRID - 1);
    let (fx, fy) = (gx - x0 as f64, gy - y0 as f64);
    let a = offsets[y0 * SUB_GRID + x0];
    let b = offsets[y0 * SUB_GRID + x1];
    let c = offsets[y1 * SUB_GRID + x0];
    let d = offsets[y1 * SUB_GRID + x1];
    [(a[0] + (b[0] - a[0]) * fx) * (1.0 - fy) + (c[0] + (d[0] - c[0]) * fx) * fy, (a[1] + (b[1] - a[1]) * fx) * (1.0 - fy) + (c[1] + (d[1] - c[1]) * fx) * fy]
}

fn subspace_warp_stabilizer(img: &mut Image, e: &EffectInstance, cx: &FxCtx, env: &dyn FxEnv, rank3: bool) -> bool {
    let Some(path) = subspace_path(env, cx.seconds, bv(e, "detailed")) else { return false };
    let n = path.steps.len() + 1;
    let k = ((cx.seconds * path.fps).round().max(0.0) as usize).min(n - 1);
    let smooth = fv(e, "smoothness", cx) as f64 / 100.0 * path.fps;
    let crop_less = fv(e, "crop_less", cx) as f64 / 100.0;
    let offsets = subspace_frame_offsets(&path, k, smooth * (0.5 + crop_less), chv(e, "result") == 1, if rank3 { 3 } else { 2 });
    let s_x = img.w as f64 / path.w.max(1) as f64;
    let s_y = img.h as f64 / path.h.max(1) as f64;
    let max_disp = offsets.iter().fold(0.0f64, |m, o| m.max((o[0] * s_x).abs() / img.w.max(1) as f64).max((o[1] * s_y).abs() / img.h.max(1) as f64));
    let framing = chv(e, "framing");
    let max_scale = (fv(e, "max_scale", cx) as f64 / 100.0).max(1.0);
    let safe = 1.0 + fv(e, "action_safe", cx) as f64 / 100.0;
    let extra = fv(e, "additional_scale", cx) as f64 / 100.0;
    let auto_zoom = if framing == 2 { (1.0 / (1.0 - 2.0 * max_disp.min(0.35))).clamp(1.0, max_scale) * safe } else { 1.0 } * extra;

    let src = img.clone();
    let (w, h) = (img.w, img.h);
    let (cxp, cyp) = (w as f64 * 0.5, h as f64 * 0.5);
    let clamp_edges = framing == 3;
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let v = (y as f64 + 0.5) / h.max(1) as f64;
        for x in 0..w {
            let u = (x as f64 + 0.5) / w.max(1) as f64;
            let zx = cxp + (x as f64 + 0.5 - cxp) / auto_zoom.max(0.05);
            let zy = cyp + (y as f64 + 0.5 - cyp) / auto_zoom.max(0.05);
            let off = sample_subspace_grid(&offsets, u, v);
            // Inverse map: destination (zx, zy) samples source at (zx - off.x, zy - off.y)
            let sx = (zx - off[0] * s_x) as f32;
            let sy = (zy - off[1] * s_y) as f32;
            let p = if clamp_edges { src.sample_bilinear_clamped(sx, sy) } else { src.sample_bilinear(sx, sy) };
            row[x * 4..x * 4 + 4].copy_from_slice(&p);
        }
    });
    if framing == 1 && max_disp > 1e-4 {
        let m = max_disp.min(0.35) as f32;
        crate::effects::crop(img, m, m, m, m, 0.0);
    }
    true
}

// ------------------------------------------------------------------ Generative Object Removal

fn dilate_binary(mask: &[bool], w: usize, h: usize, radius: usize) -> Vec<bool> {
    if radius == 0 || w == 0 || h == 0 {
        return mask.to_vec();
    }
    let r = radius.min(32) as isize;
    let mut h_pass = vec![false; w * h];
    h_pass.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, out) in row.iter_mut().enumerate() {
            let x0 = (x as isize - r).max(0) as usize;
            let x1 = (x as isize + r).min(w as isize - 1) as usize;
            *out = mask[y * w + x0..=y * w + x1].iter().any(|&b| b);
        }
    });
    let mut out = vec![false; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let y0 = (y as isize - r).max(0) as usize;
        let y1 = (y as isize + r).min(h as isize - 1) as usize;
        for (x, dst) in row.iter_mut().enumerate() {
            *dst = (y0..=y1).any(|yy| h_pass[yy * w + x]);
        }
    });
    out
}

fn hole_mask_for(img: &Image, e: &EffectInstance, t: filmcraft_time::Tick, mask_scale: f32, expand_px: usize) -> Option<Vec<bool>> {
    let (w, h) = (img.w, img.h);
    if w == 0 || h == 0 {
        return None;
    }
    let raw: Vec<bool> = if let Some(cov) = crate::mask::effect_coverage(&e.masks, t, mask_scale, w, h) {
        cov.into_iter().map(|c| c > 0.02).collect()
    } else {
        let has_alpha_hole = img.px.as_chunks::<4>().0.iter().any(|p| p[3] < 0.98) && img.px.as_chunks::<4>().0.iter().any(|p| p[3] >= 0.98);
        if !has_alpha_hole {
            return None;
        }
        img.px.as_chunks::<4>().0.iter().map(|p| p[3] < 0.5).collect()
    };
    if !raw.iter().any(|&b| b) || raw.iter().all(|&b| b) {
        return None;
    }
    let dilated = dilate_binary(&raw, w, h, expand_px);
    (!dilated.iter().all(|&b| b)).then_some(dilated)
}

fn bg_poly(w: usize, h: usize) -> [Vec2; 4] {
    let m = 4.0;
    [Vec2::new(m, m), Vec2::new(w as f64 - m, m), Vec2::new(w as f64 - m, h as f64 - m), Vec2::new(m, h as f64 - m)]
}

fn boundary_mean_luma(img: &Image, hole: &[bool], ring: &[bool]) -> f32 {
    let mut sum = 0.0f32;
    let mut cnt = 0u32;
    for (i, (&is_ring, &is_hole)) in ring.iter().zip(hole.iter()).enumerate() {
        if is_ring && !is_hole {
            let p = &img.px[i * 4..i * 4 + 4];
            if p[3] > 0.1 {
                sum += luma([p[0], p[1], p[2]]);
                cnt += 1;
            }
        }
    }
    if cnt > 0 { (sum / cnt as f32).max(1e-4) } else { 0.25 }
}

/// Generative Object Removal: multi-frame motion-compensated background disocclusion +
/// coarse-to-fine exemplar patch & harmonic diffusion inpainting inside masked regions.
pub fn generative_object_removal(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let expand = (fv(e, "expand", cx) * cx.px_scale).clamp(0.0, 48.0).round() as usize;
    let Some(mut filled_hole) = hole_mask_for(img, e, cx.t, cx.mask_scale, expand) else { return };
    let (w, h) = (img.w, img.h);
    let ring = dilate_binary(&filled_hole, w, h, 4);
    let target_luma = boundary_mean_luma(img, &filled_hole, &ring);
    let light_adapt = (fv(e, "lighting_adapt", cx) / 100.0).clamp(0.0, 1.0);

    // 1. Temporal optical-flow / background-aligned disocclusion from neighboring frames.
    if chv(e, "mode") == 0
        && let Some(env) = cx.env
    {
        let fps = env.frame_rate().max(1.0);
        let radius = fv(e, "temporal_radius", cx).clamp(1.0, 12.0).round() as i32;
        let mut cur_bg = gray_of(img);
        for (px, &is_h) in cur_bg.px.iter_mut().zip(&filled_hole) {
            if is_h {
                *px = 0.0;
            }
        }
        let prep_cur = Prepared::new(cur_bg);
        let mut acc = vec![[0.0f32; 4]; w * h];
        let mut wgt = vec![0.0f32; w * h];
        let poly = bg_poly(w, h);

        for step in 1..=radius {
            for sign in [-1.0f64, 1.0] {
                let dt = sign * step as f64 / fps;
                let Some(cand) = env.frame(dt, e) else { continue };
                if cand.w != w || cand.h != h {
                    continue;
                }
                let t_cand = cx.t + filmcraft_time::Tick::from_seconds_f64(dt);
                let cand_hole = hole_mask_for(&cand, e, t_cand, cx.mask_scale, expand + 2).unwrap_or_else(|| vec![false; w * h]);
                let mut cand_bg = gray_of(&cand);
                for (px, &is_h) in cand_bg.px.iter_mut().zip(&cand_hole) {
                    if is_h {
                        *px = 0.0;
                    }
                }
                let prep_cand = Prepared::new(cand_bg);
                let m = track_step(&prep_cur, &prep_cand, &poly, TrackMethod::PositionScaleRotation).map(|s| s.transform).unwrap_or(Affine::IDENTITY);
                let cand_luma = boundary_mean_luma(&cand, &cand_hole, &dilate_binary(&cand_hole, w, h, 4));
                let gain = 1.0 + ((target_luma / cand_luma).clamp(0.5, 2.0) - 1.0) * light_adapt;
                let temporal_w = 1.0 / step as f32;

                for y in 0..h {
                    for x in 0..w {
                        let i = y * w + x;
                        if !filled_hole[i] {
                            continue;
                        }
                        let q = m.apply(Vec2::new(x as f64 + 0.5, y as f64 + 0.5));
                        let (qx, qy) = (q.x.round() as isize, q.y.round() as isize);
                        if qx < 0 || qy < 0 || qx >= w as isize || qy >= h as isize {
                            continue;
                        }
                        if cand_hole[qy as usize * w + qx as usize] {
                            continue;
                        }
                        let s = cand.sample_bilinear_clamped(q.x as f32, q.y as f32);
                        acc[i][0] += s[0] * gain * temporal_w;
                        acc[i][1] += s[1] * gain * temporal_w;
                        acc[i][2] += s[2] * gain * temporal_w;
                        acc[i][3] += s[3] * temporal_w;
                        wgt[i] += temporal_w;
                    }
                }
            }
        }
        for i in 0..w * h {
            if filled_hole[i] && wgt[i] > 0.0 {
                let inv = 1.0 / wgt[i];
                img.px[i * 4..i * 4 + 4].copy_from_slice(&[acc[i][0] * inv, acc[i][1] * inv, acc[i][2] * inv, (acc[i][3] * inv).clamp(0.0, 1.0)]);
                filled_hole[i] = false;
            }
        }
    }

    // 2. Spatial exemplar ring + iterative harmonic diffusion for any remaining occluded pixels.
    spatial_patch_diffusion_inpaint(img, &filled_hole);
}

fn spatial_patch_diffusion_inpaint(img: &mut Image, hole: &[bool]) {
    if !hole.iter().any(|&b| b) {
        return;
    }
    let (w, h) = (img.w, img.h);
    // Collect donor pixels along the outer boundary ring so holes initialize with realistic colors & texture.
    let outer = dilate_binary(hole, w, h, 3);
    let donors: Vec<(usize, usize, [f32; 4])> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| outer[y * w + x] && !hole[y * w + x])
        .step_by(2)
        .map(|(x, y)| {
            let i = (y * w + x) * 4;
            (x, y, [img.px[i], img.px[i + 1], img.px[i + 2], img.px[i + 3]])
        })
        .collect();
    if donors.is_empty() {
        return;
    }

    // Distance-weighted Shepard boundary initialization + subtle donor micro-texture.
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            if !hole[y * w + x] {
                continue;
            }
            let mut sum = [0.0f32; 4];
            let mut ws = 0.0f32;
            let mut nearest_d2 = f32::INFINITY;
            let mut nearest_col = [0.0f32; 4];
            for &(dx, dy, col) in &donors {
                let d2 = ((x as f32 - dx as f32).powi(2) + (y as f32 - dy as f32).powi(2)).max(1.0);
                let wgt = 1.0 / (d2 * d2);
                for k in 0..4 {
                    sum[k] += col[k] * wgt;
                }
                ws += wgt;
                if d2 < nearest_d2 {
                    nearest_d2 = d2;
                    nearest_col = col;
                }
            }
            let inv = 1.0 / ws.max(1e-12);
            let smooth_c = [sum[0] * inv, sum[1] * inv, sum[2] * inv, (sum[3] * inv).clamp(0.0, 1.0)];
            // Preserve subtle local grain from mirrored donor outside the hole.
            let donor_pick = &donors[(x.wrapping_mul(73) ^ y.wrapping_mul(179)) % donors.len()].2;
            let tex_mix = 0.12f32;
            let dst = &mut row[x * 4..x * 4 + 4];
            for k in 0..3 {
                let detail = (donor_pick[k] - nearest_col[k]).clamp(-0.08, 0.08);
                dst[k] = (smooth_c[k] + detail * tex_mix).max(0.0);
            }
            dst[3] = smooth_c[3];
        }
    });

    // Multi-pass harmonic Gauss-Seidel / Jacobi relaxation for C1-smooth gradients across boundary.
    let mut buf = img.px.clone();
    for _ in 0..24 {
        buf.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let idx = y * w + x;
                if !hole[idx] {
                    row[x * 4..x * 4 + 4].copy_from_slice(&img.px[idx * 4..idx * 4 + 4]);
                    continue;
                }
                let mut acc = [0.0f32; 4];
                let mut cnt = 0.0f32;
                for (ox, oy, wgt) in [(-1isize, 0isize, 1.0f32), (1, 0, 1.0), (0, -1, 1.0), (0, 1, 1.0), (0, 0, 0.5)] {
                    let nx = (x as isize + ox).clamp(0, w as isize - 1) as usize;
                    let ny = (y as isize + oy).clamp(0, h as isize - 1) as usize;
                    let ni = (ny * w + nx) * 4;
                    for k in 0..4 {
                        acc[k] += img.px[ni + k] * wgt;
                    }
                    cnt += wgt;
                }
                for k in 0..4 {
                    row[x * 4 + k] = (acc[k] / cnt).max(0.0);
                }
                row[x * 4 + 3] = row[x * 4 + 3].min(1.0);
            }
        });
        std::mem::swap(&mut img.px, &mut buf);
    }
}
