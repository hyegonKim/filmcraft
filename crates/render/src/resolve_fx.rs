//! Clean-room CPU reference implementations of DaVinci Resolve-inspired video & color plugins:
//! - [`dctl_transform`]: Safe parallel `.dctl` (DaVinci Color Transform Language) pixel shader host
//! - [`color_space_transform`]: Camera Log / Wide Gamut CST with SMPTE RP 177 primary matrices & tone/gamut mapping
//! - [`film_halation`]: Multi-scale linear-light red-orange film emulsion scatter
//! - [`color_warper`]: Polar Hue-Chroma spider-web mesh warper with angular/radial pinning
//! - [`qualifier_3d`]: 3D HSL matte keyer with simultaneous Inside / Outside dual grading
//! - [`dynamic_zoom`]: Ease-interpolated Green-to-Red frame Ken Burns zoom

use filmcraft_color::log::LogCurve;
use filmcraft_color::spaces::{D65, Gamut, Mat3, WorkingSpace, apply3, inverse, mul, npm, to_f32};
use filmcraft_color::{hsl_to_rgb, linear_to_srgb, luma709, srgb_to_linear};
use filmcraft_project::{DctlCubeLut, DctlPixelCtx, DctlProgram, EffectInstance, builtin_dctls, parse_dctl};
use rayon::prelude::*;

use crate::effects::{FxCtx, b, choice, f, text};
use crate::image::Image;

/// Try to apply one of the ResolveFX / DCTL / OpenFX effects; returns `true` when `e.effect` matched.
pub fn apply(img: &mut Image, e: &EffectInstance, cx: &FxCtx) -> bool {
    match e.effect.as_str() {
        "dctl_transform" => dctl_transform(img, e, cx),
        "color_space_transform" => color_space_transform(img, e, cx),
        "film_halation" => film_halation(img, e, cx),
        "color_warper" => color_warper(img, e, cx),
        "qualifier_3d" => qualifier_3d(img, e, cx),
        "dynamic_zoom" => dynamic_zoom(img, e, cx),
        _ => return false,
    }
    true
}

#[inline]
pub(crate) fn signed_linear_to_srgb(v: f32) -> f32 {
    if v >= 0.0 { linear_to_srgb(v) } else { -linear_to_srgb(-v) }
}

#[inline]
pub(crate) fn signed_srgb_to_linear(v: f32) -> f32 {
    if v >= 0.0 { srgb_to_linear(v) } else { -srgb_to_linear(-v) }
}

fn resolve_dctl_source<'a>(e: &'a EffectInstance, cx: &'a FxCtx) -> Option<&'a str> {
    let inline = text(e, "custom_code").trim();
    if !inline.is_empty() {
        return Some(inline);
    }
    let pref = text(e, "plugin_ref").trim();
    if let Some(id) = pref.strip_prefix("plugin:") {
        let plug = cx.project?.plugins.iter().find(|p| p.id == id)?;
        if !plug.enabled {
            return None;
        }
        return Some(&plug.code);
    }
    if let Some(id) = pref.strip_prefix("builtin:") {
        return builtin_dctls().iter().find(|(bid, _, _)| *bid == id).map(|(_, _, code)| *code);
    }
    let mode = choice(e, "mode");
    let presets = builtin_dctls();
    match mode {
        1 => presets.first().map(|(_, _, c)| *c),
        2 => presets.get(1).map(|(_, _, c)| *c),
        3 => presets.get(2).map(|(_, _, c)| *c),
        _ => None, // Custom / Linked with neither inline code nor plugin_ref is identity pass-through
    }
}

fn decode_dctl_pixel(px: &[f32], straight: bool) -> [f32; 4] {
    let a = px[3].clamp(0.0, 1.0);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let unpre = [px[0] / a, px[1] / a, px[2] / a];
    let sr = [signed_linear_to_srgb(unpre[0]), signed_linear_to_srgb(unpre[1]), signed_linear_to_srgb(unpre[2])];
    if straight { [sr[0], sr[1], sr[2], a] } else { [sr[0] * a, sr[1] * a, sr[2] * a, a] }
}

fn eval_dctl_program(img: &mut Image, prog: &DctlProgram, params: &[(&str, f32)], luts: &[(&str, &DctlCubeLut)], frame_idx: i64, mix: f32) {
    let (w, h) = (img.w, img.h);
    if w == 0 || h == 0 {
        return;
    }
    let straight_in = !prog.returns_alpha || prog.straight_alpha;
    let src_copy: Option<Vec<f32>> = prog.uses_tex2d.then(|| img.px.clone());

    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let sampler = |sx: isize, sy: isize| -> [f32; 4] {
            let Some(buf) = src_copy.as_deref() else { return [0.0; 4] };
            let cx_i = sx.clamp(0, w as isize - 1) as usize;
            let cy_i = sy.clamp(0, h as isize - 1) as usize;
            let base = (cy_i * w + cx_i) * 4;
            decode_dctl_pixel(&buf[base..base + 4], straight_in)
        };

        for x in 0..w {
            let p = &mut row[x * 4..x * 4 + 4];
            let orig = [p[0], p[1], p[2], p[3]];
            if orig[3] <= 0.0 && !prog.returns_alpha {
                continue;
            }
            let sr = decode_dctl_pixel(&orig, straight_in);
            let pctx = DctlPixelCtx {
                width: w,
                height: h,
                x,
                y,
                rgba: sr,
                to_rgba: sr,
                frame_index: frame_idx,
                transition_progress: 0.0,
                params,
                luts,
                sample_from: prog.uses_tex2d.then_some(&sampler),
                sample_to: None,
            };

            if prog.returns_alpha {
                let out_rgba = prog.eval_pixel(&pctx).into_rgba(sr[3]);
                let out_a = out_rgba[3].clamp(0.0, 1.0);
                let unpre_sr = if prog.straight_alpha {
                    [out_rgba[0], out_rgba[1], out_rgba[2]]
                } else if out_a > 1e-6 {
                    [out_rgba[0] / out_a, out_rgba[1] / out_a, out_rgba[2] / out_a]
                } else {
                    [0.0; 3]
                };
                let lin = [
                    signed_srgb_to_linear(unpre_sr[0].clamp(-4.0, 4.0)) * out_a,
                    signed_srgb_to_linear(unpre_sr[1].clamp(-4.0, 4.0)) * out_a,
                    signed_srgb_to_linear(unpre_sr[2].clamp(-4.0, 4.0)) * out_a,
                ];
                p[0] = orig[0] * (1.0 - mix) + lin[0] * mix;
                p[1] = orig[1] * (1.0 - mix) + lin[1] * mix;
                p[2] = orig[2] * (1.0 - mix) + lin[2] * mix;
                p[3] = orig[3] * (1.0 - mix) + out_a * mix;
            } else {
                let a = orig[3];
                let out_srgb = prog.eval_pixel(&pctx).into_rgb([sr[0], sr[1], sr[2]]);
                let out_lin = [
                    signed_srgb_to_linear(out_srgb[0].clamp(-4.0, 4.0)),
                    signed_srgb_to_linear(out_srgb[1].clamp(-4.0, 4.0)),
                    signed_srgb_to_linear(out_srgb[2].clamp(-4.0, 4.0)),
                ];
                p[0] = orig[0] * (1.0 - mix) + out_lin[0] * a * mix;
                p[1] = orig[1] * (1.0 - mix) + out_lin[1] * a * mix;
                p[2] = orig[2] * (1.0 - mix) + out_lin[2] * a * mix;
            }
        }
    });
}

fn dctl_transform(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let mix = (f(e, "mix", cx) / 100.0).clamp(0.0, 1.0);
    if mix <= 0.0 {
        return;
    }
    let Some(src) = resolve_dctl_source(e, cx) else { return };
    let Ok(prog) = parse_dctl(src) else { return };
    let p1 = f(e, "p1", cx);
    let p2 = f(e, "p2", cx);
    let p3 = f(e, "p3", cx);
    let p4 = f(e, "p4", cx);
    let flag = if b(e, "flag") { 1.0 } else { 0.0 };
    let mut bindings: Vec<(&str, f32)> = vec![("p1", p1), ("p2", p2), ("p3", p3), ("p4", p4), ("apply", flag)];
    for (idx, up) in prog.ui_params.iter().take(4).enumerate() {
        let val = [p1, p2, p3, p4][idx];
        bindings.push((up.name.as_str(), val));
    }
    let mut owned_luts: Vec<(String, DctlCubeLut)> = Vec::new();
    if let Some(proj) = cx.project {
        for (ident, (tag, interp)) in &prog.declared_luts {
            if prog.luts.contains_key(ident) {
                continue;
            }
            let stem = tag.rsplit(['/', '\\']).next().unwrap_or(tag).trim_end_matches(".cube");
            if let Some(plut) = proj.luts.iter().find(|l| l.id == *tag || l.name.eq_ignore_ascii_case(tag) || l.name.eq_ignore_ascii_case(stem))
                && let Ok(mut parsed) = DctlCubeLut::parse_cube(&plut.text)
            {
                parsed.interp = *interp;
                owned_luts.push((ident.clone(), parsed));
            }
        }
    }
    let lut_refs: Vec<(&str, &DctlCubeLut)> = owned_luts.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let frame_idx = (cx.seconds * cx.frame_rate.max(1.0)).round() as i64;
    eval_dctl_program(img, &prog, &bindings, &lut_refs, frame_idx, mix);
}

// ---------------------------------------------------------------------------
// Color Space Transform (CST) — Exact SMPTE RP 177 & Published Camera Curves
// ---------------------------------------------------------------------------

/// Input choice indices (`input_space`):
/// 0: Use Timeline, 1: Rec. 709, 2: ARRI AWG3/LogC3, 3: ARRI AWG4/LogC4,
/// 4: Sony S-Gamut3.Cine/S-Log3, 5: RED RWG/Log3G10, 6: BMD Film Gen 5,
/// 7: ACEScct (AP1), 8: DaVinci Wide Gamut / Intermediate, 9: Rec. 2100 PQ, 10: Rec. 2100 HLG.
///
/// Output choice indices (`output_space`):
/// 0: Use Timeline, 1: Rec. 709, 2: DaVinci Wide Gamut / Intermediate,
/// 3: ACEScct (AP1), 4: Rec. 2100 PQ, 5: Rec. 2100 HLG.
pub(crate) fn resolve_input_code(choice_idx: u32, working: WorkingSpace) -> u32 {
    if choice_idx != 0 {
        return choice_idx;
    }
    match working {
        WorkingSpace::Rec709 => 1,
        WorkingSpace::Rec2100Pq => 9,
        WorkingSpace::Rec2100Hlg => 10,
    }
}

pub(crate) fn resolve_output_code(choice_idx: u32, working: WorkingSpace) -> u32 {
    match choice_idx {
        0 => match working {
            WorkingSpace::Rec709 => 1,
            WorkingSpace::Rec2100Pq => 9,
            WorkingSpace::Rec2100Hlg => 10,
        },
        1 => 1,
        2 => 8,  // DaVinci Wide Gamut / Intermediate
        3 => 7,  // ACEScct
        4 => 9,  // Rec. 2100 PQ
        5 => 10, // Rec. 2100 HLG
        _ => 1,
    }
}

/// CIE 1931 RGB → XYZ matrix (white Y = 1.0, D65-adapted) for canonical space code.
pub(crate) fn space_to_xyz(space: u32) -> Mat3 {
    match space {
        2 => Gamut::Awg3.to_xyz(),
        3 => Gamut::Awg4.to_xyz(),
        4 => Gamut::SGamut3Cine.to_xyz(),
        // REDWideGamutRGB (RED Digital Cinema published chromaticities, D65)
        5 => npm([[0.780_308, 0.304_253], [0.121_595, 1.493_994], [0.095_612, -0.084_589]], D65),
        // Blackmagic Design Wide Gamut Gen 5 (Blackmagic Gen 5 Color Science whitepaper)
        6 => npm([[0.717_721_5, 0.317_118_1], [0.228_041_0, 0.861_569_0], [0.100_584_1, -0.082_045_2]], [0.312_717_0, 0.329_031_2]),
        // ACES AP1 (SMPTE ST 2065-4 / ACEScg/ACEScct primaries, D65-adapted white)
        7 => npm([[0.713, 0.293], [0.165, 0.830], [0.128, 0.044]], D65),
        // DaVinci Wide Gamut (Blackmagic Design DaVinci Wide Gamut whitepaper, D65)
        8 => npm([[0.8000, 0.3130], [0.1682, 0.9877], [0.0790, -0.1155]], D65),
        9 | 10 => Gamut::Bt2020.to_xyz(),
        _ => Gamut::Bt709.to_xyz(),
    }
}

pub(crate) fn cst_gamut_matrix(in_code: u32, out_code: u32) -> [[f32; 3]; 3] {
    to_f32(&mul(&inverse(&space_to_xyz(out_code)), &space_to_xyz(in_code)))
}

/// Decode camera/HDR transfer signal `v` into scene-linear light (`1.0` = SDR diffuse white).
pub(crate) fn to_scene_linear_single(v: f32, space: u32) -> f32 {
    match space {
        2 => LogCurve::LogC3.decode(f64::from(v)) as f32,
        3 => LogCurve::LogC4.decode(f64::from(v)) as f32,
        4 => LogCurve::SLog3.decode(f64::from(v)) as f32,
        // RED Log3G10 (published RED specification, preserving negative linear toe)
        5 => {
            if v < 0.0 {
                (v / 15.1927) - 0.01
            } else {
                (10.0f32.powf((v / 0.224282).clamp(-20.0, 20.0)) - 1.0) / 155.975_33 - 0.01
            }
        }
        // Blackmagic Design Film Gen 5
        6 => {
            if v < 0.09246575 {
                (v - 0.09246575) / 10.540238 + 0.005
            } else {
                (((v - 0.530_013_3) / 0.08692876).clamp(-40.0, 40.0)).exp() - 0.00549407
            }
        }
        // ACEScct (Academy S-2016-001)
        7 => {
            if v <= 0.15525114 {
                (v - 0.072_905_53) / 10.540238
            } else {
                2.0f32.powf((v * 17.52 - 9.72).clamp(-40.0, 40.0))
            }
        }
        // DaVinci Intermediate (Blackmagic Design whitepaper)
        8 => {
            if v <= 0.02740668 {
                v / 10.444_268
            } else {
                2.0f32.powf((v / 0.07329248 - 7.0).clamp(-40.0, 40.0)) - 0.0075
            }
        }
        // SMPTE ST 2084 PQ (1.0 = 100 nits reference white, 100.0 = 10,000 nits)
        9 => {
            let p = v.clamp(0.0, 1.0).powf(1.0 / 78.84375);
            let num = (p - 0.8359375).max(0.0);
            let den = (18.851_563 - 18.6875 * p).max(1e-6);
            (num / den).powf(1.0 / 0.15930176) * 100.0
        }
        // ITU-R BT.2100 HLG Inverse OETF (C1-continuous at v = 0.5: both branches equal 0.25)
        10 => {
            let x = v.max(0.0);
            if x <= 0.5 { x * x } else { ((((x - 0.559_910_7) / 0.17883277).clamp(-40.0, 40.0)).exp() + 0.28466892) * 0.25 }
        }
        _ => v,
    }
}

/// Encode scene-linear light `lin` into canonical output space `space`.
pub(crate) fn from_scene_linear_single(lin: f32, space: u32) -> f32 {
    match space {
        2 => signed_srgb_to_linear(LogCurve::LogC3.encode(f64::from(lin)) as f32),
        3 => signed_srgb_to_linear(LogCurve::LogC4.encode(f64::from(lin)) as f32),
        4 => signed_srgb_to_linear(LogCurve::SLog3.encode(f64::from(lin)) as f32),
        7 => {
            let cct = if lin <= 0.0078125 { 10.540238 * lin + 0.072_905_53 } else { (lin.log2() + 9.72) / 17.52 };
            signed_srgb_to_linear(cct)
        }
        8 => {
            let di = if lin <= 0.00262409 { lin * 10.444_268 } else { ((lin.max(-0.0074) + 0.0075).log2() + 7.0) * 0.07329248 };
            signed_srgb_to_linear(di)
        }
        9 => {
            let y = (lin.max(0.0) / 100.0).clamp(0.0, 1.0).powf(0.15930176);
            let pq = ((0.8359375 + 18.851_563 * y) / (1.0 + 18.6875 * y)).powf(78.84375);
            signed_srgb_to_linear(pq.clamp(0.0, 1.0))
        }
        10 => {
            let e = (lin.max(0.0) / 3.0).max(0.0);
            let hlg = if e <= 1.0 / 12.0 { (3.0 * e).sqrt() } else { 0.17883277 * (12.0 * e - 0.28466892).ln() + 0.559_910_7 };
            signed_srgb_to_linear(hlg.clamp(0.0, 1.5))
        }
        _ => lin,
    }
}

pub(crate) fn tone_and_gamut_map(mut rgb: [f32; 3], tone_mode: u32, max_nits: f32, gamut_comp: bool) -> [f32; 3] {
    let peak = (max_nits / 100.0).max(1.0);
    match tone_mode {
        // DaVinci Luminance Mapping (knee roll-off preserving RGB ratios)
        1 => {
            let y = luma709(rgb[0].max(0.0), rgb[1].max(0.0), rgb[2].max(0.0));
            if y > 0.65 {
                let t = (y - 0.65) / (peak - 0.65).max(0.35);
                let mapped_y = 0.65 + 0.35 * (t / (1.0 + t));
                let scale = mapped_y / y.max(1e-6);
                rgb = [rgb[0] * scale, rgb[1] * scale, rgb[2] * scale];
            }
        }
        // Simple Reinhard
        2 => {
            rgb = rgb.map(|c| {
                let pos = c.max(0.0);
                pos / (1.0 + pos / peak) * (1.0 + 1.0 / peak)
            });
        }
        _ => {}
    }
    if gamut_comp {
        // ACES/DaVinci-style achromatic axis gamut compression for out-of-gamut excursions
        let ach = rgb[0].max(rgb[1]).max(rgb[2]).max(1e-6);
        rgb = rgb.map(|c| {
            let dist = (ach - c) / ach;
            let compressed = if dist > 0.82 {
                let x = ((dist - 0.82) / 0.5).clamp(-10.0, 10.0);
                let e2 = (2.0 * x).exp();
                0.82 + 0.18 * ((e2 - 1.0) / (e2 + 1.0))
            } else {
                dist
            };
            (ach * (1.0 - compressed)).max(0.0)
        });
    }
    rgb
}

pub(crate) fn eval_cst_pixel(unpre: [f32; 3], mat: &[[f32; 3]; 3], in_code: u32, out_code: u32, tone_mode: u32, max_nits: f32, gamut_comp: bool) -> [f32; 3] {
    let lin_src = if in_code >= 2 {
        let code = [signed_linear_to_srgb(unpre[0]), signed_linear_to_srgb(unpre[1]), signed_linear_to_srgb(unpre[2])];
        [to_scene_linear_single(code[0], in_code), to_scene_linear_single(code[1], in_code), to_scene_linear_single(code[2], in_code)]
    } else {
        unpre
    };
    let lin_dst_gamut = apply3(mat, lin_src);
    let mapped = tone_and_gamut_map(lin_dst_gamut, tone_mode, max_nits, gamut_comp);
    [from_scene_linear_single(mapped[0], out_code), from_scene_linear_single(mapped[1], out_code), from_scene_linear_single(mapped[2], out_code)]
}

fn color_space_transform(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    if let Some(op) = crate::gpufx::FxOp::eval(e, cx, img.w, img.h) {
        op.apply(img);
    }
}

// ---------------------------------------------------------------------------
// Film Halation (ResolveFX Stylize)
// ---------------------------------------------------------------------------

fn blur_separable_rgb(buf: &mut [[f32; 3]], w: usize, h: usize, radius: usize) {
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let r = radius.min(w.max(h));
    let inv = 1.0 / (2 * r + 1) as f32;
    let mut tmp = vec![[0.0f32; 3]; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, out_row)| {
        let src_row = &buf[y * w..(y + 1) * w];
        for (x, slot) in out_row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 3];
            for k in 0..=(2 * r) {
                let sx = (x + k).saturating_sub(r).min(w - 1);
                acc[0] += src_row[sx][0];
                acc[1] += src_row[sx][1];
                acc[2] += src_row[sx][2];
            }
            *slot = [acc[0] * inv, acc[1] * inv, acc[2] * inv];
        }
    });
    buf.par_chunks_mut(w).enumerate().for_each(|(y, out_row)| {
        for (x, slot) in out_row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 3];
            for k in 0..=(2 * r) {
                let sy = (y + k).saturating_sub(r).min(h - 1);
                let px = tmp[sy * w + x];
                acc[0] += px[0];
                acc[1] += px[1];
                acc[2] += px[2];
            }
            *slot = [acc[0] * inv, acc[1] * inv, acc[2] * inv];
        }
    });
}

fn film_halation(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let thresh = (f(e, "threshold", cx) / 100.0).clamp(0.0, 1.0);
    let strength = (f(e, "strength", cx) / 100.0).clamp(0.0, 2.0);
    let spread_px = (f(e, "spread", cx) * cx.px_scale.max(0.25)).clamp(1.0, 120.0) as usize;
    let hue_deg = f(e, "hue_shift", cx);
    let sec_green = (f(e, "secondary_glow", cx) / 100.0).clamp(0.0, 1.0);
    let isolate = b(e, "view_halation");
    if strength <= 0.0 && !isolate {
        return;
    }
    let (w, h) = (img.w, img.h);
    let lin_thresh = srgb_to_linear(thresh);
    let mut hi: Vec<[f32; 3]> = img
        .px
        .par_chunks(4)
        .map(|p| {
            let a = p[3];
            if a <= 0.0 {
                return [0.0; 3];
            }
            let c = [p[0] / a, p[1] / a, p[2] / a];
            let y = luma709(c[0], c[1], c[2]);
            if y > lin_thresh {
                let excess = (y - lin_thresh) / (1.0 - lin_thresh + 0.15);
                [c[0] * excess, c[1] * excess, c[2] * excess]
            } else {
                [0.0; 3]
            }
        })
        .collect();

    let r1 = (spread_px / 2).max(1);
    blur_separable_rgb(&mut hi, w, h, r1);
    blur_separable_rgb(&mut hi, w, h, spread_px);

    let hue_norm = hue_deg.rem_euclid(360.0) / 360.0;
    let tint = hsl_to_rgb(hue_norm, 0.92, 0.55);
    img.px.par_chunks_mut(4).zip(hi.par_iter()).for_each(|(p, glow)| {
        let a = p[3];
        if a <= 0.0 {
            return;
        }
        let energy = glow[0] * 0.55 + glow[1] * 0.35 + glow[2] * 0.10;
        let hal = [energy * tint[0] * strength * 1.35, energy * (tint[1] * 0.45 + sec_green * 0.45) * strength, energy * tint[2] * 0.18 * strength];
        if isolate {
            p[0] = hal[0] * a;
            p[1] = hal[1] * a;
            p[2] = hal[2] * a;
        } else {
            p[0] += hal[0] * a;
            p[1] += hal[1] * a;
            p[2] += hal[2] * a;
        }
    });
}

// ---------------------------------------------------------------------------
// Color Warper (Hue-Chroma Spider-Web Warper)
// ---------------------------------------------------------------------------

pub(crate) fn shortest_hue_deg(h1: f32, h2: f32) -> f32 {
    (h1 - h2 + 540.0).rem_euclid(360.0) - 180.0
}

fn color_warper(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    if let Some(op) = crate::gpufx::FxOp::eval(e, cx, img.w, img.h) {
        op.apply(img);
    }
}

// ---------------------------------------------------------------------------
// 3D HSL Qualifier with Simultaneous Inside / Outside Grading
// ---------------------------------------------------------------------------

pub(crate) fn smooth_band(x: f32, lo: f32, hi: f32, soft: f32) -> f32 {
    let s = soft.max(1e-4);
    let enter = ((x - (lo - s)) / s).clamp(0.0, 1.0);
    let exit = (((hi + s) - x) / s).clamp(0.0, 1.0);
    enter.min(exit)
}

pub(crate) fn grade_zone(sr: [f32; 3], ev: f32, sat_mul: f32, warmth: f32) -> [f32; 3] {
    let gain = 2.0f32.powf(ev.clamp(-20.0, 20.0));
    let mut lin = [
        srgb_to_linear(sr[0].clamp(0.0, 1.0)) * gain * (1.0 + warmth * 0.22),
        srgb_to_linear(sr[1].clamp(0.0, 1.0)) * gain,
        srgb_to_linear(sr[2].clamp(0.0, 1.0)) * gain * (1.0 - warmth * 0.22),
    ];
    let y = luma709(lin[0].max(0.0), lin[1].max(0.0), lin[2].max(0.0));
    lin[0] = (y + (lin[0] - y) * sat_mul).max(0.0);
    lin[1] = (y + (lin[1] - y) * sat_mul).max(0.0);
    lin[2] = (y + (lin[2] - y) * sat_mul).max(0.0);
    lin
}

fn qualifier_3d(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    if let Some(op) = crate::gpufx::FxOp::eval(e, cx, img.w, img.h) {
        op.apply(img);
    }
}

// ---------------------------------------------------------------------------
// Dynamic Zoom (Resolve Cut / Edit Page)
// ---------------------------------------------------------------------------

pub(crate) fn ease_curve(u: f32, kind: u32) -> f32 {
    let t = u.clamp(0.0, 1.0);
    match kind {
        1 => t * t,                       // Ease In
        2 => 1.0 - (1.0 - t) * (1.0 - t), // Ease Out
        3 => t * t * (3.0 - 2.0 * t),     // Ease In and Out
        _ => t,                           // Linear
    }
}

fn dynamic_zoom(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    if let Some(op) = crate::gpufx::FxOp::eval(e, cx, img.w, img.h) {
        op.apply(img);
    }
}

#[cfg(test)]
mod tests {
    use filmcraft_color::WorkingSpace;
    use filmcraft_project::{ParamValue, find_effect};
    use filmcraft_time::Tick;

    use super::{from_scene_linear_single, space_to_xyz, to_scene_linear_single};
    use crate::effects::{FxCtx, apply};
    use crate::image::Image;

    fn sample_img() -> Image {
        let mut img = Image::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                let v = (x + y) as f32 / 30.0;
                let idx = (y * img.w + x) * 4;
                img.px[idx..idx + 4].copy_from_slice(&[v, v * 0.6, 1.0 - v * 0.5, 1.0]);
            }
        }
        img
    }

    fn test_ctx() -> FxCtx<'static> {
        FxCtx {
            t: Tick::ZERO,
            px_scale: 1.0,
            mask_scale: 1.0,
            seconds: 1.25,
            timecode: "00:00:01:06",
            clip_name: "TestClip",
            project: None,
            env: None,
            frame_rate: 24.0,
            working: WorkingSpace::Rec709,
        }
    }

    #[test]
    fn resolve_fx_processors_transform_pixels_without_nan() {
        let cx = test_ctx();
        for id in ["dctl_transform", "color_space_transform", "film_halation", "color_warper", "qualifier_3d", "dynamic_zoom"] {
            let def = find_effect(id).unwrap_or_else(|| panic!("missing {id}"));
            let mut inst = def.instance();
            if id == "color_warper" {
                inst.params.get_mut("hue_rotate").unwrap().value = ParamValue::Float(35.0);
            }
            if id == "color_space_transform" {
                inst.params.get_mut("input_space").unwrap().value = ParamValue::Choice(4);
            }
            let mut img = sample_img();
            let before = img.px.clone();
            apply(&mut img, &inst, &cx);
            assert!(img.px.iter().all(|v| v.is_finite()), "{id} produced non-finite pixel");
            assert_ne!(img.px, before, "{id} should alter pixels");
        }
    }

    #[test]
    fn cst_gamut_matrices_preserve_neutral_white_and_hlg_is_continuous_and_sandwich_inverts() {
        // 1. Every CIE 1931 gamut matrix preserves D65 neutral white (row sums of Y = 1.0)
        for space in 1..=10 {
            let m = space_to_xyz(space);
            let white_y = m[1][0] + m[1][1] + m[1][2];
            assert!((white_y - 1.0).abs() < 1e-4, "space {space} white Y = {white_y}");
        }
        // AWG3 != AWG4 and BMD Gen 5 != DWG != ACES AP1
        assert_ne!(space_to_xyz(2), space_to_xyz(3));
        assert_ne!(space_to_xyz(6), space_to_xyz(8));
        assert_ne!(space_to_xyz(7), space_to_xyz(8));

        // 2. HLG is C0/C1 continuous across x = 0.5
        let lo = to_scene_linear_single(0.4999, 10);
        let hi = to_scene_linear_single(0.5001, 10);
        assert!((hi - lo).abs() < 5e-4, "HLG discontinuity around 0.5: {lo} vs {hi}");

        // 3. CST sandwich round-trip: Rec.709 -> DaVinci Wide Gamut / Intermediate -> Rec.709
        let cx = test_ctx();
        let def = find_effect("color_space_transform").unwrap();
        let mut to_dwg = def.instance();
        to_dwg.params.get_mut("input_space").unwrap().value = ParamValue::Choice(1); // Rec.709
        to_dwg.params.get_mut("output_space").unwrap().value = ParamValue::Choice(2); // DWG / Intermediate
        to_dwg.params.get_mut("tone_mapping").unwrap().value = ParamValue::Choice(0); // None
        to_dwg.params.get_mut("gamut_compression").unwrap().value = ParamValue::Bool(false);

        let mut from_dwg = def.instance();
        from_dwg.params.get_mut("input_space").unwrap().value = ParamValue::Choice(8); // DWG / Intermediate
        from_dwg.params.get_mut("output_space").unwrap().value = ParamValue::Choice(1); // Rec.709
        from_dwg.params.get_mut("tone_mapping").unwrap().value = ParamValue::Choice(0); // None
        from_dwg.params.get_mut("gamut_compression").unwrap().value = ParamValue::Bool(false);

        let mut img = Image::new(1, 1);
        img.px.copy_from_slice(&[0.35, 0.18, 0.12, 1.0]);
        apply(&mut img, &to_dwg, &cx);
        assert!((img.px[0] - 0.35).abs() > 0.02, "encoded into DWG/DI");
        apply(&mut img, &from_dwg, &cx);
        assert!((img.px[0] - 0.35).abs() < 1e-3 && (img.px[1] - 0.18).abs() < 1e-3 && (img.px[2] - 0.12).abs() < 1e-3, "round-trip: {:?}", img.px);

        let _ = from_scene_linear_single(0.18, 8);
    }
}
