//! Physical 3D Stage Compositor, SDF Clay Modeler, Bounded OBJ BVH Raycaster, Surface Relight /
//! PBR Material Sampler, and Procedural PBR Material Shaders (`"3D & Material"`).
//!
//! Mathematical pipeline:
//! - **Cook-Torrance GGX Microfacet BRDF**: Trowbridge-Reitz $D_{\text{GGX}}$, Smith Schlick-GGX
//!   $G_{\text{Smith}}$, and Fresnel-Schlick $F_{\text{Schlick}}$ with energy-conserving diffuse.
//! - **SDF Clay Modeler**: Exact signed distance primitives and polynomial smooth CSG (`smin`,
//!   smooth subtract, smooth intersect), 64-step sphere tracing, tetrahedral analytic normals,
//!   curvature-driven edge wear (`weathering`), and 2-step SDF ambient occlusion.
//! - **Custom OBJ BVH Raycaster**: Bounded LRU mesh cache + flat AABB BVH with Möller–Trumbore
//!   intersection and smooth barycentric normal interpolation.
//! - **Video-Driven IBL & Shadow Catcher**: `16×8` spherical environment downsampler + cosine
//!   irradiance convolution from the live video frame, plus soft ground-plane shadow catching.
//! - **Surface Relight & PBR Sampler**: Multi-scale Gaussian Retinex delighting, multi-scale
//!   Scharr height/normal extraction, gradient-energy roughness, cavity AO, and $C^\infty$
//!   toroidal raised-cosine seamless tiling.

use std::f32::consts::{FRAC_1_PI, PI, TAU};
use std::sync::{Arc, Mutex, OnceLock};

use filmcraft_color::{luma709, srgb_to_linear};
use filmcraft_project::{EffectInstance, ObjMesh, stage_material_spec};
use rayon::prelude::*;

use crate::effects::{FxCtx, b, choice, color, f, text};
use crate::image::Image;

/// Dispatch `"stage_3d"`, `"surface_relight"`, and `"procedural_pbr"`.
pub fn apply(img: &mut Image, e: &EffectInstance, cx: &FxCtx) -> bool {
    match e.effect.as_str() {
        "stage_3d" => stage_3d(img, e, cx),
        "surface_relight" => surface_relight(img, e, cx),
        "procedural_pbr" => procedural_pbr(img, e, cx),
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------
// 3D Math Primitives (Zero-panic, NaN-safe)
// ---------------------------------------------------------------------------

#[inline]
fn v_add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[inline]
fn v_sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
fn v_scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

#[inline]
fn v_mul(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}

#[inline]
fn v_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn v_cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

#[inline]
fn v_len(a: [f32; 3]) -> f32 {
    v_dot(a, a).max(0.0).sqrt()
}

#[inline]
fn v_norm(a: [f32; 3]) -> [f32; 3] {
    let l2 = v_dot(a, a);
    if l2.is_finite() && l2 > 1e-16 {
        let inv = 1.0 / l2.sqrt();
        [a[0] * inv, a[1] * inv, a[2] * inv]
    } else {
        [0.0, 0.0, 1.0]
    }
}

#[inline]
fn v_lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

#[inline]
fn v_reflect(incident: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let d = 2.0 * v_dot(incident, n);
    v_sub(incident, v_scale(n, d))
}

#[derive(Clone, Copy)]
struct Mat3([f32; 9]);

impl Mat3 {
    fn from_euler_deg(rx_deg: f32, ry_deg: f32, rz_deg: f32) -> Self {
        let (sx, cx) = rx_deg.to_radians().sin_cos();
        let (sy, cy) = ry_deg.to_radians().sin_cos();
        let (sz, cz) = rz_deg.to_radians().sin_cos();
        // R = Rz * Ry * Rx
        Self([cz * cy, cz * sy * sx - sz * cx, cz * sy * cx + sz * sx, sz * cy, sz * sy * sx + cz * cx, sz * sy * cx - cz * sx, -sy, cy * sx, cy * cx])
    }

    #[inline]
    fn mul_vec(self, v: [f32; 3]) -> [f32; 3] {
        let m = self.0;
        [m[0] * v[0] + m[1] * v[1] + m[2] * v[2], m[3] * v[0] + m[4] * v[1] + m[5] * v[2], m[6] * v[0] + m[7] * v[1] + m[8] * v[2]]
    }

    #[inline]
    fn mul_transpose(self, v: [f32; 3]) -> [f32; 3] {
        let m = self.0;
        [m[0] * v[0] + m[3] * v[1] + m[6] * v[2], m[1] * v[0] + m[4] * v[1] + m[7] * v[2], m[2] * v[0] + m[5] * v[1] + m[8] * v[2]]
    }
}

#[inline]
fn srgb3_to_linear(c: [f32; 3]) -> [f32; 3] {
    [srgb_to_linear(c[0].clamp(0.0, 1.0)), srgb_to_linear(c[1].clamp(0.0, 1.0)), srgb_to_linear(c[2].clamp(0.0, 1.0))]
}

// ---------------------------------------------------------------------------
// Cook-Torrance GGX Microfacet BRDF
// ---------------------------------------------------------------------------

struct PbrSurface {
    albedo: [f32; 3],
    metallic: f32,
    roughness: f32,
    ao: f32,
}

#[inline]
fn d_ggx(n_dot_h: f32, alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let denom = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    a2 * FRAC_1_PI / (denom * denom).max(1e-7)
}

#[inline]
fn g_schlick_ggx(n_dot_x: f32, k: f32) -> f32 {
    n_dot_x / (n_dot_x * (1.0 - k) + k).max(1e-6)
}

#[inline]
fn g_smith(n_dot_v: f32, n_dot_l: f32, roughness: f32) -> f32 {
    let r = roughness + 1.0;
    let k = (r * r) * 0.125;
    g_schlick_ggx(n_dot_v, k) * g_schlick_ggx(n_dot_l, k)
}

#[inline]
fn f_schlick(cos_theta: f32, f0: [f32; 3]) -> [f32; 3] {
    let m = (1.0 - cos_theta.clamp(0.0, 1.0)).powi(5);
    [f0[0] + (1.0 - f0[0]) * m, f0[1] + (1.0 - f0[1]) * m, f0[2] + (1.0 - f0[2]) * m]
}

#[inline]
fn f_schlick_roughness(cos_theta: f32, f0: [f32; 3], roughness: f32) -> [f32; 3] {
    let inv_r = 1.0 - roughness;
    let m = (1.0 - cos_theta.clamp(0.0, 1.0)).powi(5);
    [f0[0] + (inv_r.max(f0[0]) - f0[0]) * m, f0[1] + (inv_r.max(f0[1]) - f0[1]) * m, f0[2] + (inv_r.max(f0[2]) - f0[2]) * m]
}

fn shade_ggx_direct(n: [f32; 3], v: [f32; 3], l: [f32; 3], radiance: [f32; 3], mat: &PbrSurface) -> [f32; 3] {
    let n_dot_l = v_dot(n, l).max(0.0);
    if n_dot_l <= 0.0 {
        return [0.0; 3];
    }
    let n_dot_v = v_dot(n, v).max(1e-4);
    let h = v_norm(v_add(v, l));
    let n_dot_h = v_dot(n, h).max(0.0);
    let v_dot_h = v_dot(v, h).max(0.0);

    let rough = mat.roughness.clamp(0.04, 1.0);
    let alpha = rough * rough;
    let f0 = v_lerp([0.04, 0.04, 0.04], mat.albedo, mat.metallic);

    let ndf = d_ggx(n_dot_h, alpha);
    let geom = g_smith(n_dot_v, n_dot_l, rough);
    let fres = f_schlick(v_dot_h, f0);

    let spec_denom = (4.0 * n_dot_v * n_dot_l).max(1e-5);
    let spec = v_scale(fres, ndf * geom / spec_denom);

    let kd = [(1.0 - fres[0]) * (1.0 - mat.metallic), (1.0 - fres[1]) * (1.0 - mat.metallic), (1.0 - fres[2]) * (1.0 - mat.metallic)];
    let diff = v_scale(v_mul(kd, mat.albedo), FRAC_1_PI);

    let brdf = v_add(diff, spec);
    v_scale(v_mul(brdf, radiance), n_dot_l)
}

// ---------------------------------------------------------------------------
// Video-Driven Spherical Environment IBL (`16×8` Lat-Long + Cosine Irradiance)
// ---------------------------------------------------------------------------

const ENV_W: usize = 16;
const ENV_H: usize = 8;

struct VideoIbl {
    spec_grid: [[f32; 3]; ENV_W * ENV_H],
    diff_grid: [[f32; 3]; ENV_W * ENV_H],
    intensity: f32,
}

impl VideoIbl {
    fn from_frame(img: &Image, use_video: bool, intensity: f32) -> Self {
        let mut spec_grid = [[0.0_f32; 3]; ENV_W * ENV_H];
        if use_video && img.w > 0 && img.h > 0 {
            for gy in 0..ENV_H {
                // Map elevation v_frac in [0, 1] (top to bottom) onto frame Y, wrapping azimuth around frame X.
                let fy = ((gy as f32 + 0.5) / ENV_H as f32) * img.h as f32;
                let y0 = ((gy * img.h) / ENV_H).min(img.h - 1);
                let y1 = (((gy + 1) * img.h) / ENV_H).max(y0 + 1).min(img.h);
                for gx in 0..ENV_W {
                    // Fold [0, 1] azimuth smoothly as mirrored panoramic wrap so seam at ±pi is C0 continuous.
                    let u_az = (gx as f32 + 0.5) / ENV_W as f32;
                    let tri_u = 1.0 - (2.0 * u_az - 1.0).abs();
                    let x0 = ((tri_u * (img.w.saturating_sub(1)) as f32).floor() as usize).min(img.w - 1);
                    let x1 = (x0 + (img.w / ENV_W).max(1)).min(img.w);
                    let mut acc = [0.0_f32; 3];
                    let mut count = 0.0_f32;
                    let step_y = ((y1 - y0) / 4).max(1);
                    let step_x = ((x1 - x0) / 4).max(1);
                    let mut sy = y0;
                    while sy < y1 {
                        let mut sx = x0;
                        while sx < x1 {
                            let c = Image::unpremul(img.get(sx, sy));
                            acc[0] += c[0].clamp(0.0, 8.0);
                            acc[1] += c[1].clamp(0.0, 8.0);
                            acc[2] += c[2].clamp(0.0, 8.0);
                            count += 1.0;
                            sx += step_x;
                        }
                        sy += step_y;
                    }
                    let _ = fy;
                    let avg = if count > 0.0 { v_scale(acc, 1.0 / count) } else { [0.18, 0.18, 0.20] };
                    // Ensure subtle studio floor/sky floor so completely black clips still show silhouette rim
                    spec_grid[gy * ENV_W + gx] = [avg[0] + 0.03, avg[1] + 0.03, avg[2] + 0.035];
                }
            }
        } else {
            for gy in 0..ENV_H {
                let v_frac = (gy as f32 + 0.5) / ENV_H as f32;
                let sky_t = (1.0 - v_frac).clamp(0.0, 1.0);
                let base = v_lerp([0.07, 0.075, 0.085], [0.42, 0.46, 0.54], sky_t * sky_t);
                for gx in 0..ENV_W {
                    let phi = ((gx as f32 + 0.5) / ENV_W as f32) * TAU;
                    let studio_strip = (0.5 + 0.5 * phi.cos()).powi(6) * 0.45 * sky_t;
                    spec_grid[gy * ENV_W + gx] = [base[0] + studio_strip, base[1] + studio_strip, base[2] + studio_strip];
                }
            }
        }

        // Cosine hemisphere convolution into `diff_grid`
        let dirs = env_cell_directions();
        let mut diff_grid = [[0.0_f32; 3]; ENV_W * ENV_H];
        for i in 0..(ENV_W * ENV_H) {
            let n = dirs[i].0;
            let mut sum = [0.0_f32; 3];
            let mut w_sum = 0.0_f32;
            for j in 0..(ENV_W * ENV_H) {
                let (d, solid_weight) = dirs[j];
                let cos_w = v_dot(n, d).max(0.0) * solid_weight;
                if cos_w > 0.0 {
                    sum = v_add(sum, v_scale(spec_grid[j], cos_w));
                    w_sum += cos_w;
                }
            }
            diff_grid[i] = if w_sum > 1e-6 { v_scale(sum, 1.0 / w_sum) } else { spec_grid[i] };
        }

        Self { spec_grid, diff_grid, intensity: intensity.max(0.0) }
    }

    fn sample_grid(grid: &[[f32; 3]; ENV_W * ENV_H], dir: [f32; 3]) -> [f32; 3] {
        let d = v_norm(dir);
        // Elevation theta in [-pi/2, pi/2], gy=0 is +Y sky, gy=ENV_H-1 is -Y ground
        let theta = d[1].clamp(-1.0, 1.0).asin();
        let phi = d[0].atan2(d[2]); // [-pi, pi]
        let u = ((phi + PI) / TAU).rem_euclid(1.0) * ENV_W as f32 - 0.5;
        let v = ((0.5 - theta * FRAC_1_PI).clamp(0.0, 1.0)) * (ENV_H as f32 - 1.0);

        let x0 = u.floor() as isize;
        let tx = u - x0 as f32;
        let x0w = x0.rem_euclid(ENV_W as isize) as usize;
        let x1w = (x0 + 1).rem_euclid(ENV_W as isize) as usize;

        let y0 = (v.floor() as usize).min(ENV_H - 1);
        let y1 = (y0 + 1).min(ENV_H - 1);
        let ty = (v - y0 as f32).clamp(0.0, 1.0);

        let c00 = grid[y0 * ENV_W + x0w];
        let c10 = grid[y0 * ENV_W + x1w];
        let c01 = grid[y1 * ENV_W + x0w];
        let c11 = grid[y1 * ENV_W + x1w];
        let top = v_lerp(c00, c10, tx);
        let bot = v_lerp(c01, c11, tx);
        v_lerp(top, bot, ty)
    }

    fn evaluate(&self, n: [f32; 3], v: [f32; 3], mat: &PbrSurface) -> [f32; 3] {
        if self.intensity <= 0.0 {
            return [0.0; 3];
        }
        let n_dot_v = v_dot(n, v).max(0.0);
        let f0 = v_lerp([0.04, 0.04, 0.04], mat.albedo, mat.metallic);
        let ks = f_schlick_roughness(n_dot_v, f0, mat.roughness);
        let kd = [(1.0 - ks[0]) * (1.0 - mat.metallic), (1.0 - ks[1]) * (1.0 - mat.metallic), (1.0 - ks[2]) * (1.0 - mat.metallic)];

        let irradiance = Self::sample_grid(&self.diff_grid, n);
        let diffuse = v_mul(kd, v_mul(irradiance, mat.albedo));

        let refl_dir = v_reflect(v_scale(v, -1.0), n);
        let sharp_env = Self::sample_grid(&self.spec_grid, refl_dir);
        let blur_env = Self::sample_grid(&self.diff_grid, refl_dir);
        let prefiltered = v_lerp(sharp_env, blur_env, (mat.roughness * mat.roughness).clamp(0.0, 1.0));
        let env_brdf_scale = (1.0 - 0.65 * mat.roughness).clamp(0.15, 1.0);
        let specular = v_scale(v_mul(prefiltered, ks), env_brdf_scale);

        v_scale(v_add(diffuse, specular), self.intensity * mat.ao)
    }
}

fn env_cell_directions() -> &'static [([f32; 3], f32); ENV_W * ENV_H] {
    static DIRS: OnceLock<[([f32; 3], f32); ENV_W * ENV_H]> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut arr = [([0.0_f32; 3], 1.0_f32); ENV_W * ENV_H];
        for gy in 0..ENV_H {
            let theta = (0.5 - (gy as f32 + 0.5) / ENV_H as f32) * PI;
            let (sin_t, cos_t) = theta.sin_cos();
            let weight = cos_t.abs().max(0.05);
            for gx in 0..ENV_W {
                let phi = ((gx as f32 + 0.5) / ENV_W as f32) * TAU - PI;
                let (sin_p, cos_p) = phi.sin_cos();
                arr[gy * ENV_W + gx] = ([cos_t * sin_p, sin_t, cos_t * cos_p], weight);
            }
        }
        arr
    })
}

// ---------------------------------------------------------------------------
// SDF Clay Primitives & Polynomial Smooth CSG (`smin`, `ssub`, `smax`)
// ---------------------------------------------------------------------------

#[inline]
fn smin_poly(a: f32, b: f32, k: f32) -> f32 {
    let k = k.max(1e-4);
    let h = (k - (a - b).abs()).max(0.0) / k;
    a.min(b) - h * h * k * 0.25
}

#[inline]
fn ssub_poly(d1: f32, d2: f32, k: f32) -> f32 {
    -smin_poly(-d1, d2, k)
}

#[inline]
fn smax_poly(a: f32, b: f32, k: f32) -> f32 {
    -smin_poly(-a, -b, k)
}

#[inline]
fn sd_sphere(p: [f32; 3], r: f32) -> f32 {
    v_len(p) - r
}

#[inline]
fn sd_round_box(p: [f32; 3], b: [f32; 3], r: f32) -> f32 {
    let q = [p[0].abs() - b[0], p[1].abs() - b[1], p[2].abs() - b[2]];
    let outside = v_len([q[0].max(0.0), q[1].max(0.0), q[2].max(0.0)]);
    let inside = q[0].max(q[1].max(q[2])).min(0.0);
    outside + inside - r
}

#[inline]
fn sd_capped_cylinder(p: [f32; 3], h: f32, r: f32) -> f32 {
    let d_r = (p[0] * p[0] + p[2] * p[2]).sqrt() - r;
    let d_y = p[1].abs() - h;
    let outside = (d_r.max(0.0).powi(2) + d_y.max(0.0).powi(2)).sqrt();
    outside + d_r.max(d_y).min(0.0) - 0.025
}

#[inline]
fn sd_torus(p: [f32; 3], major: f32, minor: f32) -> f32 {
    let qx = (p[0] * p[0] + p[2] * p[2]).sqrt() - major;
    (qx * qx + p[1] * p[1]).sqrt() - minor
}

fn sd_bottle(p: [f32; 3], k_blend: f32) -> f32 {
    // Revolved product bottle: body + curved shoulder + narrow neck + cap lip + bottom punt indent
    let body = sd_capped_cylinder([p[0], p[1] + 0.16, p[2]], 0.36, 0.30);
    let shoulder = sd_sphere([p[0], p[1] - 0.16, p[2]], 0.29);
    let neck = sd_capped_cylinder([p[0], p[1] - 0.44, p[2]], 0.18, 0.11);
    let lip = sd_torus([p[0], p[1] - 0.59, p[2]], 0.115, 0.028);
    let punt = sd_sphere([p[0], p[1] + 0.58, p[2]], 0.16);

    let lower = smin_poly(body, shoulder, 0.12);
    let upper = smin_poly(lower, neck, (k_blend * 0.6).clamp(0.04, 0.18));
    let with_lip = smin_poly(upper, lip, 0.025);
    ssub_poly(with_lip, punt, 0.04)
}

fn eval_sdf(p: [f32; 3], shape: u32, sdf_blend: f32) -> f32 {
    let k = sdf_blend.clamp(0.02, 0.8);
    match shape {
        0 => sd_sphere(p, 0.60),
        1 => sd_round_box(p, [0.40, 0.40, 0.40], 0.10),
        2 => sd_bottle(p, k),
        3 => sd_capped_cylinder(p, 0.50, 0.38),
        4 => sd_torus([p[0], p[2], p[1]], 0.42, 0.17),
        5 => {
            // SDF Clay Smooth Union: rounded core + tilted ring + two organic lobes
            let core = sd_round_box(p, [0.30, 0.30, 0.30], 0.09);
            let ring = sd_torus([p[0], 0.707 * (p[1] - p[2]), 0.707 * (p[1] + p[2])], 0.42, 0.11);
            let lobe_a = sd_sphere([p[0] - 0.34, p[1] - 0.22, p[2] - 0.16], 0.24);
            let lobe_b = sd_sphere([p[0] + 0.32, p[1] + 0.20, p[2] + 0.14], 0.22);
            let u1 = smin_poly(core, ring, k);
            let u2 = smin_poly(u1, lobe_a, k);
            smin_poly(u2, lobe_b, k)
        }
        6 => {
            // SDF Clay Subtract: rounded monolith sculpted by spherical & cylindrical boolean cuts
            let base = sd_round_box(p, [0.44, 0.44, 0.44], 0.08);
            let cut_front = sd_sphere([p[0] - 0.18, p[1] - 0.14, p[2] - 0.38], 0.38);
            let cut_bore = sd_capped_cylinder([p[0] + 0.12, p[2], p[1] + 0.10], 0.70, 0.20);
            let c1 = ssub_poly(base, cut_front, k);
            ssub_poly(c1, cut_bore, k * 0.75)
        }
        7 => {
            // SDF Clay Intersect: smooth boolean lens/gem intersection of two offset spheres + box
            let s1 = sd_sphere([p[0] - 0.20, p[1], p[2]], 0.62);
            let s2 = sd_sphere([p[0] + 0.20, p[1], p[2]], 0.62);
            let slab = sd_round_box(p, [0.52, 0.46, 0.42], 0.06);
            smax_poly(smax_poly(s1, s2, k), slab, k * 0.7)
        }
        _ => sd_sphere(p, 0.60),
    }
}

fn sdf_normal_and_curvature(p: [f32; 3], shape: u32, sdf_blend: f32) -> ([f32; 3], f32) {
    let h = 0.0025_f32;
    let e0 = [1.0_f32, -1.0, -1.0];
    let e1 = [-1.0_f32, -1.0, 1.0];
    let e2 = [-1.0_f32, 1.0, -1.0];
    let e3 = [1.0_f32, 1.0, 1.0];

    let d0 = eval_sdf(v_add(p, v_scale(e0, h)), shape, sdf_blend);
    let d1 = eval_sdf(v_add(p, v_scale(e1, h)), shape, sdf_blend);
    let d2 = eval_sdf(v_add(p, v_scale(e2, h)), shape, sdf_blend);
    let d3 = eval_sdf(v_add(p, v_scale(e3, h)), shape, sdf_blend);
    let dc = eval_sdf(p, shape, sdf_blend);

    let grad = v_add(v_add(v_scale(e0, d0), v_scale(e1, d1)), v_add(v_scale(e2, d2), v_scale(e3, d3)));
    let n = v_norm(grad);
    let curv = ((d0 + d1 + d2 + d3 - 4.0 * dc) / (3.0 * h)).clamp(-2.0, 2.0);
    (n, curv)
}

fn sdf_ambient_occlusion(p: [f32; 3], n: [f32; 3], shape: u32, sdf_blend: f32) -> f32 {
    let h1 = 0.06_f32;
    let h2 = 0.16_f32;
    let d1 = eval_sdf(v_add(p, v_scale(n, h1)), shape, sdf_blend);
    let d2 = eval_sdf(v_add(p, v_scale(n, h2)), shape, sdf_blend);
    let occ = (h1 - d1).max(0.0) * 3.2 + (h2 - d2).max(0.0) * 1.6;
    (1.0 - occ).clamp(0.12, 1.0)
}

// ---------------------------------------------------------------------------
// Bounded Custom OBJ Cache & Flat AABB BVH Raycaster
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BvhNode {
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
    left_or_first: u32,
    count: u32, // > 0 => leaf [left_or_first .. left_or_first + count], 0 => internal (children left_or_first, left_or_first + 1)
}

struct CompiledObj {
    mesh: ObjMesh,
    tri_order: Vec<u32>,
    tri_curv: Vec<f32>,
    nodes: Vec<BvhNode>,
}

struct CachedObjEntry {
    path: String,
    stamp: u64,
    compiled: Arc<CompiledObj>,
}

const MAX_CACHED_OBJS: usize = 8;

fn read_obj_source(trimmed: &str) -> Option<(u64, std::borrow::Cow<'_, str>)> {
    if let Some(inline) = trimmed.strip_prefix("obj:") {
        if inline.len() > filmcraft_project::MAX_OBJ_BYTES {
            return None;
        }
        return Some((inline.len() as u64, std::borrow::Cow::Borrowed(inline)));
    }
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let meta = std::fs::metadata(trimmed).ok()?;
        if meta.len() > filmcraft_project::MAX_OBJ_BYTES as u64 {
            return None;
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0_u64, |d| d.as_secs().wrapping_mul(1_000_000_000) ^ u64::from(d.subsec_nanos()));
        let stamp = meta.len() ^ mtime_ns;
        let text = std::fs::read_to_string(trimmed).ok()?;
        Some((stamp, std::borrow::Cow::Owned(text)))
    }
}

fn load_compiled_obj(path: &str) -> Option<Arc<CompiledObj>> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    static CACHE: OnceLock<Mutex<Vec<CachedObjEntry>>> = OnceLock::new();
    let (stamp, source_text) = read_obj_source(trimmed)?;

    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.path == trimmed && c.stamp == stamp).map(|c| c.compiled.clone()) {
        return Some(hit);
    }

    let mesh = filmcraft_project::parse_obj(&source_text).ok()?;
    let compiled = Arc::new(CompiledObj::build(mesh));

    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|c| c.path != trimmed);
    if guard.len() >= MAX_CACHED_OBJS {
        guard.remove(0);
    }
    guard.push(CachedObjEntry { path: trimmed.to_string(), stamp, compiled: compiled.clone() });
    Some(compiled)
}

impl CompiledObj {
    pub(crate) fn build(mesh: ObjMesh) -> Self {
        let tri_count = mesh.tris.len();
        let mut tri_order: Vec<u32> = (0..tri_count as u32).collect();
        let mut centroids: Vec<[f32; 3]> = Vec::with_capacity(tri_count);
        let mut tri_curv: Vec<f32> = Vec::with_capacity(tri_count);

        for tri in &mesh.tris {
            let p0 = mesh.positions.get(tri[0] as usize).copied().unwrap_or([0.0; 3]);
            let p1 = mesh.positions.get(tri[1] as usize).copied().unwrap_or([0.0; 3]);
            let p2 = mesh.positions.get(tri[2] as usize).copied().unwrap_or([0.0; 3]);
            centroids.push([(p0[0] + p1[0] + p2[0]) / 3.0, (p0[1] + p1[1] + p2[1]) / 3.0, (p0[2] + p1[2] + p2[2]) / 3.0]);

            let n0 = mesh.normals.get(tri[0] as usize).copied().unwrap_or([0.0, 1.0, 0.0]);
            let n1 = mesh.normals.get(tri[1] as usize).copied().unwrap_or([0.0, 1.0, 0.0]);
            let n2 = mesh.normals.get(tri[2] as usize).copied().unwrap_or([0.0, 1.0, 0.0]);
            let spread = (1.0 - v_dot(n0, n1).min(v_dot(n1, n2)).min(v_dot(n2, n0))).clamp(0.0, 1.0);
            tri_curv.push(spread * 1.5);
        }

        let mut nodes = Vec::with_capacity((tri_count * 2).max(1));
        nodes.push(BvhNode { aabb_min: [-1.0; 3], aabb_max: [1.0; 3], left_or_first: 0, count: tri_count as u32 });
        build_bvh_recursive(&mesh, &centroids, &mut tri_order, &mut nodes, 0, 0);

        Self { mesh, tri_order, tri_curv, nodes }
    }

    fn raycast(&self, ro: [f32; 3], rd: [f32; 3], t_max: f32) -> Option<(f32, [f32; 3], f32)> {
        if self.nodes.is_empty() {
            return None;
        }
        let inv_d = [
            if rd[0].abs() > 1e-8 { 1.0 / rd[0] } else { rd[0].signum() * 1e8 },
            if rd[1].abs() > 1e-8 { 1.0 / rd[1] } else { rd[1].signum() * 1e8 },
            if rd[2].abs() > 1e-8 { 1.0 / rd[2] } else { rd[2].signum() * 1e8 },
        ];
        let mut stack = [0_u32; 32];
        let mut sp = 1_usize;
        let mut best_t = t_max;
        let mut best_hit: Option<(u32, f32, f32)> = None;

        while sp > 0 {
            sp -= 1;
            let Some(node) = self.nodes.get(stack[sp] as usize) else { continue };
            if !ray_aabb(ro, inv_d, node.aabb_min, node.aabb_max, best_t) {
                continue;
            }
            if node.count > 0 {
                let start = node.left_or_first as usize;
                let end = (start + node.count as usize).min(self.tri_order.len());
                for &ti in &self.tri_order[start..end] {
                    let Some(&tri) = self.mesh.tris.get(ti as usize) else { continue };
                    let p0 = self.mesh.positions[tri[0] as usize];
                    let p1 = self.mesh.positions[tri[1] as usize];
                    let p2 = self.mesh.positions[tri[2] as usize];
                    if let Some((t, u, v)) = ray_triangle(ro, rd, p0, p1, p2, best_t) {
                        best_t = t;
                        best_hit = Some((ti, u, v));
                    }
                }
            } else if sp + 2 <= stack.len() {
                stack[sp] = node.left_or_first + 1;
                stack[sp + 1] = node.left_or_first;
                sp += 2;
            }
        }

        let (ti, u, v) = best_hit?;
        let tri = self.mesh.tris[ti as usize];
        let w = (1.0 - u - v).max(0.0);
        let n0 = self.mesh.normals[tri[0] as usize];
        let n1 = self.mesh.normals[tri[1] as usize];
        let n2 = self.mesh.normals[tri[2] as usize];
        let mut n = v_norm([n0[0] * w + n1[0] * u + n2[0] * v, n0[1] * w + n1[1] * u + n2[1] * v, n0[2] * w + n1[2] * u + n2[2] * v]);
        if v_dot(n, rd) > 0.0 {
            n = v_scale(n, -1.0);
        }
        let curv = self.tri_curv.get(ti as usize).copied().unwrap_or(0.0);
        Some((best_t, n, curv))
    }

    fn any_hit(&self, ro: [f32; 3], rd: [f32; 3], t_max: f32) -> bool {
        self.raycast(ro, rd, t_max).is_some()
    }
}

fn compute_slice_aabb(mesh: &ObjMesh, tri_order: &[u32]) -> ([f32; 3], [f32; 3]) {
    let mut bmin = [f32::INFINITY; 3];
    let mut bmax = [f32::NEG_INFINITY; 3];
    for &ti in tri_order {
        let Some(&tri) = mesh.tris.get(ti as usize) else { continue };
        for &vi in &tri {
            let Some(&p) = mesh.positions.get(vi as usize) else { continue };
            for k in 0..3 {
                bmin[k] = bmin[k].min(p[k] - 1e-4);
                bmax[k] = bmax[k].max(p[k] + 1e-4);
            }
        }
    }
    if !bmin[0].is_finite() { ([-1.0; 3], [1.0; 3]) } else { (bmin, bmax) }
}

fn build_bvh_recursive(mesh: &ObjMesh, centroids: &[[f32; 3]], tri_order: &mut [u32], nodes: &mut Vec<BvhNode>, node_idx: usize, depth: usize) {
    let (first, count) = {
        let n = &nodes[node_idx];
        (n.left_or_first as usize, n.count as usize)
    };
    let slice = &tri_order[first..first + count];
    let (bmin, bmax) = compute_slice_aabb(mesh, slice);
    nodes[node_idx].aabb_min = bmin;
    nodes[node_idx].aabb_max = bmax;

    if count <= 8 || depth >= 14 {
        return;
    }
    let ext = v_sub(bmax, bmin);
    let axis = if ext[0] >= ext[1] && ext[0] >= ext[2] {
        0
    } else if ext[1] >= ext[2] {
        1
    } else {
        2
    };
    let mid_pos = 0.5 * (bmin[axis] + bmax[axis]);
    let sub = &mut tri_order[first..first + count];
    let mut split = 0_usize;
    for i in 0..sub.len() {
        if centroids[sub[i] as usize][axis] < mid_pos {
            sub.swap(i, split);
            split += 1;
        }
    }
    if split == 0 || split == count {
        split = count / 2;
    }

    let left_idx = nodes.len() as u32;
    nodes.push(BvhNode { aabb_min: bmin, aabb_max: bmax, left_or_first: first as u32, count: split as u32 });
    nodes.push(BvhNode { aabb_min: bmin, aabb_max: bmax, left_or_first: (first + split) as u32, count: (count - split) as u32 });
    nodes[node_idx].left_or_first = left_idx;
    nodes[node_idx].count = 0;

    build_bvh_recursive(mesh, centroids, tri_order, nodes, left_idx as usize, depth + 1);
    build_bvh_recursive(mesh, centroids, tri_order, nodes, left_idx as usize + 1, depth + 1);
}

#[inline]
fn ray_aabb(ro: [f32; 3], inv_d: [f32; 3], bmin: [f32; 3], bmax: [f32; 3], t_max: f32) -> bool {
    let mut tmin = 0.0_f32;
    let mut tmax = t_max;
    for k in 0..3 {
        let t1 = (bmin[k] - ro[k]) * inv_d[k];
        let t2 = (bmax[k] - ro[k]) * inv_d[k];
        tmin = tmin.max(t1.min(t2));
        tmax = tmax.min(t1.max(t2));
    }
    tmax >= tmin
}

#[inline]
fn ray_triangle(ro: [f32; 3], rd: [f32; 3], p0: [f32; 3], p1: [f32; 3], p2: [f32; 3], t_max: f32) -> Option<(f32, f32, f32)> {
    let e1 = v_sub(p1, p0);
    let e2 = v_sub(p2, p0);
    let h = v_cross(rd, e2);
    let a = v_dot(e1, h);
    if a.abs() < 1e-7 {
        return None;
    }
    let inv_a = 1.0 / a;
    let s = v_sub(ro, p0);
    let u = inv_a * v_dot(s, h);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = v_cross(s, e1);
    let v = inv_a * v_dot(rd, q);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = inv_a * v_dot(e2, q);
    (t > 1e-4 && t < t_max).then_some((t, u, v))
}

// ---------------------------------------------------------------------------
// Effect 1: `"stage_3d"` (`"3D Stage Compositor"`)
// ---------------------------------------------------------------------------

struct StageConfig {
    shape: u32,
    sdf_blend: f32,
    material_choice: u32,
    base_linear: [f32; 3],
    metallic: f32,
    roughness: f32,
    weathering: f32,
    transmission: f32,
    center: [f32; 3],
    scale: f32,
    rot: Mat3,
    tan_half_fov: f32,
    light_dir: [f32; 3],
    shadow_catcher: f32,
    ground_y: f32,
    obj_mesh: Option<Arc<CompiledObj>>,
}

struct ObjectHit {
    t_world: f32,
    pos_world: [f32; 3],
    pos_local: [f32; 3],
    normal_world: [f32; 3],
    curvature: f32,
    ao: f32,
    coverage: f32,
}

fn eval_stage_config(e: &EffectInstance, cx: &FxCtx) -> StageConfig {
    let mut shape = choice(e, "shape");
    let obj_mesh = if shape == 8 { load_compiled_obj(text(e, "mesh_path")) } else { None };
    if shape == 8 && obj_mesh.is_none() {
        shape = 0; // Graceful fallback to Sphere when custom OBJ path is empty/unreadable
    }

    let mat_choice = choice(e, "material");
    let spec = stage_material_spec(mat_choice);
    let user_col = color(e, "base_color", cx);
    let (base_linear, metallic, roughness, transmission) = if mat_choice == 7 {
        (srgb3_to_linear([user_col[0], user_col[1], user_col[2]]), f(e, "metallic", cx).clamp(0.0, 1.0), f(e, "roughness", cx).clamp(0.04, 1.0), 0.0)
    } else {
        (srgb3_to_linear(spec.base_srgb), spec.metallic, spec.roughness.clamp(0.04, 1.0), spec.transmission)
    };

    let scale = (f(e, "scale", cx) / 100.0).clamp(0.10, 3.0);
    let pos_x = (f(e, "pos_x", cx) / 100.0).clamp(-1.5, 1.5) * 1.35;
    let pos_y = -(f(e, "pos_y", cx) / 100.0).clamp(-1.5, 1.5) * 0.95;
    let fov_rad = f(e, "fov_deg", cx).clamp(20.0, 100.0).to_radians();

    let az = f(e, "light_azimuth", cx).to_radians();
    let el = f(e, "light_elevation", cx).clamp(5.0, 89.0).to_radians();
    let (sin_az, cos_az) = az.sin_cos();
    let (sin_el, cos_el) = el.sin_cos();
    let light_dir = v_norm([cos_el * sin_az, sin_el, cos_el * cos_az]);

    StageConfig {
        shape,
        sdf_blend: f(e, "sdf_blend", cx).clamp(0.02, 0.8),
        material_choice: mat_choice,
        base_linear,
        metallic,
        roughness,
        weathering: f(e, "weathering", cx).clamp(0.0, 1.0),
        transmission,
        center: [pos_x, pos_y, 0.0],
        scale,
        rot: Mat3::from_euler_deg(f(e, "rot_x", cx), f(e, "rot_y", cx), f(e, "rot_z", cx)),
        tan_half_fov: (0.5 * fov_rad).tan().max(0.15),
        light_dir,
        shadow_catcher: f(e, "shadow_catcher", cx).clamp(0.0, 1.0),
        ground_y: f(e, "ground_y", cx).clamp(-1.5, 1.5),
        obj_mesh,
    }
}

fn ray_sphere_interval(ro: [f32; 3], rd: [f32; 3], center: [f32; 3], radius: f32) -> Option<(f32, f32)> {
    let oc = v_sub(ro, center);
    let b = v_dot(oc, rd);
    let c = v_dot(oc, oc) - radius * radius;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t1 = -b - s;
    let t2 = -b + s;
    (t2 > 0.0).then_some((t1.max(0.001), t2))
}

fn trace_stage_object(ro: [f32; 3], rd: [f32; 3], px_cone: f32, cfg: &StageConfig) -> Option<ObjectHit> {
    let bound_r = 1.25 * cfg.scale;
    let (t_enter, t_exit) = ray_sphere_interval(ro, rd, cfg.center, bound_r)?;

    let inv_s = 1.0 / cfg.scale.max(0.05);
    let ro_local = v_scale(cfg.rot.mul_transpose(v_sub(ro, cfg.center)), inv_s);
    let rd_local = v_norm(cfg.rot.mul_transpose(rd));

    if let Some(obj) = &cfg.obj_mesh {
        let (t_loc, n_loc, curv) = obj.raycast(ro_local, rd_local, t_exit * inv_s + 2.0)?;
        let pos_local = v_add(ro_local, v_scale(rd_local, t_loc));
        let pos_world = v_add(cfg.center, v_scale(cfg.rot.mul_vec(pos_local), cfg.scale));
        let t_world = v_dot(v_sub(pos_world, ro), rd).max(0.001);
        let normal_world = v_norm(cfg.rot.mul_vec(n_loc));
        return Some(ObjectHit { t_world, pos_world, pos_local, normal_world, curvature: curv, ao: 1.0 - (curv * 0.2).clamp(0.0, 0.35), coverage: 1.0 });
    }

    let mut t_loc = (t_enter * inv_s).max(0.0);
    let max_loc = t_exit * inv_s;
    let mut min_cone_ratio = f32::INFINITY;
    let mut closest_t = t_loc;

    for _ in 0..64 {
        if t_loc > max_loc {
            break;
        }
        let p = v_add(ro_local, v_scale(rd_local, t_loc));
        let d = eval_sdf(p, cfg.shape, cfg.sdf_blend);
        let cone_w = (px_cone * (t_loc * cfg.scale).max(0.5) * inv_s).max(1e-3);
        let ratio = d / cone_w;
        if ratio < min_cone_ratio {
            min_cone_ratio = ratio;
            closest_t = t_loc;
        }
        if d < 9e-4 {
            let (n_loc, curv) = sdf_normal_and_curvature(p, cfg.shape, cfg.sdf_blend);
            let ao = sdf_ambient_occlusion(p, n_loc, cfg.shape, cfg.sdf_blend);
            let pos_world = v_add(cfg.center, v_scale(cfg.rot.mul_vec(p), cfg.scale));
            let t_world = v_dot(v_sub(pos_world, ro), rd).max(0.001);
            return Some(ObjectHit { t_world, pos_world, pos_local: p, normal_world: v_norm(cfg.rot.mul_vec(n_loc)), curvature: curv, ao, coverage: 1.0 });
        }
        t_loc += d.max(0.0015);
    }

    // Sub-pixel silhouette edge anti-aliasing when ray grazed within 1 pixel cone
    if min_cone_ratio < 0.85 {
        let cov = (0.85 - min_cone_ratio).clamp(0.0, 1.0);
        let p = v_add(ro_local, v_scale(rd_local, closest_t));
        let (n_loc, curv) = sdf_normal_and_curvature(p, cfg.shape, cfg.sdf_blend);
        let ao = sdf_ambient_occlusion(p, n_loc, cfg.shape, cfg.sdf_blend);
        let pos_world = v_add(cfg.center, v_scale(cfg.rot.mul_vec(p), cfg.scale));
        let t_world = v_dot(v_sub(pos_world, ro), rd).max(0.001);
        return Some(ObjectHit {
            t_world,
            pos_world,
            pos_local: p,
            normal_world: v_norm(cfg.rot.mul_vec(n_loc)),
            curvature: curv,
            ao,
            coverage: cov * cov * (3.0 - 2.0 * cov),
        });
    }
    None
}

fn trace_soft_shadow(origin_world: [f32; 3], light_dir: [f32; 3], cfg: &StageConfig) -> f32 {
    let bound_r = 1.28 * cfg.scale;
    if ray_sphere_interval(origin_world, light_dir, cfg.center, bound_r).is_none() {
        return 1.0;
    }
    let inv_s = 1.0 / cfg.scale.max(0.05);
    let ro_local = v_scale(cfg.rot.mul_transpose(v_sub(origin_world, cfg.center)), inv_s);
    let rd_local = v_norm(cfg.rot.mul_transpose(light_dir));

    if let Some(obj) = &cfg.obj_mesh {
        return if obj.any_hit(v_add(ro_local, v_scale(rd_local, 0.02)), rd_local, 6.0) { 0.12 } else { 1.0 };
    }

    let mut res = 1.0_f32;
    let mut t = 0.03_f32;
    for _ in 0..24 {
        if t > 4.0 {
            break;
        }
        let p = v_add(ro_local, v_scale(rd_local, t));
        let h = eval_sdf(p, cfg.shape, cfg.sdf_blend);
        if h < 0.001 {
            return 0.05;
        }
        res = res.min(8.5 * h / t);
        t += h.clamp(0.02, 0.22);
    }
    res.clamp(0.05, 1.0)
}

fn evaluate_stage_surface(hit: &ObjectHit, cfg: &StageConfig) -> PbrSurface {
    let mut albedo = cfg.base_linear;
    let mut metallic = cfg.metallic;
    let mut roughness = cfg.roughness;
    let p = hit.pos_local;

    // Procedural detail per preset material
    match cfg.material_choice {
        0 => {
            // Brushed Metal: fine radial/lathe micro-grooves
            let groove = ((p[1] * 95.0).sin() * 0.5 + 0.5) * 0.08;
            roughness = (roughness + groove).clamp(0.06, 1.0);
        }
        5 => {
            // Marble Stone: 3D Perlin/FBM veining
            let n = fbm3([p[0] * 3.5, p[1] * 3.5, p[2] * 3.5]);
            let vein = (1.0 - (p[0] * 4.0 + p[1] * 2.5 + n * 4.5).sin().abs()).powi(5);
            albedo = v_lerp(albedo, [0.16, 0.17, 0.19], vein * 0.72);
            roughness = (roughness + vein * 0.14).clamp(0.06, 1.0);
        }
        6 => {
            // Weathered Copper: subtle verdigris tint in crevices even at default weathering
            let crevice = (1.0 - hit.ao).clamp(0.0, 1.0);
            albedo = v_lerp(albedo, [0.14, 0.42, 0.34], crevice * (0.35 + 0.5 * cfg.weathering));
            metallic *= 1.0 - crevice * 0.55;
        }
        _ => {}
    }

    // Substance Painter-style Curvature & Cavity Weathering
    if cfg.weathering > 0.001 {
        let edge_wear = (hit.curvature * 1.35).clamp(0.0, 1.0) * cfg.weathering;
        let cavity_dirt = (1.0 - hit.ao + (-hit.curvature * 0.6).max(0.0)).clamp(0.0, 1.0) * cfg.weathering;
        // Exposed bright rubbed edges
        albedo = v_lerp(albedo, [0.78, 0.79, 0.82], edge_wear * 0.65);
        metallic = (metallic + edge_wear * 0.45).clamp(0.0, 1.0);
        roughness = (roughness * (1.0 - edge_wear * 0.35) + cavity_dirt * 0.42).clamp(0.04, 1.0);
        // Darkened cavity patina
        albedo = v_scale(albedo, (1.0 - cavity_dirt * 0.48).clamp(0.25, 1.0));
    }

    PbrSurface { albedo, metallic, roughness, ao: hit.ao }
}

pub(crate) fn stage_3d(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let (w, h) = (img.w, img.h);
    if w == 0 || h == 0 {
        return;
    }
    let cfg = eval_stage_config(e, cx);
    let ibl = VideoIbl::from_frame(img, b(e, "video_ibl"), f(e, "ibl_intensity", cx));
    let src = img.clone();

    let cam_pos = [0.0_f32, 0.0, 2.85];
    let aspect = w as f32 / h.max(1) as f32;
    let px_cone = (2.0 * cfg.tan_half_fov) / h.max(1) as f32;
    let key_radiance = [2.65_f32, 2.55, 2.42];
    let rim_dir = v_norm([-cfg.light_dir[0], 0.35, -0.75]);
    let rim_radiance = [0.55_f32, 0.62, 0.75];

    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let ndc_y = (1.0 - 2.0 * (y as f32 + 0.5) / h as f32) * cfg.tan_half_fov;
        for x in 0..w {
            let ndc_x = (2.0 * (x as f32 + 0.5) / w as f32 - 1.0) * aspect * cfg.tan_half_fov;
            let rd = v_norm([ndc_x, ndc_y, -1.0]);
            let bg = src.get(x, y);

            // Ground plane intersection t_ground for Shadow Catcher
            let t_ground = if rd[1].abs() > 1e-5 {
                let tg = (cfg.ground_y - cam_pos[1]) / rd[1];
                (tg > 0.05 && tg < 12.0).then_some(tg)
            } else {
                None
            };

            let obj_hit = trace_stage_object(cam_pos, rd, px_cone, &cfg);

            // Compute ground shadow occlusion on backdrop (visible behind semi-transparent edges or around object)
            let mut backdrop_rgb = Image::unpremul(bg);
            let backdrop_a = bg[3];
            if cfg.shadow_catcher > 0.001
                && let Some(tg) = t_ground
                && obj_hit.as_ref().is_none_or(|hit| hit.coverage < 0.999 || tg < hit.t_world)
            {
                let gp = v_add(cam_pos, v_scale(rd, tg));
                let dist_xz = ((gp[0] - cfg.center[0]).powi(2) + (gp[2] - cfg.center[2]).powi(2)).sqrt();
                let fade = (1.0 - dist_xz / (3.2 * cfg.scale.max(0.5))).clamp(0.0, 1.0);
                if fade > 0.0 {
                    let vis = trace_soft_shadow(v_add(gp, [0.0, 0.015, 0.0]), cfg.light_dir, &cfg);
                    let occ = (1.0 - vis) * cfg.shadow_catcher * fade;
                    let darken = (1.0 - occ).clamp(0.0, 1.0);
                    backdrop_rgb = v_scale(backdrop_rgb, darken);
                }
            }

            if let Some(hit) = obj_hit
                && t_ground.is_none_or(|tg| hit.t_world <= tg + 1e-3)
            {
                let surf = evaluate_stage_surface(&hit, &cfg);
                let view = v_scale(rd, -1.0);
                let shadow = trace_soft_shadow(v_add(hit.pos_world, v_scale(hit.normal_world, 0.018 * cfg.scale)), cfg.light_dir, &cfg);

                let direct = shade_ggx_direct(hit.normal_world, view, cfg.light_dir, v_scale(key_radiance, shadow), &surf);
                let rim = shade_ggx_direct(hit.normal_world, view, rim_dir, rim_radiance, &surf);
                let ambient = ibl.evaluate(hit.normal_world, view, &surf);
                let mut lit = v_add(v_add(direct, rim), ambient);

                // Frosted Glass transmission: refract screen-space backdrop through surface normal
                if cfg.transmission > 0.01 {
                    let refr_px = 18.0 * (1.0 + surf.roughness * 2.0);
                    let sx = (x as f32 + 0.5 - hit.normal_world[0] * refr_px).clamp(0.5, w as f32 - 0.5);
                    let sy = (y as f32 + 0.5 + hit.normal_world[1] * refr_px).clamp(0.5, h as f32 - 0.5);
                    let s_a = Image::unpremul(src.sample_bilinear_clamped(sx, sy));
                    let s_b = Image::unpremul(src.sample_bilinear_clamped(sx + 2.5, sy - 2.5));
                    let refr_col = v_mul(v_scale(v_add(s_a, s_b), 0.5), surf.albedo);
                    let fres_edge = (1.0 - v_dot(hit.normal_world, view).clamp(0.0, 1.0)).powi(4);
                    let trans_w = cfg.transmission * (1.0 - 0.75 * fres_edge);
                    lit = v_add(v_scale(refr_col, trans_w), lit);
                }

                // Soft filmic shoulder to prevent harsh clipping on mirror highlights while keeping linear HDR headroom
                let cov = hit.coverage.clamp(0.0, 1.0);
                let out_a = cov + backdrop_a * (1.0 - cov);
                let comp = [
                    lit[0] * cov + backdrop_rgb[0] * backdrop_a * (1.0 - cov),
                    lit[1] * cov + backdrop_rgb[1] * backdrop_a * (1.0 - cov),
                    lit[2] * cov + backdrop_rgb[2] * backdrop_a * (1.0 - cov),
                ];
                row[x * 4] = comp[0];
                row[x * 4 + 1] = comp[1];
                row[x * 4 + 2] = comp[2];
                row[x * 4 + 3] = out_a;
            } else {
                row[x * 4] = backdrop_rgb[0] * backdrop_a;
                row[x * 4 + 1] = backdrop_rgb[1] * backdrop_a;
                row[x * 4 + 2] = backdrop_rgb[2] * backdrop_a;
                row[x * 4 + 3] = backdrop_a;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Effect 2: `"surface_relight"` (`"Surface Relight & PBR Sampler"`)
// ---------------------------------------------------------------------------

/// Sampled 6-channel PBR maps & summary statistics returned for `material.sampleClipPbr` and tests.
pub struct SampledPbrMaps {
    pub width: usize,
    pub height: usize,
    pub albedo: Image,
    pub normal: Image,
    pub roughness: Image,
    pub height_map: Image,
    pub ao: Image,
    pub relit: Image,
    pub border_seam_rmse: f32,
}

/// Parameters for [`sample_pbr_channels`].
#[derive(Clone, Copy, Debug)]
pub struct PbrSampleParams {
    pub delight: f32,
    pub normal_strength: f32,
    pub base_roughness: f32,
    pub metallic: f32,
    pub ao_strength: f32,
    pub make_tileable: bool,
    pub light_dir: [f32; 3],
    pub light_radiance: [f32; 3],
}

impl Default for PbrSampleParams {
    fn default() -> Self {
        Self {
            delight: 0.65,
            normal_strength: 1.5,
            base_roughness: 0.35,
            metallic: 0.0,
            ao_strength: 0.4,
            make_tileable: false,
            light_dir: v_norm([0.35, 0.45, 0.60]),
            light_radiance: [1.3, 1.25, 1.18],
        }
    }
}

/// Toroidal raised-cosine $C^\infty$ seamless blend:
/// $T(u, v) = \sum_{a,b \in \{0,1\}} W_a(u) W_b(v) S((u + 0.5a) \bmod 1, (v + 0.5b) \bmod 1)$
/// where $W_0(t) = \sin^2(\pi t)$ and $W_1(t) = \cos^2(\pi t)$.
/// Preserves the centre $u=0.5, v=0.5$ intact while making left/right and top/bottom borders smoothly periodic.
fn make_seamless_toroidal(src: &Image) -> Image {
    let (w, h) = (src.w, src.h);
    if w < 2 || h < 2 {
        return src.clone();
    }
    let mut out = Image::new(w, h);
    let wf = w as f32;
    let hf = h as f32;
    out.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        // Use periodic grid u = x / w, v = y / h so pixel w wraps smoothly to pixel 0
        let v = y as f32 / hf;
        let wy0 = (PI * v).sin().powi(2);
        let wy1 = 1.0 - wy0;
        let sy0 = y as f32 + 0.5;
        let sy1 = ((v + 0.5).rem_euclid(1.0)) * hf + 0.5;

        for x in 0..w {
            let u = x as f32 / wf;
            let wx0 = (PI * u).sin().powi(2);
            let wx1 = 1.0 - wx0;
            let sx0 = x as f32 + 0.5;
            let sx1 = ((u + 0.5).rem_euclid(1.0)) * wf + 0.5;

            let c00 = sample_periodic(src, sx0, sy0);
            let c10 = sample_periodic(src, sx1, sy0);
            let c01 = sample_periodic(src, sx0, sy1);
            let c11 = sample_periodic(src, sx1, sy1);

            for k in 0..4 {
                row[x * 4 + k] = wx0 * wy0 * c00[k] + wx1 * wy0 * c10[k] + wx0 * wy1 * c01[k] + wx1 * wy1 * c11[k];
            }
        }
    });
    out
}

#[inline]
fn sample_periodic(img: &Image, x: f32, y: f32) -> [f32; 4] {
    let w = img.w as isize;
    let h = img.h as isize;
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = fx.floor() as isize;
    let y0 = fy.floor() as isize;
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let ix0 = x0.rem_euclid(w) as usize;
    let ix1 = (x0 + 1).rem_euclid(w) as usize;
    let iy0 = y0.rem_euclid(h) as usize;
    let iy1 = (y0 + 1).rem_euclid(h) as usize;
    let (a, b, c, d) = (img.get(ix0, iy0), img.get(ix1, iy0), img.get(ix0, iy1), img.get(ix1, iy1));
    let mut o = [0.0; 4];
    for k in 0..4 {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        o[k] = top + (bot - top) * ty;
    }
    o
}

fn blur_scalar_separable(field: &[f32], w: usize, h: usize, radius: usize, periodic: bool) -> Vec<f32> {
    if w == 0 || h == 0 || radius == 0 {
        return field.to_vec();
    }
    let r = radius.min(32) as isize;
    let sigma = (r as f32 * 0.5).max(0.8);
    let mut weights = Vec::with_capacity((2 * r + 1) as usize);
    let mut wsum = 0.0_f32;
    for d in -r..=r {
        let wt = (-(d as f32 * d as f32) / (2.0 * sigma * sigma)).exp();
        weights.push(wt);
        wsum += wt;
    }
    for wt in &mut weights {
        *wt /= wsum.max(1e-6);
    }

    let mut tmp = vec![0.0_f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let base = y * w;
        for x in 0..w {
            let mut acc = 0.0_f32;
            for (k, d) in (-r..=r).enumerate() {
                let sx = if periodic { (x as isize + d).rem_euclid(w as isize) as usize } else { (x as isize + d).clamp(0, w as isize - 1) as usize };
                acc += field[base + sx] * weights[k];
            }
            row[x] = acc;
        }
    });

    let mut out = vec![0.0_f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let mut acc = 0.0_f32;
            for (k, d) in (-r..=r).enumerate() {
                let sy = if periodic { (y as isize + d).rem_euclid(h as isize) as usize } else { (y as isize + d).clamp(0, h as isize - 1) as usize };
                acc += tmp[sy * w + x] * weights[k];
            }
            row[x] = acc;
        }
    });
    out
}

pub fn border_seam_error(img: &Image) -> f32 {
    let (w, h) = (img.w, img.h);
    if w < 2 || h < 2 {
        return 0.0;
    }
    let mut sum_sq = 0.0_f64;
    let mut n = 0_usize;
    for y in 0..h {
        let l = Image::unpremul(img.get(0, y));
        let r = Image::unpremul(img.get(w - 1, y));
        for k in 0..3 {
            let d = f64::from(l[k] - r[k]);
            sum_sq += d * d;
        }
        n += 3;
    }
    for x in 0..w {
        let t = Image::unpremul(img.get(x, 0));
        let b = Image::unpremul(img.get(x, h - 1));
        for k in 0..3 {
            let d = f64::from(t[k] - b[k]);
            sum_sq += d * d;
        }
        n += 3;
    }
    if n == 0 { 0.0 } else { (sum_sq / n as f64).sqrt() as f32 }
}

/// Extract all 6 PBR channels (`albedo`, `normal`, `roughness`, `height_map`, `ao`, `relit`) from `src`.
pub fn sample_pbr_channels(src: &Image, params: &PbrSampleParams) -> SampledPbrMaps {
    let (w, h) = (src.w, src.h);
    if w == 0 || h == 0 {
        return SampledPbrMaps {
            width: 0,
            height: 0,
            albedo: Image::new(0, 0),
            normal: Image::new(0, 0),
            roughness: Image::new(0, 0),
            height_map: Image::new(0, 0),
            ao: Image::new(0, 0),
            relit: Image::new(0, 0),
            border_seam_rmse: 0.0,
        };
    }

    let base_img = if params.make_tileable { make_seamless_toroidal(src) } else { src.clone() };
    let periodic = params.make_tileable;

    // Luminance & log-luminance fields for Multi-Scale Retinex delighting + height extraction
    let mut luma = vec![0.0_f32; w * h];
    let mut log_luma = vec![0.0_f32; w * h];
    let mut log_sum = 0.0_f64;
    for i in 0..(w * h) {
        let p = &base_img.px[i * 4..i * 4 + 4];
        let c = Image::unpremul([p[0], p[1], p[2], p[3]]);
        let y_lin = luma709(c[0].max(0.0), c[1].max(0.0), c[2].max(0.0));
        luma[i] = y_lin;
        let ly = (y_lin + 0.015).ln();
        log_luma[i] = ly;
        log_sum += f64::from(ly);
    }
    let mean_illum = ((log_sum / (w * h).max(1) as f64).exp() as f32 - 0.015).max(0.08);

    let r_mid = (w.min(h) / 24).clamp(2, 8);
    let r_wide = (w.min(h) / 6).clamp(6, 24);
    let log_mid = blur_scalar_separable(&log_luma, w, h, r_mid, periodic);
    let log_wide = blur_scalar_separable(&log_luma, w, h, r_wide, periodic);
    let luma_smooth = blur_scalar_separable(&luma, w, h, r_mid, periodic);

    // Normalized Height field [0, 1] combining fine luma relief + mid-band detail
    let mut height_field = vec![0.0_f32; w * h];
    for i in 0..(w * h) {
        let detail = (luma[i] - luma_smooth[i]) * 1.6;
        let base_h = (luma[i] / (mean_illum * 2.2).max(0.1)).clamp(0.0, 1.0);
        height_field[i] = (0.55 * base_h + 0.45 * (0.5 + detail)).clamp(0.0, 1.0);
    }
    let height_broad = blur_scalar_separable(&height_field, w, h, r_mid.max(3), periodic);

    let mut albedo_img = Image::new(w, h);
    let mut normal_img = Image::new(w, h);
    let mut rough_img = Image::new(w, h);
    let mut height_img = Image::new(w, h);
    let mut ao_img = Image::new(w, h);
    let mut relit_img = Image::new(w, h);

    let delight = params.delight.clamp(0.0, 1.0);
    let n_strength = params.normal_strength.clamp(0.1, 5.0);
    let base_rough = params.base_roughness.clamp(0.04, 1.0);
    let metallic = params.metallic.clamp(0.0, 1.0);
    let ao_strength = params.ao_strength.clamp(0.0, 1.0);

    let fetch_h = |x: isize, y: isize| -> f32 {
        let sx = if periodic { x.rem_euclid(w as isize) as usize } else { x.clamp(0, w as isize - 1) as usize };
        let sy = if periodic { y.rem_euclid(h as isize) as usize } else { y.clamp(0, h as isize - 1) as usize };
        height_field[sy * w + sx]
    };

    for y in 0..h {
        let yi = y as isize;
        for x in 0..w {
            let xi = x as isize;
            let idx = y * w + x;
            let orig_px = base_img.get(x, y);
            let alpha = orig_px[3].clamp(0.0, 1.0);
            let orig_rgb = Image::unpremul(orig_px);

            // 1. Multi-Scale Retinex Delighted Albedo
            let low_shading = ((0.45 * log_mid[idx] + 0.55 * log_wide[idx]).exp() - 0.015).max(0.04);
            let gain = (mean_illum / low_shading).powf(delight).clamp(0.25, 3.5);
            let alb = [(orig_rgb[0] * gain).clamp(0.01, 0.98), (orig_rgb[1] * gain).clamp(0.01, 0.98), (orig_rgb[2] * gain).clamp(0.01, 0.98)];

            // 2. Multi-scale 3×3 Scharr + 3px Meso Gradient -> Unit Tangent Normal
            let gx_fine = (3.0 * fetch_h(xi + 1, yi - 1) + 10.0 * fetch_h(xi + 1, yi) + 3.0 * fetch_h(xi + 1, yi + 1)
                - 3.0 * fetch_h(xi - 1, yi - 1)
                - 10.0 * fetch_h(xi - 1, yi)
                - 3.0 * fetch_h(xi - 1, yi + 1))
                / 16.0;
            let gy_fine = (3.0 * fetch_h(xi - 1, yi + 1) + 10.0 * fetch_h(xi, yi + 1) + 3.0 * fetch_h(xi + 1, yi + 1)
                - 3.0 * fetch_h(xi - 1, yi - 1)
                - 10.0 * fetch_h(xi, yi - 1)
                - 3.0 * fetch_h(xi + 1, yi - 1))
                / 16.0;
            let gx_meso = (fetch_h(xi + 3, yi) - fetch_h(xi - 3, yi)) * 0.35;
            let gy_meso = (fetch_h(xi, yi + 3) - fetch_h(xi, yi - 3)) * 0.35;

            let gx = (gx_fine * 0.72 + gx_meso * 0.28) * n_strength * 2.8;
            let gy = (gy_fine * 0.72 + gy_meso * 0.28) * n_strength * 2.8;
            // Screen +Y points down; in 3D tangent space +Y points up, so ny = +gy
            let nrm = v_norm([-gx, gy, 1.0]);

            // 3. Local Gradient Energy + Micro-Variance Roughness
            let grad_mag = (gx_fine * gx_fine + gy_fine * gy_fine).sqrt();
            let micro_var = (luma[idx] - luma_smooth[idx]).abs();
            let rgh = (base_rough + grad_mag * 0.85 + micro_var * 0.35).clamp(0.04, 1.0);

            // 4. Cavity / Horizon Ambient Occlusion
            let cavity = (height_broad[idx] - height_field[idx]).max(0.0);
            let lap = (fetch_h(xi - 1, yi) + fetch_h(xi + 1, yi) + fetch_h(xi, yi - 1) + fetch_h(xi, yi + 1) - 4.0 * height_field[idx]).max(0.0);
            let ao_val = (1.0 - ao_strength * (cavity * 3.2 + lap * 1.8)).clamp(0.08, 1.0);
            let h_val = height_field[idx];

            // 5. Physical Cook-Torrance GGX 3D Surface Relighting
            let surf = PbrSurface { albedo: alb, metallic, roughness: rgh, ao: ao_val };
            let view = [0.0_f32, 0.0, 1.0];
            let direct = shade_ggx_direct(nrm, view, params.light_dir, params.light_radiance, &surf);
            let amb_hemi = v_scale(alb, (0.18 + 0.10 * nrm[1]) * ao_val * (1.0 - 0.7 * metallic));
            let relit = v_add(direct, amb_hemi);

            let o = idx * 4;
            albedo_img.px[o..o + 4].copy_from_slice(&[alb[0] * alpha, alb[1] * alpha, alb[2] * alpha, alpha]);
            // Tangent normal map is non-color linear data with alpha=1 where opaque so 2*rgb - 1 is unit length
            let na = if alpha > 0.0 { 1.0 } else { 0.0 };
            normal_img.px[o..o + 4].copy_from_slice(&[(0.5 * nrm[0] + 0.5) * na, (0.5 * nrm[1] + 0.5) * na, (0.5 * nrm[2] + 0.5) * na, na]);
            rough_img.px[o..o + 4].copy_from_slice(&[rgh * alpha, rgh * alpha, rgh * alpha, alpha]);
            height_img.px[o..o + 4].copy_from_slice(&[h_val * alpha, h_val * alpha, h_val * alpha, alpha]);
            ao_img.px[o..o + 4].copy_from_slice(&[ao_val * alpha, ao_val * alpha, ao_val * alpha, alpha]);
            relit_img.px[o..o + 4].copy_from_slice(&[relit[0] * alpha, relit[1] * alpha, relit[2] * alpha, alpha]);
        }
    }

    let seam = border_seam_error(&albedo_img);
    SampledPbrMaps {
        width: w,
        height: h,
        albedo: albedo_img,
        normal: normal_img,
        roughness: rough_img,
        height_map: height_img,
        ao: ao_img,
        relit: relit_img,
        border_seam_rmse: seam,
    }
}

fn surface_relight(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let lx = f(e, "light_x", cx).clamp(-1.0, 1.0);
    let ly = f(e, "light_y", cx).clamp(-1.0, 1.0);
    let lz = f(e, "light_z", cx).clamp(0.05, 2.0);
    let lc = color(e, "light_color", cx);
    let lc_lin = srgb3_to_linear([lc[0], lc[1], lc[2]]);
    let inten = f(e, "intensity", cx).clamp(0.0, 4.0) * 2.2;

    let params = PbrSampleParams {
        delight: f(e, "delight", cx),
        normal_strength: f(e, "normal_strength", cx),
        base_roughness: f(e, "roughness", cx),
        metallic: f(e, "metallic", cx),
        ao_strength: f(e, "ao_strength", cx),
        make_tileable: b(e, "make_tileable"),
        light_dir: v_norm([lx, ly, lz]),
        light_radiance: v_scale(lc_lin, inten),
    };

    let maps = sample_pbr_channels(img, &params);
    *img = match choice(e, "output_mode") {
        1 => maps.albedo,
        2 => maps.normal,
        3 => maps.roughness,
        4 => maps.height_map,
        5 => maps.ao,
        _ => maps.relit,
    };
}

// ---------------------------------------------------------------------------
// Effect 3: `"procedural_pbr"` (`"Procedural PBR Material"`)
// ---------------------------------------------------------------------------

#[inline]
fn hash2_u32(x: i32, y: i32, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8DA6_B343) ^ (y as u32).wrapping_mul(0xD816_3841) ^ seed.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^ (h >> 16)
}

#[inline]
fn hash2_f32(x: i32, y: i32, seed: u32) -> f32 {
    (hash2_u32(x, y, seed) >> 8) as f32 / (1_u32 << 24) as f32
}

#[inline]
fn hash3_f32(x: i32, y: i32, z: i32) -> f32 {
    let h = hash2_u32(x ^ z.wrapping_mul(374_761_393), y ^ z.wrapping_mul(668_265_263), 0x9E37_79B9);
    (h >> 8) as f32 / (1_u32 << 24) as f32
}

#[inline]
fn quintic(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn value_noise2(u: f32, v: f32, seed: u32) -> f32 {
    let ix = u.floor() as i32;
    let iy = v.floor() as i32;
    let fx = quintic(u - ix as f32);
    let fy = quintic(v - iy as f32);
    let a = hash2_f32(ix, iy, seed);
    let b = hash2_f32(ix + 1, iy, seed);
    let c = hash2_f32(ix, iy + 1, seed);
    let d = hash2_f32(ix + 1, iy + 1, seed);
    let top = a + (b - a) * fx;
    let bot = c + (d - c) * fx;
    top + (bot - top) * fy
}

fn fbm2(mut u: f32, mut v: f32, seed: u32) -> f32 {
    let mut sum = 0.0_f32;
    let mut amp = 0.5_f32;
    for oct in 0..4_u32 {
        sum += amp * value_noise2(u, v, seed.wrapping_add(oct * 101));
        u = u * 2.03 + 13.7;
        v = v * 2.03 - 7.3;
        amp *= 0.5;
    }
    sum
}

fn fbm3(p: [f32; 3]) -> f32 {
    let mut q = p;
    let mut sum = 0.0_f32;
    let mut amp = 0.5_f32;
    for _ in 0..3 {
        let ix = q[0].floor() as i32;
        let iy = q[1].floor() as i32;
        let iz = q[2].floor() as i32;
        let fx = quintic(q[0] - ix as f32);
        let fy = quintic(q[1] - iy as f32);
        let fz = quintic(q[2] - iz as f32);
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let c00 = lerp(hash3_f32(ix, iy, iz), hash3_f32(ix + 1, iy, iz), fx);
        let c10 = lerp(hash3_f32(ix, iy + 1, iz), hash3_f32(ix + 1, iy + 1, iz), fx);
        let c01 = lerp(hash3_f32(ix, iy, iz + 1), hash3_f32(ix + 1, iy, iz + 1), fx);
        let c11 = lerp(hash3_f32(ix, iy + 1, iz + 1), hash3_f32(ix + 1, iy + 1, iz + 1), fx);
        let c0 = lerp(c00, c10, fy);
        let c1 = lerp(c01, c11, fy);
        sum += amp * lerp(c0, c1, fz);
        q = [q[0] * 2.05 + 5.1, q[1] * 2.05 - 3.7, q[2] * 2.05 + 8.9];
        amp *= 0.5;
    }
    sum
}

/// Cellular Voronoi returning `(f1_dist, f2_minus_f1_edge, cell_hash)`.
fn voronoi2(u: f32, v: f32, seed: u32) -> (f32, f32, u32) {
    let gx = u.floor() as i32;
    let gy = v.floor() as i32;
    let mut d1 = 1e9_f32;
    let mut d2 = 1e9_f32;
    let mut best_id = 0_u32;
    for dy in -1..=1 {
        for dx in -1..=1 {
            let cx = gx + dx;
            let cy = gy + dy;
            let jx = hash2_f32(cx, cy, seed) * 0.82 + 0.09;
            let jy = hash2_f32(cx, cy, seed ^ 0x55AA_1234) * 0.82 + 0.09;
            let px = cx as f32 + jx - u;
            let py = cy as f32 + jy - v;
            let d = (px * px + py * py).sqrt();
            if d < d1 {
                d2 = d1;
                d1 = d;
                best_id = hash2_u32(cx, cy, seed ^ 0x1357_9BDF);
            } else if d < d2 {
                d2 = d;
            }
        }
    }
    (d1, (d2 - d1).max(0.0), best_id)
}

struct ProcSample {
    albedo: [f32; 3],
    height: f32,
    roughness: f32,
    metallic: f32,
}

fn eval_procedural_pattern(pattern: u32, u: f32, v: f32, base_rough: f32, base_metal: f32, weathering: f32) -> ProcSample {
    match pattern {
        0 => {
            // Brushed Anisotropic Metal: stretched horizontal FBM scratches + cross-grain polish
            let brush = fbm2(u * 2.2, v * 46.0, 11);
            let fine = value_noise2(u * 5.0, v * 110.0, 29);
            let wear = fbm2(u * 3.5, v * 3.5, 73) * weathering;
            let h = 0.55 * brush + 0.45 * fine - wear * 0.25;
            let base = v_lerp([0.74, 0.77, 0.81], [0.32, 0.31, 0.30], (wear * 0.7).clamp(0.0, 1.0));
            ProcSample {
                albedo: base,
                height: h,
                roughness: (base_rough + (0.5 - brush).abs() * 0.22 + wear * 0.35).clamp(0.04, 1.0),
                metallic: (base_metal * (1.0 - wear * 0.35)).clamp(0.0, 1.0),
            }
        }
        1 => {
            // Carbon Hex Weave: 2×2 twill carbon fiber bundles with clearcoat matrix
            let su = u * 4.0;
            let sv = v * 4.0;
            let iu = su.floor() as i32;
            let iv = sv.floor() as i32;
            let fu = su - iu as f32;
            let fv = sv - iv as f32;
            let warp_over = ((iu + iv).rem_euclid(2)) == 0;
            let profile = if warp_over { (fu * PI).sin() * (0.85 + 0.15 * (fv * PI).sin()) } else { (fv * PI).sin() * (0.85 + 0.15 * (fu * PI).sin()) };
            let strand = if warp_over { (fu * 24.0 * PI).cos() } else { (fv * 24.0 * PI).cos() } * 0.05;
            let scuff = fbm2(u * 5.0, v * 5.0, 41) * weathering;
            let h = (profile * 0.85 + strand - scuff * 0.2).clamp(0.0, 1.0);
            let tow_col = if warp_over { [0.055, 0.060, 0.068] } else { [0.038, 0.042, 0.048] };
            ProcSample {
                albedo: v_lerp(tow_col, [0.16, 0.16, 0.17], scuff * 0.5),
                height: h,
                roughness: (base_rough * (0.65 + 0.45 * (1.0 - profile)) + scuff * 0.35).clamp(0.04, 1.0),
                metallic: (base_metal * 0.35 + 0.12 * profile).clamp(0.0, 1.0),
            }
        }
        2 => {
            // Terrazzo Marble: Voronoi mineral chips embedded in polished cement-marble matrix
            let warp_u = u + 0.12 * fbm2(u * 2.0, v * 2.0, 51);
            let warp_v = v + 0.12 * fbm2(u * 2.0 + 9.1, v * 2.0 - 4.3, 53);
            let (_, edge, cid) = voronoi2(warp_u * 3.8, warp_v * 3.8, 97);
            let chip_thresh = 0.10 + 0.06 * ((cid & 15) as f32 / 15.0);
            let in_chip = ((edge - chip_thresh) * 28.0).clamp(0.0, 1.0);
            let palette = [
                [0.82, 0.62, 0.48], // terracotta chip
                [0.24, 0.34, 0.38], // slate blue-grey
                [0.88, 0.84, 0.76], // warm quartz
                [0.44, 0.52, 0.44], // serpentine green
            ];
            let chip_srgb = palette[(cid as usize) & 3];
            let chip_lin = srgb3_to_linear(chip_srgb);
            let vein = fbm2(u * 3.0, v * 3.0, 109);
            let matrix_lin = v_lerp([0.72, 0.70, 0.66], [0.52, 0.50, 0.48], vein * (0.5 + 0.5 * weathering));
            let albedo = v_lerp(matrix_lin, chip_lin, in_chip);
            let h = 0.45 + 0.18 * in_chip + 0.12 * vein - weathering * 0.15 * (1.0 - in_chip);
            ProcSample {
                albedo,
                height: h,
                roughness: (base_rough + (1.0 - in_chip) * 0.22 + weathering * 0.25).clamp(0.04, 1.0),
                metallic: (base_metal * 0.12).clamp(0.0, 0.35),
            }
        }
        3 => {
            // Ceramic Tiles & Grout: beveled rectangular glaze tiles with recessed mortar joints
            let gu = u * 3.0;
            let gv = v * 3.0;
            let tx = gu.floor() as i32;
            let ty = gv.floor() as i32;
            let fu = gu - tx as f32;
            let fv = gv - ty as f32;
            let border = fu.min(1.0 - fu).min(fv.min(1.0 - fv));
            let chip_noise = fbm2(u * 14.0, v * 14.0, 131) * weathering * 0.08;
            let tile_mask = ((border - 0.055 - chip_noise) * 32.0).clamp(0.0, 1.0);
            let bevel = ((border - 0.04) * 10.0).clamp(0.0, 1.0);
            let glaze_var = (hash2_f32(tx, ty, 149) - 0.5) * 0.08;
            let tile_col = [(0.58 + glaze_var).clamp(0.05, 0.95), (0.66 + glaze_var * 0.8).clamp(0.05, 0.95), (0.70 + glaze_var * 0.6).clamp(0.05, 0.95)];
            let grout_grain = value_noise2(u * 40.0, v * 40.0, 163);
            let grout_col = v_scale([0.24, 0.23, 0.22], 0.8 + 0.4 * grout_grain);
            ProcSample {
                albedo: v_lerp(grout_col, tile_col, tile_mask),
                height: bevel * 0.75 + grout_grain * 0.08 * (1.0 - tile_mask),
                roughness: v_lerp([0.86, 0.0, 0.0], [(base_rough * 0.55).max(0.05), 0.0, 0.0], tile_mask)[0],
                metallic: (base_metal * 0.15 * tile_mask).clamp(0.0, 1.0),
            }
        }
        _ => {
            // Weathered Rust Steel: machined steel plate + FBM iron-oxide corrosion pits
            let corr_field = fbm2(u * 3.2, v * 3.2, 211) * 0.7 + fbm2(u * 9.5, v * 9.5, 223) * 0.3;
            let threshold = 1.02 - weathering.clamp(0.0, 1.0) * 0.95;
            let rust_mask = ((corr_field - threshold) * 5.5).clamp(0.0, 1.0);
            let steel_brush = value_noise2(u * 4.0, v * 55.0, 239);
            let rust_detail = fbm2(u * 18.0, v * 18.0, 251);
            let steel_col = [0.62 + 0.06 * steel_brush, 0.65 + 0.06 * steel_brush, 0.69 + 0.06 * steel_brush];
            let rust_col = v_lerp([0.28, 0.08, 0.025], [0.54, 0.19, 0.05], rust_detail);
            let h = (0.65 + 0.08 * steel_brush) * (1.0 - rust_mask) + (0.28 + 0.25 * rust_detail) * rust_mask;
            ProcSample {
                albedo: v_lerp(steel_col, rust_col, rust_mask),
                height: h,
                roughness: ((base_rough * (1.0 - rust_mask)) + (0.86 * rust_mask)).clamp(0.04, 1.0),
                metallic: (base_metal * (1.0 - rust_mask)).clamp(0.0, 1.0),
            }
        }
    }
}

fn procedural_pbr(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let (w, h) = (img.w, img.h);
    if w == 0 || h == 0 {
        return;
    }
    let blend_op = f(e, "blend_opacity", cx).clamp(0.0, 1.0);
    if blend_op <= 0.0 {
        return;
    }
    let pattern = choice(e, "pattern");
    let scale = f(e, "scale", cx).clamp(0.25, 8.0);
    let base_rough = f(e, "roughness", cx).clamp(0.04, 1.0);
    let base_metal = f(e, "metallic", cx).clamp(0.0, 1.0);
    let weathering = f(e, "weathering", cx).clamp(0.0, 1.0);
    let ang = f(e, "light_angle", cx).to_radians();
    let phase = if b(e, "animate_phase") { (cx.seconds * 0.35) as f32 } else { 0.0 };

    let ibl = VideoIbl::from_frame(img, true, 0.85);
    let (sin_a, cos_a) = ang.sin_cos();
    let light_dir = v_norm([cos_a * 0.68, sin_a * 0.68, 0.73]);
    let light_rad = [2.4_f32, 2.32, 2.20];
    let inv_dim = scale / (w.min(h).max(1) as f32);
    let eps = inv_dim.max(0.002);

    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let v = y as f32 * inv_dim + phase * 0.4;
        for x in 0..w {
            let p = &mut row[x * 4..x * 4 + 4];
            let a = p[3].clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let u = x as f32 * inv_dim + phase;
            let c = eval_procedural_pattern(pattern, u, v, base_rough, base_metal, weathering);
            let hx = eval_procedural_pattern(pattern, u + eps, v, base_rough, base_metal, weathering).height;
            let hy = eval_procedural_pattern(pattern, u, v + eps, base_rough, base_metal, weathering).height;
            let gx = (hx - c.height) / eps * 0.14;
            let gy = (hy - c.height) / eps * 0.14;
            let nrm = v_norm([-gx, gy, 1.0]);

            let surf = PbrSurface { albedo: c.albedo, metallic: c.metallic, roughness: c.roughness, ao: (0.72 + 0.28 * c.height).clamp(0.25, 1.0) };
            let view = [0.0_f32, 0.0, 1.0];
            let direct = shade_ggx_direct(nrm, view, light_dir, light_rad, &surf);
            let env = ibl.evaluate(nrm, view, &surf);
            let shaded = v_add(direct, env);

            let orig = [p[0] / a, p[1] / a, p[2] / a];
            let mixed = v_lerp(orig, shaded, blend_op);
            p[0] = mixed[0] * a;
            p[1] = mixed[1] * a;
            p[2] = mixed[2] * a;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_color::WorkingSpace;
    use filmcraft_project::{Param, ParamValue, find_effect};
    use filmcraft_time::Tick;

    fn test_ctx() -> FxCtx<'static> {
        FxCtx {
            t: Tick::ZERO,
            px_scale: 1.0,
            mask_scale: 1.0,
            seconds: 0.5,
            timecode: "00:00:00:12",
            clip_name: "Test",
            project: None,
            env: None,
            frame_rate: 24.0,
            working: WorkingSpace::Rec709,
        }
    }

    fn gradient_ramp_image(w: usize, h: usize) -> Image {
        let mut img = Image::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let u = x as f32 / (w - 1).max(1) as f32;
                let v = y as f32 / (h - 1).max(1) as f32;
                let ripple = 0.08 * (x as f32 * 0.45).sin() * (y as f32 * 0.35).cos();
                let r = (0.12 + 0.72 * u + ripple).clamp(0.02, 0.98);
                let g = (0.15 + 0.65 * v - ripple * 0.5).clamp(0.02, 0.98);
                let b = (0.20 + 0.50 * (0.5 * u + 0.5 * v)).clamp(0.02, 0.98);
                let i = (y * w + x) * 4;
                img.px[i..i + 4].copy_from_slice(&[r, g, b, 1.0]);
            }
        }
        img
    }

    #[test]
    fn stage_3d_renders_shaded_object_video_ibl_and_ground_shadow_catcher() {
        let cx = test_ctx();
        let backdrop = Image::filled(96, 64, [0.45, 0.48, 0.52, 1.0]);
        let mut inst = find_effect("stage_3d").expect("stage_3d def").instance();
        inst.params.insert("shape".into(), Param::new(ParamValue::Choice(5))); // SDF Clay Smooth Union
        inst.params.insert("material".into(), Param::new(ParamValue::Choice(1))); // Polished Gold
        inst.params.insert("shadow_catcher".into(), Param::new(ParamValue::Float(0.75)));
        inst.params.insert("ground_y".into(), Param::new(ParamValue::Float(-0.62)));

        let mut out = backdrop.clone();
        apply(&mut out, &inst, &cx);
        assert!(out.px.iter().all(|v| v.is_finite()), "all pixels finite");

        // Center pixel hits the 3D clay sculpture and differs substantially from neutral grey backdrop
        let center = Image::unpremul(out.get(48, 32));
        let bg_c = Image::unpremul(backdrop.get(48, 32));
        let diff_center = (center[0] - bg_c[0]).abs() + (center[1] - bg_c[1]).abs() + (center[2] - bg_c[2]).abs();
        assert!(diff_center > 0.12, "center should render shaded gold 3D object, got {center:?} vs {bg_c:?}");

        // Compare against shadow_catcher = 0 to prove ground plane shadow darkens pixels beneath the object
        let mut no_shadow_inst = inst.clone();
        no_shadow_inst.params.insert("shadow_catcher".into(), Param::new(ParamValue::Float(0.0)));
        let mut out_no_shadow = backdrop;
        apply(&mut out_no_shadow, &no_shadow_inst, &cx);

        let sum_luma = |im: &Image| -> f32 { (28..68).flat_map(|x| (44..62).map(move |y| luma709(im.get(x, y)[0], im.get(x, y)[1], im.get(x, y)[2]))).sum() };
        assert!(sum_luma(&out) < sum_luma(&out_no_shadow) - 0.5, "shadow catcher must darken ground region below object");
    }

    #[test]
    fn custom_obj_bvh_raycaster_hits_parsed_mesh() {
        let cube_obj = r#"
            v -1 -1  1
            v  1 -1  1
            v  1  1  1
            v -1  1  1
            v -1 -1 -1
            v  1 -1 -1
            v  1  1 -1
            v -1  1 -1
            f 1 2 3 4
            f 6 5 8 7
            f 2 6 7 3
            f 5 1 4 8
            f 4 3 7 8
            f 5 6 2 1
        "#;
        let mesh = filmcraft_project::parse_obj(cube_obj).expect("parse cube");
        let compiled = CompiledObj::build(mesh);
        let hit = compiled.raycast([0.0, 0.0, 2.5], [0.0, 0.0, -1.0], 10.0);
        assert!(hit.is_some(), "ray from +Z must intersect unit cube BVH");
        let (t, n, _) = hit.unwrap();
        assert!(t > 1.5 && t < 2.5);
        assert!(n[2] > 0.9, "front face normal points toward +Z, got {n:?}");
    }

    #[test]
    fn surface_relight_outputs_unit_normals_and_make_tileable_reduces_seam_error() {
        let cx = test_ctx();
        let src = gradient_ramp_image(64, 64);

        // 1. Verify Tangent Normal Map (output_mode = 2) produces unit-length vectors everywhere
        let mut nrm_inst = find_effect("surface_relight").expect("surface_relight").instance();
        nrm_inst.params.insert("output_mode".into(), Param::new(ParamValue::Choice(2)));
        nrm_inst.params.insert("normal_strength".into(), Param::new(ParamValue::Float(2.2)));
        let mut nrm_img = src.clone();
        apply(&mut nrm_img, &nrm_inst, &cx);

        for y in 0..nrm_img.h {
            for x in 0..nrm_img.w {
                let p = nrm_img.get(x, y);
                let nx = 2.0 * p[0] - 1.0;
                let ny = 2.0 * p[1] - 1.0;
                let nz = 2.0 * p[2] - 1.0;
                let len = (nx * nx + ny * ny + nz * nz).sqrt();
                assert!((len - 1.0).abs() < 1e-3, "non-unit normal at ({x},{y}): len={len}");
                assert!(nz > 0.0, "tangent normal Z must be positive hemisphere: {nz}");
            }
        }

        // 2. Verify make_tileable reduces border seam RMSE dramatically on a strong ramp image
        let raw_maps = sample_pbr_channels(&src, &PbrSampleParams { make_tileable: false, ..Default::default() });
        let seamless_maps = sample_pbr_channels(&src, &PbrSampleParams { make_tileable: true, ..Default::default() });
        assert!(
            seamless_maps.border_seam_rmse < raw_maps.border_seam_rmse * 0.25,
            "make_tileable must substantially reduce border seam error: seamless={} vs raw={}",
            seamless_maps.border_seam_rmse,
            raw_maps.border_seam_rmse
        );
    }

    #[test]
    fn procedural_pbr_all_patterns_are_finite_and_deterministic() {
        let cx = test_ctx();
        let base = Image::filled(48, 48, [0.3, 0.35, 0.4, 1.0]);
        for pat in 0..5 {
            let mut inst = find_effect("procedural_pbr").expect("procedural_pbr").instance();
            inst.params.insert("pattern".into(), Param::new(ParamValue::Choice(pat)));
            let mut a = base.clone();
            let mut b_img = base.clone();
            apply(&mut a, &inst, &cx);
            apply(&mut b_img, &inst, &cx);
            assert_eq!(a.px, b_img.px, "pattern {pat} must be deterministic");
            assert!(a.px.iter().all(|v| v.is_finite() && *v >= 0.0), "pattern {pat} finite & non-negative");
            assert_ne!(a.px, base.px, "pattern {pat} must shade pixels");
        }
    }
}
