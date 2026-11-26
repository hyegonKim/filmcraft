//! Lights & Glows (and the glow-like Legacy Alpha Glow, the procedural Lens Flare).

use filmcraft_project::EffectInstance;
use rayon::prelude::*;

use super::{
    Affine, FxCtx, Image, add_light, alpha_of, blend_simple, blur_plane, bv, chv, cv, fbm, fv, generate, hash01, highlights, lerp3, lin, luma, over, pv,
    smoothstep,
};
use crate::effects::gaussian;

fn blurred(src: &Image, sigma: f32) -> Image {
    let mut g = src.clone();
    gaussian(&mut g, sigma, sigma, false);
    g
}

/// Add `light` (premultiplied linear) to `img`: Add or Screen.
fn add_image(img: &mut Image, light: &Image, gain: [f32; 3], screen: bool) {
    img.px.par_chunks_mut(4).zip(light.px.par_chunks(4)).for_each(|(p, l)| {
        let add = [l[0] * gain[0], l[1] * gain[1], l[2] * gain[2]];
        if screen {
            let a = [add[0].min(1.0), add[1].min(1.0), add[2].min(1.0)];
            for k in 0..3 {
                p[k] = p[k] + a[k] - p[k].min(1.0) * a[k];
            }
            p[3] = p[3] + (1.0 - p[3]) * a[0].max(a[1]).max(a[2]);
        } else {
            add_light(p, add);
        }
    });
}

fn saturate_image(img: &mut Image, sat: f32) {
    if (sat - 1.0).abs() < 1e-4 {
        return;
    }
    img.px.par_chunks_mut(4).for_each(|p| {
        let l = luma([p[0], p[1], p[2]]);
        for v in &mut p[..3] {
            *v = (l + (*v - l) * sat).max(0.0);
        }
    });
}

/// Replace the hue of a light buffer with `c` (keeping its luminance), by `k`.
fn tint(img: &mut Image, c: [f32; 3], k: f32) {
    if k <= 0.0 {
        return;
    }
    img.px.par_chunks_mut(4).for_each(|p| {
        let l = luma([p[0], p[1], p[2]]);
        for i in 0..3 {
            p[i] += (l * c[i] * 1.5 - p[i]) * k;
        }
    });
}

/// Echo Glow: glow repeated in widening echoes that fade by Decay.
pub fn echo_glow(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let hi = highlights(img, fv(e, "threshold", cx) / 100.0, 0.1);
    let n = fv(e, "echoes", cx).clamp(1.0, 12.0) as usize;
    let r = fv(e, "radius", cx) * cx.px_scale * 0.5;
    let spread = fv(e, "spread", cx).max(1.0);
    let inten = fv(e, "intensity", cx) / 100.0;
    let decay = 1.0 - fv(e, "decay", cx) / 100.0;
    let col = lin(cv(e, "color", cx));
    let layers: Vec<(Image, f32)> = (0..n).into_par_iter().map(|i| (blurred(&hi, r * spread.powi(i as i32)), decay.powi(i as i32))).collect();
    let mut acc = Image::new(img.w, img.h);
    for (l, wgt) in &layers {
        acc.px.par_iter_mut().zip(l.px.par_iter()).for_each(|(a, b)| *a += b * wgt);
    }
    let norm = inten / layers.iter().map(|l| l.1).sum::<f32>().max(1e-6) * 2.0;
    tint(&mut acc, col, 1.0);
    add_image(img, &acc, [norm; 3], false);
}

/// Sobel edge strength of display luma.
fn edges(img: &Image) -> Vec<f32> {
    let w = img.w;
    let mut out = vec![0.0f32; img.w * img.h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let l = |dx: isize, dy: isize| {
                let p = img.get_clamped(x as isize + dx, y as isize + dy);
                filmcraft_color::linear_to_srgb(luma([p[0], p[1], p[2]]).clamp(0.0, 1.0)) * p[3].min(1.0) + 0.0
            };
            let gx = l(1, -1) + 2.0 * l(1, 0) + l(1, 1) - l(-1, -1) - 2.0 * l(-1, 0) - l(-1, 1);
            let gy = l(-1, 1) + 2.0 * l(0, 1) + l(1, 1) - l(-1, -1) - 2.0 * l(0, -1) - l(1, -1);
            *o = (gx * gx + gy * gy).sqrt() / 4.0;
        }
    });
    out
}

/// Edge Glow: coloured glow along the picture's edges.
pub fn edge_glow(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let th = fv(e, "threshold", cx) / 100.0;
    let width = fv(e, "width", cx) * cx.px_scale;
    let r = fv(e, "radius", cx) * cx.px_scale * 0.5;
    let inten = fv(e, "intensity", cx) / 100.0;
    let col = lin(cv(e, "color", cx));
    let mut m: Vec<f32> = edges(img).into_iter().map(|v| smoothstep(th, th + 0.1, v)).collect();
    if width > 1.0 {
        m = blur_plane(&m, img.w, img.h, width * 0.5).into_iter().map(|v| (v * 3.0).min(1.0)).collect();
    }
    let core = m.clone();
    let halo = blur_plane(&m, img.w, img.h, r);
    let mut light = Image::new(img.w, img.h);
    light.px.par_chunks_mut(4).enumerate().for_each(|(i, p)| {
        let v = (core[i] * 0.6 + halo[i] * 1.6) * inten;
        p.copy_from_slice(&[col[0] * v, col[1] * v, col[2] * v, v.min(1.0)]);
    });
    if bv(e, "only") {
        *img = Image::new(img.w, img.h);
    }
    add_image(img, &light, [1.0; 3], false);
}

/// Sum of `a^k · src(p − k·d)` for k = 0…len (an exponentially decaying streak), by doubling.
/// `len` sets the decay and normalisation; `reach` (at most `len`) bounds how far the passes go.
fn streak(src: &Image, dir: (f32, f32), len: f32, reach: f32) -> Image {
    let a = (-1.0 / len.max(1.0)).exp();
    let mut s = src.clone();
    let mut n = 1.0f32;
    while n < reach * 3.0 {
        let prev = s.clone();
        let k = a.powf(n);
        let (dx, dy) = (dir.0 * n, dir.1 * n);
        let w = s.w;
        s.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let q = prev.sample_bilinear(x as f32 + 0.5 - dx, y as f32 + 0.5 - dy);
                for c in 0..4 {
                    row[x * 4 + c] += q[c] * k;
                }
            }
        });
        n *= 2.0;
    }
    let norm = 1.0 - a;
    s.px.par_iter_mut().for_each(|v| *v *= norm);
    s
}

/// Glint: star streaks from the brightest points.
pub fn glint(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let hi = highlights(img, fv(e, "threshold", cx) / 100.0, 0.08);
    let rays = [2usize, 4, 6, 8][chv(e, "rays").min(3) as usize];
    let len = fv(e, "length", cx) * cx.px_scale;
    // past the picture's extent a longer streak adds nothing but more full-image passes (its weighting still uses `len`)
    let reach = len.clamp(0.0, (hi.w + hi.h) as f32);
    let rot = fv(e, "rotation", cx).to_radians();
    let inten = fv(e, "intensity", cx) / 100.0 * 3.0;
    let col = lin(cv(e, "color", cx));
    let colorize = fv(e, "colorize", cx) / 100.0;
    let parts: Vec<Image> = (0..rays)
        .into_par_iter()
        .map(|i| {
            let a = rot + std::f32::consts::TAU * i as f32 / rays as f32;
            streak(&hi, (a.cos(), a.sin()), len * 0.35, reach * 0.35)
        })
        .collect();
    let mut acc = Image::new(img.w, img.h);
    for p in &parts {
        acc.px.par_iter_mut().zip(p.px.par_iter()).for_each(|(a, b)| *a += b);
    }
    tint(&mut acc, col, colorize);
    add_image(img, &acc, [inten * col[0].max(0.2), inten * col[1].max(0.2), inten * col[2].max(0.2)], false);
}

/// Light Leaks: drifting warm coloured light washes (procedural, seeded, animated by Speed).
pub fn light_leaks(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let c1 = lin(cv(e, "c1", cx));
    let c2 = lin(cv(e, "c2", cx));
    let inten = fv(e, "intensity", cx) / 100.0;
    let scale = (fv(e, "scale", cx) / 100.0).max(0.05) * img.w.max(img.h) as f32 * 0.6;
    let dir = fv(e, "direction", cx).to_radians();
    let t = (cx.seconds * fv(e, "speed", cx) as f64) as f32;
    let seed = fv(e, "seed", cx).max(0.0) as u64;
    let mode = chv(e, "mode");
    let (dx, dy) = (dir.cos(), dir.sin());
    let (w, h) = (img.w as f32, img.h as f32);
    generate(img, |x, y, p| {
        let (u, v) = (x / scale, y / scale);
        // stronger towards the edge the leak enters from
        let along = ((x / w - 0.5) * dx + (y / h - 0.5) * dy) * 2.0;
        let bias = smoothstep(-0.2, 1.0, -along);
        let n = fbm(u + dx * t * 0.3, v + dy * t * 0.3, t * 0.15, 3.0, seed) * 0.5 + 0.5;
        let m = fbm(u * 0.7 + 5.2, v * 0.7, t * 0.1 + 9.0, 2.0, seed) * 0.5 + 0.5;
        let k = smoothstep(0.35, 0.85, n) * bias * inten;
        let light = lerp3(c1, c2, m).map(|c| c * k);
        let base = Image::unpremul(p);
        let o = match mode {
            1 => [base[0] + light[0], base[1] + light[1], base[2] + light[2]],
            2 => blend_simple(4, base, lerp3(base, light.map(|q| q + 0.5), k.min(1.0))),
            _ => blend_simple(2, base, light),
        };
        let a = p[3] + (1.0 - p[3]) * k.min(1.0);
        [o[0] * a, o[1] * a, o[2] * a, a]
    });
}

/// RGB Split: red and blue sampled from opposite offsets (linear or radial from a centre).
pub fn rgb_split(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let amt = fv(e, "amount", cx) * cx.px_scale;
    if amt.abs() < 1e-3 {
        return;
    }
    let radial = chv(e, "mode") == 1;
    let ang = fv(e, "angle", cx).to_radians();
    let c = pv(e, "center", cx, img);
    let blend = fv(e, "blend", cx) / 100.0;
    let maxd = ((img.w as f64).hypot(img.h as f64) / 2.0) as f32;
    let src = img.clone();
    let w = img.w;
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (ox, oy) = if radial {
                let (dx, dy) = (px - c.x as f32, py - c.y as f32);
                (dx / maxd * amt, dy / maxd * amt)
            } else {
                (ang.cos() * amt, ang.sin() * amt)
            };
            let r = src.sample_bilinear(px - ox, py - oy);
            let g = src.get(x, y);
            let b = src.sample_bilinear(px + ox, py + oy);
            let o = [r[0], g[1], b[2], r[3].max(g[3]).max(b[3])];
            for k in 0..4 {
                row[x * 4 + k] = o[k] + (g[k] - o[k]) * blend;
            }
        }
    });
}

/// Volumetric Rays: light shafts radiating from a source point (radial blur of the highlights).
pub fn volumetric_rays(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let c = pv(e, "center", cx, img);
    let len = fv(e, "length", cx) / 100.0;
    let inten = fv(e, "intensity", cx) / 100.0;
    let col = lin(cv(e, "color", cx));
    let only = bv(e, "only");
    let mut hi = highlights(img, fv(e, "threshold", cx) / 100.0, 0.1);
    let (fw, fh) = (img.w, img.h);
    let mut down = 1.0;
    while hi.w > 960 {
        hi = hi.downsample2();
        down *= 2.0;
    }
    let (ccx, ccy) = ((c.x / down) as f32, (c.y / down) as f32);
    const N: usize = 48;
    let src = hi.clone();
    let w = hi.w;
    hi.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut acc = [0.0f32; 4];
            let mut wsum = 0.0;
            for i in 0..N {
                let t = i as f32 / N as f32 * len;
                let s = src.sample_bilinear(px + (ccx - px) * t, py + (ccy - py) * t);
                let wgt = 1.0 - i as f32 / N as f32 * 0.6;
                for k in 0..4 {
                    acc[k] += s[k] * wgt;
                }
                wsum += wgt;
            }
            for k in 0..4 {
                row[x * 4 + k] = acc[k] / wsum;
            }
        }
    });
    let rays = if down > 1.0 { hi.transformed(fw, fh, &Affine::scale(fw as f64 / hi.w as f64, fh as f64 / hi.h as f64)) } else { hi };
    if only {
        *img = Image::new(fw, fh);
    }
    add_image(img, &rays, col.map(|v| v * inten * 2.5), false);
}

/// Wonder Glow: a rich multi-scale glow (three radii) with saturation and colour controls.
pub fn wonder_glow(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let mut hi = highlights(img, fv(e, "threshold", cx) / 100.0, 0.15);
    saturate_image(&mut hi, fv(e, "saturation", cx) / 100.0);
    if bv(e, "use_color") {
        tint(&mut hi, lin(cv(e, "color", cx)), 1.0);
    }
    let r = fv(e, "radius", cx) * cx.px_scale * 0.5;
    let inten = fv(e, "intensity", cx) / 100.0;
    let parts: Vec<(Image, f32)> = [(0.25f32, 0.45f32), (1.0, 0.35), (3.0, 0.2)].par_iter().map(|&(k, wgt)| (blurred(&hi, r * k), wgt)).collect();
    let mut acc = Image::new(img.w, img.h);
    for (p, wgt) in &parts {
        acc.px.par_iter_mut().zip(p.px.par_iter()).for_each(|(a, b)| *a += b * wgt);
    }
    add_image(img, &acc, [inten * 1.5; 3], chv(e, "mode") == 1);
}

/// Lens Flare (26.5 rebuild): core glow, halo, ghosts along the line through the frame centre and
/// a lens-type-dependent streak; tinted by Flare Color, scaled by Size.
pub fn lens_flare(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let c = pv(e, "center", cx, img);
    let br = fv(e, "brightness", cx) / 100.0;
    let blend = fv(e, "blend", cx) / 100.0;
    let lens = chv(e, "lens_type");
    let tintc = lin(cv(e, "color", cx));
    let size = (fv(e, "size", cx) / 100.0).max(0.05) as f64;
    let ghosts = fv(e, "ghosts", cx).clamp(0.0, 8.0) as usize;
    let (w, h) = (img.w as f64, img.h as f64);
    let (mx, my) = (w / 2.0, h / 2.0);
    let scale = w.max(h) * size;
    let core_k = [18.0, 26.0, 14.0, 22.0][lens.min(3) as usize];
    let ghost_list: Vec<(f64, f64, [f32; 3])> = (0..ghosts)
        .map(|i| {
            let t = 0.35 + i as f64 * 0.42 + hash01(i as i64, lens as i64, 0, 7) as f64 * 0.2;
            let r = 0.02 + hash01(i as i64, lens as i64, 1, 7) as f64 * 0.07;
            let hue = [[0.4, 0.6, 1.0], [1.0, 0.5, 0.3], [0.4, 1.0, 0.5], [0.9, 0.4, 1.0]][i % 4];
            (t, r, hue)
        })
        .collect();
    let wi = img.w;
    img.px.par_chunks_mut(wi * 4).enumerate().for_each(|(y, row)| {
        for x in 0..wi {
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            let d = (px - c.x).hypot(py - c.y) / scale;
            let core = 0.35 * (-d * core_k).exp() + 0.08 * (-d * 3.0).exp();
            let mut add = [core as f32; 3];
            if lens == 1 || lens == 2 {
                let ring = (1.0 - ((d - 0.12) / 0.012).abs()).max(0.0) * 0.08;
                for (k, v) in add.iter_mut().enumerate() {
                    *v += (ring * [0.6, 0.8, 1.0][k]) as f32;
                }
            }
            if lens == 3 {
                // anamorphic: long horizontal streak
                let s = (-((py - c.y).abs() / scale) * 160.0).exp() * (-((px - c.x).abs() / scale) * 2.0).exp() * 0.5;
                add[0] += s as f32 * 0.5;
                add[1] += s as f32 * 0.7;
                add[2] += s as f32;
            }
            for (t, rad, hue) in &ghost_list {
                let gx = c.x + (mx - c.x) * 2.0 * t;
                let gy = c.y + (my - c.y) * 2.0 * t;
                let dd = (px - gx).hypot(py - gy) / scale;
                let g = ((1.0 - ((dd - rad) / 0.01).abs()).max(0.0) * 0.12 + if dd < *rad { 0.05 } else { 0.0 }) as f32;
                for k in 0..3 {
                    add[k] += hue[k] * g;
                }
            }
            let p = &mut row[x * 4..x * 4 + 4];
            let k = br * (1.0 - blend);
            add_light(p, [add[0] * tintc[0] * k, add[1] * tintc[1] * k, add[2] * tintc[2] * k]);
        }
    });
}

/// Alpha Glow (Legacy): glow outward from the alpha edges, Start Color fading to End Color.
pub fn alpha_glow(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let r = fv(e, "glow", cx) * cx.px_scale * 0.5;
    if r <= 0.3 {
        return;
    }
    let bright = fv(e, "brightness", cx) / 255.0;
    let sc = lin(cv(e, "start_color", cx));
    let ec = lin(cv(e, "end_color", cx));
    let use_end = bv(e, "use_end");
    let fade = bv(e, "fade_out");
    let a = alpha_of(img);
    let g = blur_plane(&a, img.w, img.h, r);
    img.px.par_chunks_mut(4).enumerate().for_each(|(i, p)| {
        let gv = (g[i] * 2.0).min(1.0);
        let k = if fade {
            gv * bright
        } else if gv > 0.02 {
            bright
        } else {
            0.0
        };
        if k <= 0.0 || p[3] >= 1.0 {
            return;
        }
        let col = if use_end { lerp3(ec, sc, gv) } else { sc };
        // glow behind the layer
        let under = [col[0] * k, col[1] * k, col[2] * k, k];
        let front = [p[0], p[1], p[2], p[3]];
        let mut o = under;
        over(&mut o, front);
        p.copy_from_slice(&o);
    });
}

// ------------------------------------------------------------------ FilmImpact FX (Premiere Pro 27)

fn unpremul(p: [f32; 4]) -> ([f32; 3], f32) {
    let a = p[3].clamp(0.0, 1.0);
    if a <= 1e-6 { ([0.0; 3], 0.0) } else { ([p[0] / a, p[1] / a, p[2] / a], a) }
}

fn srgb3(lin_rgb: [f32; 3]) -> [f32; 3] {
    [
        filmcraft_color::linear_to_srgb(lin_rgb[0].max(0.0)),
        filmcraft_color::linear_to_srgb(lin_rgb[1].max(0.0)),
        filmcraft_color::linear_to_srgb(lin_rgb[2].max(0.0)),
    ]
}

fn lin3(srgb_rgb: [f32; 3]) -> [f32; 3] {
    [
        filmcraft_color::srgb_to_linear(srgb_rgb[0].max(0.0)),
        filmcraft_color::srgb_to_linear(srgb_rgb[1].max(0.0)),
        filmcraft_color::srgb_to_linear(srgb_rgb[2].max(0.0)),
    ]
}

fn rgb_to_hsl(rgb: [f32; 3]) -> (f32, f32, f32) {
    let r = rgb[0].clamp(0.0, 1.0);
    let g = rgb[1].clamp(0.0, 1.0);
    let b = rgb[2].clamp(0.0, 1.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    let d = max - min;
    if d <= 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-6);
    let h = if (max - r).abs() < 1e-6 {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < 1e-6 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s.clamp(0.0, 1.0), l)
}

fn hsl_to_rgb(h_deg: f32, s: f32, l: f32) -> [f32; 3] {
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h_deg.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c * 0.5;
    [(r1 + m).clamp(0.0, 1.0), (g1 + m).clamp(0.0, 1.0), (b1 + m).clamp(0.0, 1.0)]
}

/// Impact Bevel Alpha: directional highlight & shadow relief sculpted along alpha boundaries.
pub fn impact_bevel_alpha(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let thick = (fv(e, "thickness", cx) * cx.px_scale).max(0.0);
    let inten = (fv(e, "intensity", cx) / 100.0).max(0.0);
    if thick <= 0.05 || inten <= 1e-4 || img.w == 0 || img.h == 0 {
        return;
    }
    let soft = (fv(e, "softness", cx) / 100.0).clamp(0.0, 1.0);
    let angle = fv(e, "angle", cx).to_radians();
    let (lx, ly) = (angle.cos(), angle.sin());
    let hi_col = lin(cv(e, "highlight_color", cx));
    let sh_col = lin(cv(e, "shadow_color", cx));
    let sigma = (thick * (0.35 + 0.65 * soft)).max(0.5);
    let height = blur_plane(&alpha_of(img), img.w, img.h, sigma);
    let step = (thick * 0.35).clamp(1.0, 16.0) as isize;
    let (w, h) = (img.w, img.h);
    let sample_h = |x: isize, y: isize| -> f32 {
        let xx = x.clamp(0, w as isize - 1) as usize;
        let yy = y.clamp(0, h as isize - 1) as usize;
        height[yy * w + xx]
    };
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let p = &mut row[x * 4..x * 4 + 4];
            let a = p[3].clamp(0.0, 1.0);
            if a <= 1e-5 {
                continue;
            }
            let gx = (sample_h(x as isize + step, y as isize) - sample_h(x as isize - step, y as isize)) * 0.5;
            let gy = (sample_h(x as isize, y as isize + step) - sample_h(x as isize, y as isize - step)) * 0.5;
            let slope = -(gx * lx + gy * ly) * (2.2 * inten);
            if slope >= 0.0 {
                let k = slope.min(2.0) * a;
                for c in 0..3 {
                    p[c] = (p[c] + hi_col[c] * k).max(0.0);
                }
            } else {
                let k = (-slope).min(1.0);
                for c in 0..3 {
                    p[c] = (p[c] * (1.0 - k) + sh_col[c] * a * k).max(0.0);
                }
            }
        }
    });
}

/// Impact Emboss: directional bilinear relief emboss with controllable color preservation.
pub fn impact_emboss(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let relief = (fv(e, "relief", cx) * cx.px_scale).clamp(0.0, 200.0);
    let contrast = (fv(e, "contrast", cx) / 100.0).clamp(0.0, 10.0);
    let blend = (fv(e, "blend", cx) / 100.0).clamp(0.0, 1.0);
    if relief <= 0.02 || blend >= 0.9999 || img.w == 0 || img.h == 0 {
        return;
    }
    let colorize = (fv(e, "color_preserve", cx) / 100.0).clamp(0.0, 1.0);
    let ang = fv(e, "angle", cx).to_radians();
    let (dx, dy) = (ang.cos() * relief * 0.5, ang.sin() * relief * 0.5);
    let src = img.clone();
    let w = img.w;
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let orig = [row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]];
            let (base_lin, a) = unpremul(orig);
            if a <= 1e-6 {
                continue;
            }
            let (f_lin, _) = unpremul(src.sample_bilinear_clamped(x as f32 + 0.5 + dx, y as f32 + 0.5 + dy));
            let (b_lin, _) = unpremul(src.sample_bilinear_clamped(x as f32 + 0.5 - dx, y as f32 + 0.5 - dy));
            let (fs, bs, os) = (srgb3(f_lin), srgb3(b_lin), srgb3(base_lin));
            let diff = [(fs[0] - bs[0]) * contrast, (fs[1] - bs[1]) * contrast, (fs[2] - bs[2]) * contrast];
            let gray_d = 0.2126 * diff[0] + 0.7152 * diff[1] + 0.0722 * diff[2];
            let mut out_s = [0.0f32; 3];
            for k in 0..3 {
                let neutral = (0.5 + gray_d).clamp(0.0, 1.0);
                let tinted = (os[k] + diff[k]).clamp(0.0, 1.0);
                out_s[k] = neutral + (tinted - neutral) * colorize;
            }
            let out_l = lin3(out_s);
            let dst = &mut row[x * 4..x * 4 + 4];
            for k in 0..3 {
                let emb = out_l[k] * a;
                dst[k] = emb + (orig[k] - emb) * blend;
            }
        }
    });
}

/// Impact Fill: solid alpha-aware fill with blend modes, feathering, and alpha inversion.
pub fn impact_fill(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let opacity = (fv(e, "opacity", cx) / 100.0).clamp(0.0, 1.0);
    if opacity <= 1e-5 || img.w == 0 || img.h == 0 {
        return;
    }
    let fill = lin(cv(e, "color", cx));
    let fill_a = cv(e, "color", cx)[3].clamp(0.0, 1.0);
    let mode = chv(e, "mode");
    let invert = bv(e, "invert_alpha");
    let feather = (fv(e, "feather", cx) * cx.px_scale * 0.5).max(0.0);
    let mut mask: Vec<f32> = alpha_of(img).into_iter().map(|a| if invert { (1.0 - a).clamp(0.0, 1.0) } else { a.clamp(0.0, 1.0) }).collect();
    if feather > 0.25 {
        mask = blur_plane(&mask, img.w, img.h, feather);
    }
    let k = opacity * fill_a;
    img.px.par_chunks_mut(4).zip(mask.par_iter()).for_each(|(p, &m)| {
        let ma = (m * k).clamp(0.0, 1.0);
        if mode == 0 {
            let keep = 1.0 - opacity;
            p[0] = p[0] * keep + fill[0] * ma;
            p[1] = p[1] * keep + fill[1] * ma;
            p[2] = p[2] * keep + fill[2] * ma;
            p[3] = p[3] * keep + ma;
        } else {
            let (src_c, a) = unpremul([p[0], p[1], p[2], p[3]]);
            let out_a = if invert { ma } else { a };
            let blended = blend_simple(mode, src_c, fill);
            for c in 0..3 {
                let target = blended[c].max(0.0) * out_a;
                p[c] = p[c] + (target - p[c]) * k;
            }
            if invert {
                p[3] = p[3] + (ma - p[3]) * opacity;
            }
        }
    });
}

/// Impact Gradient Tint: 3-zone (Shadow / Midtone / Highlight) luminance color map.
pub fn impact_gradient_tint(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let amount = (fv(e, "amount", cx) / 100.0).clamp(0.0, 1.0);
    if amount <= 1e-5 {
        return;
    }
    let sh = lin(cv(e, "shadow_color", cx));
    let mid = lin(cv(e, "midtone_color", cx));
    let hi = lin(cv(e, "highlight_color", cx));
    let mp = (fv(e, "midpoint", cx) / 100.0).clamp(0.05, 0.95);
    img.px.par_chunks_mut(4).for_each(|p| {
        let (rgb, a) = unpremul([p[0], p[1], p[2], p[3]]);
        if a <= 1e-6 {
            return;
        }
        let s = srgb3(rgb);
        let y = 0.2126 * s[0] + 0.7152 * s[1] + 0.0722 * s[2];
        let yc = y.clamp(0.0, 1.0);
        let mapped = if yc <= mp {
            let t = smoothstep(0.0, mp, yc);
            lerp3(sh, mid, t)
        } else {
            let t = smoothstep(mp, 1.0, yc);
            lerp3(mid, hi, t)
        };
        let hdr_boost = y.max(1.0);
        for c in 0..3 {
            let tc = mapped[c] * hdr_boost * a;
            p[c] = (p[c] + (tc - p[c]) * amount).max(0.0);
        }
    });
}

/// Impact Hue & Saturation: master HSL shift plus monochrome-to-hue Colorize mode.
pub fn impact_hue_saturation(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let hue_shift = fv(e, "hue", cx);
    let sat_delta = (fv(e, "saturation", cx) / 100.0).clamp(-1.0, 2.0);
    let light_delta = (fv(e, "lightness", cx) / 100.0).clamp(-1.0, 1.0);
    let colorize = bv(e, "colorize");
    let col_hue = fv(e, "colorize_hue", cx);
    let col_sat = (fv(e, "colorize_sat", cx) / 100.0).clamp(0.0, 1.0);
    if !colorize && hue_shift.abs() < 1e-4 && sat_delta.abs() < 1e-4 && light_delta.abs() < 1e-4 {
        return;
    }
    img.px.par_chunks_mut(4).for_each(|p| {
        let (rgb, a) = unpremul([p[0], p[1], p[2], p[3]]);
        if a <= 1e-6 {
            return;
        }
        let s = srgb3(rgb);
        let (mut h, mut sat, mut l) = rgb_to_hsl(s);
        if colorize {
            h = col_hue;
            sat = col_sat;
        } else {
            h += hue_shift;
            sat = if sat_delta >= 0.0 { sat + (1.0 - sat) * sat_delta.min(1.0) } else { sat * (1.0 + sat_delta) };
        }
        l = if light_delta >= 0.0 { l + (1.0 - l) * light_delta } else { l * (1.0 + light_delta) };
        let out_l = lin3(hsl_to_rgb(h, sat, l));
        p[0] = out_l[0] * a;
        p[1] = out_l[1] * a;
        p[2] = out_l[2] * a;
    });
}

fn quantize_smooth(v: f32, levels: f32, smooth: f32) -> f32 {
    let n = (levels - 1.0).max(1.0);
    let scaled = v.clamp(0.0, 1.0) * n;
    let base = scaled.floor();
    let frac = scaled - base;
    let edge = (0.5 * (1.0 - smooth)).clamp(0.0, 0.499);
    let t = smoothstep(0.5 - edge, 0.5 + edge, frac);
    ((base + t) / n).clamp(0.0, 1.0)
}

/// Impact Posterize: gamma-aware, anti-aliased tonal quantization with optional chroma preservation.
pub fn impact_posterize(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let blend = (fv(e, "blend", cx) / 100.0).clamp(0.0, 1.0);
    if blend >= 0.9999 {
        return;
    }
    let levels = fv(e, "levels", cx).clamp(2.0, 64.0);
    let gamma = fv(e, "gamma", cx).clamp(0.2, 4.0);
    let inv_gamma = 1.0 / gamma;
    let smooth = (fv(e, "smoothness", cx) / 100.0).clamp(0.0, 1.0);
    let luma_only = bv(e, "luma_only");
    img.px.par_chunks_mut(4).for_each(|p| {
        let (rgb, a) = unpremul([p[0], p[1], p[2], p[3]]);
        if a <= 1e-6 {
            return;
        }
        let s = srgb3(rgb);
        let out_s = if luma_only {
            let y = (0.2126 * s[0] + 0.7152 * s[1] + 0.0722 * s[2]).clamp(0.0, 1.0);
            let qy = quantize_smooth(y.powf(gamma), levels, smooth).powf(inv_gamma);
            let ratio = if y > 1e-5 { qy / y } else { 0.0 };
            [(s[0] * ratio).clamp(0.0, 1.0), (s[1] * ratio).clamp(0.0, 1.0), (s[2] * ratio).clamp(0.0, 1.0)]
        } else {
            [
                quantize_smooth(s[0].clamp(0.0, 1.0).powf(gamma), levels, smooth).powf(inv_gamma),
                quantize_smooth(s[1].clamp(0.0, 1.0).powf(gamma), levels, smooth).powf(inv_gamma),
                quantize_smooth(s[2].clamp(0.0, 1.0).powf(gamma), levels, smooth).powf(inv_gamma),
            ]
        };
        let out_l = lin3(out_s);
        for c in 0..3 {
            let post = out_l[c] * a;
            p[c] = post + (p[c] - post) * blend;
        }
    });
}

/// Impact Radial Blur: rotational spin blur centred on a point with optional highlight boost.
pub fn impact_radial_blur(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let deg = fv(e, "amount", cx).clamp(0.0, 180.0);
    let blend = (fv(e, "blend", cx) / 100.0).clamp(0.0, 1.0);
    if deg <= 0.05 || blend >= 0.9999 || img.w == 0 || img.h == 0 {
        return;
    }
    let c = pv(e, "center", cx, img);
    let (cxp, cyp) = (c.x as f32, c.y as f32);
    let half_rad = (deg * 0.5).to_radians();
    let samples = [9usize, 17, 33][chv(e, "quality").min(2) as usize];
    let thr = (fv(e, "threshold", cx) / 100.0).clamp(0.0, 1.0);
    let src = img.clone();
    let w = img.w;
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - cyp;
        for x in 0..w {
            let dx = x as f32 + 0.5 - cxp;
            let mut acc = [0.0f32; 4];
            let mut wsum = 0.0f32;
            for i in 0..samples {
                let u = if samples > 1 { i as f32 / (samples - 1) as f32 * 2.0 - 1.0 } else { 0.0 };
                let a = u * half_rad;
                let (sn, cs) = a.sin_cos();
                let sx = cxp + dx * cs - dy * sn;
                let sy = cyp + dx * sn + dy * cs;
                let s = src.sample_bilinear_clamped(sx, sy);
                let lum = luma([s[0], s[1], s[2]]);
                let wgt = (1.0 - 0.35 * u.abs()) * (1.0 + if thr > 0.0 && lum > thr { (lum - thr) * 1.5 } else { 0.0 });
                for k in 0..4 {
                    acc[k] += s[k] * wgt;
                }
                wsum += wgt;
            }
            let inv = 1.0 / wsum.max(1e-6);
            let dst = &mut row[x * 4..x * 4 + 4];
            for k in 0..4 {
                let blurred = (acc[k] * inv).max(0.0);
                dst[k] = blurred + (dst[k] - blurred) * blend;
            }
            dst[3] = dst[3].min(1.0);
        }
    });
}

/// Impact Zoom Blur: radial zoom streak filter with exponential falloff and highlight boost.
pub fn impact_zoom_blur(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let zoom = (fv(e, "amount", cx) / 100.0).clamp(-1.0, 2.0);
    let blend = (fv(e, "blend", cx) / 100.0).clamp(0.0, 1.0);
    if zoom.abs() <= 1e-4 || blend >= 0.9999 || img.w == 0 || img.h == 0 {
        return;
    }
    let c = pv(e, "center", cx, img);
    let (cxp, cyp) = (c.x as f32, c.y as f32);
    let falloff = (fv(e, "decay", cx) / 100.0).clamp(0.0, 1.0);
    let glow = (fv(e, "glow", cx) / 100.0).clamp(0.0, 2.0);
    let src = img.clone();
    let samples = 19usize;
    let w = img.w;
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - cyp;
        for x in 0..w {
            let dx = x as f32 + 0.5 - cxp;
            let mut acc = [0.0f32; 4];
            let mut wsum = 0.0f32;
            for i in 0..samples {
                let t = i as f32 / (samples - 1) as f32;
                let scale = (1.0 / (1.0 + zoom * t).max(0.15)).clamp(0.1, 4.0);
                let sx = cxp + dx * scale;
                let sy = cyp + dy * scale;
                let s = src.sample_bilinear_clamped(sx, sy);
                let decay_w = (-falloff * 3.0 * t).exp();
                let hi_w = 1.0 + glow * luma([s[0], s[1], s[2]]).clamp(0.0, 2.0) * (1.0 - 0.5 * t);
                let wgt = decay_w * hi_w;
                for k in 0..4 {
                    acc[k] += s[k] * wgt;
                }
                wsum += wgt;
            }
            let inv = 1.0 / wsum.max(1e-6);
            let dst = &mut row[x * 4..x * 4 + 4];
            for k in 0..4 {
                let blurred = (acc[k] * inv).max(0.0);
                dst[k] = blurred + (dst[k] - blurred) * blend;
            }
            dst[3] = dst[3].min(1.0);
        }
    });
}
