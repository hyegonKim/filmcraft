//! Color Grading Presets (Effects panel ▸ Color Grading Presets): built-in Color Grading looks, and their
//! thumbnails rendered by our own Color Grading on a generated preview picture.
//!
//! The presets are original work: parameter values chosen by FilmCraft contributors, named
//! descriptively (no third-party look names, LUTs or preview images). The preview picture
//! ([`preview_image`]) is procedural: a dusk sky with a sun, mountain ridges, a lake with its
//! reflection, a grey ramp down the left edge and six colour chips along the bottom, so a
//! thumbnail shows what a look does to sky, shadows, saturated colours and neutrals.

use filmcraft_color::srgb_to_linear;
use filmcraft_geom::Vec2;
use filmcraft_project::{EffectInstance, ParamValue, find_effect};
use filmcraft_time::Tick;

use crate::Image;
use crate::effects::{FxCtx, apply};

/// One preset: a Color Grading instance with these parameters (the rest at their defaults).
#[derive(Clone, Debug)]
pub struct LumetriPreset {
    /// Sub-folder of Color Grading Presets.
    pub folder: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<(&'static str, ParamValue)>,
}

impl LumetriPreset {
    /// A Color Grading effect instance configured as the preset.
    pub fn instance(&self) -> EffectInstance {
        let mut e = match find_effect("lumetri") {
            Some(def) => def.instance(),
            // A bare instance named after the effect; the preset's parameters are added below.
            None => EffectInstance {
                effect: "lumetri".into(),
                enabled: true,
                params: Default::default(),
                masks: Vec::new(),
                post_fader: false,
                essential: false,
                layer: None,
            },
        };
        for (k, v) in &self.params {
            if let Some(p) = e.params.get_mut(*k) {
                p.value = v.clone();
            }
        }
        e
    }
}

/// The preset folders in Effects panel order.
pub const FOLDERS: [&str; 7] = ["Cinematic", "Film Emulation", "Effect Styles", "Boost & Holdout", "Monochrome", "Vignette", "Technical"];

fn fl(v: f64) -> ParamValue {
    ParamValue::Float(v)
}
fn rgb(r: f32, g: f32, b: f32) -> ParamValue {
    ParamValue::Color([r, g, b, 1.0])
}
/// A colour-wheel offset towards hue `deg` (0 = red, 120 = green, 240 = blue) of length `len`.
fn wheel(deg: f64, len: f64) -> ParamValue {
    let a = deg.to_radians();
    ParamValue::Vec2(Vec2::new(a.cos() * len, a.sin() * len))
}
fn curve(points: &[[f32; 2]]) -> ParamValue {
    ParamValue::Curve(points.to_vec())
}

/// Every built-in preset, by folder.
pub fn presets() -> Vec<LumetriPreset> {
    let p = |folder, name, description, params| LumetriPreset { folder, name, description, params };
    vec![
        // ---- Cinematic
        p(
            "Cinematic",
            "Teal Shadows, Warm Skin",
            "Cyan-teal shadows and warm highlights with a little more contrast",
            vec![("shadow_tint", rgb(0.36, 0.52, 0.6)), ("highlight_tint", rgb(0.64, 0.53, 0.42)), ("contrast", fl(20.0)), ("creative_sat", fl(110.0))],
        ),
        p(
            "Cinematic",
            "Night Exterior",
            "Day for night: darker, cooler and less saturated",
            vec![("temperature", fl(-45.0)), ("exposure", fl(-0.9)), ("contrast", fl(15.0)), ("creative_sat", fl(70.0)), ("shadow_tint", rgb(0.4, 0.45, 0.62))],
        ),
        p(
            "Cinematic",
            "Golden Dusk",
            "Warm, soft late-afternoon light",
            vec![("temperature", fl(35.0)), ("tint", fl(8.0)), ("highlights", fl(-15.0)), ("vibrance", fl(20.0)), ("highlight_tint", rgb(0.66, 0.55, 0.4))],
        ),
        p("Cinematic", "Steel Blue", "Cool, desaturated and punchy", vec![("temperature", fl(-25.0)), ("saturation", fl(80.0)), ("contrast", fl(25.0))]),
        p(
            "Cinematic",
            "Hard Sun",
            "High contrast with held highlights and deep shadows",
            vec![("contrast", fl(45.0)), ("shadows", fl(-20.0)), ("highlights", fl(-25.0)), ("vibrance", fl(15.0))],
        ),
        p(
            "Cinematic",
            "Cinema Split Tone",
            "Deep cyan-indigo shadow separation with amber highlight roll-off",
            vec![
                ("shadow_tint", rgb(0.30, 0.46, 0.64)),
                ("highlight_tint", rgb(0.68, 0.56, 0.38)),
                ("contrast", fl(24.0)),
                ("highlights", fl(-18.0)),
                ("vibrance", fl(15.0)),
            ],
        ),
        p(
            "Cinematic",
            "Cinematic Cool Low",
            "Subtle cool theatrical balance with gentle contrast",
            vec![("temperature", fl(-16.0)), ("contrast", fl(12.0)), ("shadow_tint", rgb(0.44, 0.49, 0.57))],
        ),
        p(
            "Cinematic",
            "Cinematic Cool Med",
            "Medium cool theatrical grade with crisp midtones",
            vec![("temperature", fl(-28.0)), ("contrast", fl(22.0)), ("highlights", fl(-12.0)), ("shadow_tint", rgb(0.39, 0.47, 0.60))],
        ),
        p(
            "Cinematic",
            "Cinematic Cool High",
            "Strong cool theatrical look with deep slate shadows",
            vec![
                ("temperature", fl(-42.0)),
                ("contrast", fl(34.0)),
                ("highlights", fl(-20.0)),
                ("creative_sat", fl(88.0)),
                ("shadow_tint", rgb(0.34, 0.44, 0.64)),
            ],
        ),
        p(
            "Cinematic",
            "Cinematic Vivid Low",
            "Lightly punchy cinema contrast and vibrance",
            vec![("contrast", fl(14.0)), ("vibrance", fl(22.0)), ("creative_sat", fl(108.0))],
        ),
        p(
            "Cinematic",
            "Cinematic Vivid Med",
            "Rich theatrical saturation with protected highlights",
            vec![("contrast", fl(26.0)), ("highlights", fl(-15.0)), ("vibrance", fl(36.0)), ("creative_sat", fl(118.0))],
        ),
        p(
            "Cinematic",
            "Cinematic Vivid High",
            "Bold high-impact cinema colour with gamut protection",
            vec![
                ("contrast", fl(38.0)),
                ("highlights", fl(-22.0)),
                ("vibrance", fl(50.0)),
                ("creative_sat", fl(130.0)),
                ("gamut_compression", ParamValue::Choice(1)),
            ],
        ),
        p(
            "Cinematic",
            "Cinematic Warm Low",
            "Gentle golden warmth with soft shadow lift",
            vec![("temperature", fl(16.0)), ("tint", fl(4.0)), ("contrast", fl(12.0)), ("highlight_tint", rgb(0.58, 0.52, 0.44))],
        ),
        p(
            "Cinematic",
            "Cinematic Warm Med",
            "Balanced warm cinema grade with rich skin and amber highlights",
            vec![("temperature", fl(28.0)), ("tint", fl(7.0)), ("contrast", fl(22.0)), ("highlights", fl(-14.0)), ("highlight_tint", rgb(0.63, 0.54, 0.40))],
        ),
        p(
            "Cinematic",
            "Cinematic Warm High",
            "Intense tungsten-gold cinema grade with deep contrast",
            vec![
                ("temperature", fl(44.0)),
                ("tint", fl(10.0)),
                ("contrast", fl(34.0)),
                ("highlights", fl(-20.0)),
                ("vibrance", fl(18.0)),
                ("highlight_tint", rgb(0.68, 0.55, 0.36)),
            ],
        ),
        // ---- Film Emulation
        p(
            "Film Emulation",
            "Faded Print",
            "Lifted blacks, softer contrast, muted colour",
            vec![("faded_film", fl(45.0)), ("contrast", fl(-10.0)), ("creative_sat", fl(85.0))],
        ),
        p(
            "Film Emulation",
            "Silver Retention",
            "Bleach-bypass style: desaturated with hard contrast",
            vec![("look", ParamValue::Choice(4)), ("look_intensity", fl(80.0))],
        ),
        p(
            "Film Emulation",
            "Cross Processed",
            "Yellow-green highlights and blue-lifted shadows from crossed channel curves",
            vec![
                ("curve_red", curve(&[[0.0, 0.0], [0.5, 0.56], [1.0, 1.0]])),
                ("curve_green", curve(&[[0.0, 0.02], [0.5, 0.54], [1.0, 0.98]])),
                ("curve_blue", curve(&[[0.0, 0.12], [1.0, 0.82]])),
            ],
        ),
        p("Film Emulation", "Warm Negative", "Warm print stock with gentle roll-off", vec![("look", ParamValue::Choice(2)), ("temperature", fl(10.0))]),
        p(
            "Film Emulation",
            "Colorful",
            "Reversal stock look with lively primaries and smooth highlight shoulder",
            vec![("vibrance", fl(38.0)), ("creative_sat", fl(120.0)), ("contrast", fl(18.0)), ("highlights", fl(-18.0)), ("faded_film", fl(8.0))],
        ),
        p(
            "Film Emulation",
            "Contemporary",
            "Modern vision stock balance with clean neutrals and subtle split tone",
            vec![
                ("temperature", fl(8.0)),
                ("contrast", fl(16.0)),
                ("highlights", fl(-20.0)),
                ("shadows", fl(12.0)),
                ("shadow_tint", rgb(0.45, 0.50, 0.55)),
                ("highlight_tint", rgb(0.55, 0.52, 0.46)),
            ],
        ),
        p(
            "Film Emulation",
            "High Contrast",
            "Print stock with crisp toe, dense blacks and punchy midtones",
            vec![("contrast", fl(52.0)), ("blacks", fl(-20.0)), ("highlights", fl(-22.0)), ("faded_film", fl(6.0)), ("creative_sat", fl(106.0))],
        ),
        // ---- Effect Styles
        p(
            "Effect Styles",
            "Muted",
            "restrained chroma with lifted shadows and soft highlight roll-off",
            vec![("saturation", fl(64.0)), ("faded_film", fl(24.0)), ("contrast", fl(-12.0)), ("highlights", fl(-16.0))],
        ),
        p(
            "Effect Styles",
            "Office",
            "Fluorescent cool-green interior cast with crisp contrast",
            vec![("temperature", fl(-18.0)), ("tint", fl(-22.0)), ("contrast", fl(18.0)), ("creative_sat", fl(88.0))],
        ),
        p(
            "Effect Styles",
            "Sunscorched",
            "Overexposed desert heat with bleached highlights and amber push",
            vec![("exposure", fl(0.45)), ("temperature", fl(38.0)), ("contrast", fl(28.0)), ("whites", fl(18.0)), ("faded_film", fl(14.0))],
        ),
        p(
            "Effect Styles",
            "Sunset",
            "Magenta-gold horizon glow with deep warm shadows",
            vec![
                ("temperature", fl(40.0)),
                ("tint", fl(18.0)),
                ("highlights", fl(-16.0)),
                ("vibrance", fl(26.0)),
                ("shadow_tint", rgb(0.56, 0.42, 0.54)),
                ("highlight_tint", rgb(0.68, 0.52, 0.36)),
            ],
        ),
        p(
            "Effect Styles",
            "Vintage Faded",
            "Aged print matte blacks, warm creamy highlights and softened detail",
            vec![
                ("faded_film", fl(52.0)),
                ("temperature", fl(20.0)),
                ("creative_sat", fl(80.0)),
                ("sharpen", fl(-12.0)),
                ("highlight_tint", rgb(0.60, 0.56, 0.44)),
            ],
        ),
        // ---- Boost & Holdout
        p(
            "Boost & Holdout",
            "Boost Blues",
            "Emphasises sky and cyan-blue hues while keeping neutrals clean",
            vec![("hue_vs_sat", curve(&[[0.0, 0.5], [0.46, 0.52], [0.62, 0.92], [0.76, 0.52], [1.0, 0.5]])), ("vibrance", fl(16.0))],
        ),
        p(
            "Boost & Holdout",
            "Boost Green",
            "Enriches foliage and emerald hues across midtones",
            vec![("hue_vs_sat", curve(&[[0.0, 0.5], [0.20, 0.52], [0.34, 0.95], [0.48, 0.52], [1.0, 0.5]])), ("vibrance", fl(22.0))],
        ),
        p(
            "Boost & Holdout",
            "Boost Pink",
            "Lifts magenta, rose and neon pink accents",
            vec![("hue_vs_sat", curve(&[[0.0, 0.55], [0.72, 0.50], [0.87, 0.95], [0.97, 0.72], [1.0, 0.55]])), ("vibrance", fl(22.0)), ("tint", fl(8.0))],
        ),
        p(
            "Boost & Holdout",
            "Boost Red",
            "Deepens crimson and scarlet hues without clipping skin",
            vec![("hue_vs_sat", curve(&[[0.0, 0.94], [0.06, 0.65], [0.15, 0.50], [0.90, 0.52], [0.97, 0.90], [1.0, 0.94]])), ("vibrance", fl(18.0))],
        ),
        p(
            "Boost & Holdout",
            "Boost Skin",
            "Warms and enriches natural skin-tone orange-amber hues",
            vec![("hue_vs_sat", curve(&[[0.0, 0.56], [0.07, 0.86], [0.16, 0.54], [1.0, 0.56]])), ("temperature", fl(12.0)), ("vibrance", fl(16.0))],
        ),
        p(
            "Boost & Holdout",
            "Boost Yellow",
            "Intensifies warm gold and sunlit yellow tones",
            vec![("hue_vs_sat", curve(&[[0.0, 0.50], [0.08, 0.56], [0.16, 0.94], [0.26, 0.52], [1.0, 0.50]])), ("vibrance", fl(18.0))],
        ),
        p(
            "Boost & Holdout",
            "Holdout Blue",
            "Isolates cyan-blue hues while desaturating the rest of the frame",
            vec![("hue_vs_sat", curve(&[[0.0, 0.04], [0.46, 0.06], [0.56, 0.72], [0.64, 0.78], [0.72, 0.06], [1.0, 0.04]]))],
        ),
        p(
            "Boost & Holdout",
            "Holdout Green",
            "Isolates green hues while pulling all other colours to neutral",
            vec![("hue_vs_sat", curve(&[[0.0, 0.04], [0.20, 0.06], [0.30, 0.76], [0.38, 0.76], [0.46, 0.06], [1.0, 0.04]]))],
        ),
        p(
            "Boost & Holdout",
            "Holdout Red",
            "Isolates red hues while desaturating warm/cool surroundings",
            vec![("hue_vs_sat", curve(&[[0.0, 0.82], [0.05, 0.72], [0.11, 0.05], [0.90, 0.05], [0.96, 0.76], [1.0, 0.82]]))],
        ),
        // ---- Monochrome
        p("Monochrome", "Neutral Mono", "Black and white", vec![("saturation", fl(0.0))]),
        p(
            "Monochrome",
            "Hard Mono",
            "Black and white with crushed blacks and strong contrast",
            vec![("saturation", fl(0.0)), ("contrast", fl(60.0)), ("blacks", fl(-25.0))],
        ),
        p(
            "Monochrome",
            "Sepia Tone",
            "Monochrome toned warm brown",
            vec![("look", ParamValue::Choice(6)), ("wheel_midtones", wheel(30.0, 0.35)), ("wheel_highlights", wheel(45.0, 0.2))],
        ),
        p("Monochrome", "Cold Mono", "Monochrome toned cyan-blue", vec![("look", ParamValue::Choice(6)), ("wheel_midtones", wheel(210.0, 0.3))]),
        p(
            "Monochrome",
            "Mono Contrasty",
            "Punchy silver halide monochrome with crisp S-curve and deep blacks",
            vec![("saturation", fl(0.0)), ("contrast", fl(48.0)), ("blacks", fl(-18.0)), ("whites", fl(14.0))],
        ),
        p(
            "Monochrome",
            "Mono Sepia",
            "Classic archival sepia print with soft faded blacks",
            vec![("look", ParamValue::Choice(6)), ("faded_film", fl(18.0)), ("wheel_midtones", wheel(34.0, 0.42)), ("wheel_shadows", wheel(26.0, 0.18))],
        ),
        p(
            "Monochrome",
            "Mono Steel",
            "Metallic blue-grey monochrome with crisp contrast",
            vec![("look", ParamValue::Choice(6)), ("contrast", fl(28.0)), ("wheel_midtones", wheel(215.0, 0.34)), ("wheel_shadows", wheel(225.0, 0.22))],
        ),
        // ---- Vignette
        p(
            "Vignette",
            "Vignette Medium",
            "Balanced optical edge falloff drawing focus to the centre",
            vec![("vignette_amount", fl(-2.0)), ("vignette_midpoint", fl(46.0)), ("vignette_feather", fl(62.0))],
        ),
        p(
            "Vignette",
            "Vignette Heavy",
            "Deep dramatic corner darkening with tight midpoint",
            vec![("vignette_amount", fl(-3.8)), ("vignette_midpoint", fl(34.0)), ("vignette_feather", fl(55.0))],
        ),
        p(
            "Vignette",
            "Vignette Aberration",
            "Optical corner vignette with radial red-blue chromatic fringing",
            vec![("vignette_amount", fl(-2.2)), ("vignette_midpoint", fl(42.0)), ("vignette_feather", fl(58.0)), ("vignette_aberration", fl(70.0))],
        ),
        // ---- Technical
        p("Technical", "Lift Shadows", "Opens up the shadows", vec![("shadows", fl(40.0)), ("blacks", fl(10.0))]),
        p("Technical", "Protect Highlights", "Pulls highlights and whites down", vec![("highlights", fl(-40.0)), ("whites", fl(-15.0))]),
        p("Technical", "Half Saturation", "Saturation at 50 %", vec![("saturation", fl(50.0))]),
        p("Technical", "Boost Vibrance", "More colour in the less saturated areas", vec![("vibrance", fl(40.0))]),
        p(
            "Technical",
            "Legal Range Squeeze",
            "Maps full-range levels into 16–235 (video range) with a luma curve",
            vec![("curve_luma", curve(&[[0.0, 16.0 / 255.0], [1.0, 235.0 / 255.0]]))],
        ),
    ]
}

/// The preset called `name` (case-insensitive, supporting `"Film Emulation <Name>"` aliases).
pub fn find(name: &str) -> Option<LumetriPreset> {
    let trimmed = name.trim();
    let stripped = trimmed.strip_prefix("Film Emulation ").or_else(|| trimmed.strip_prefix("film emulation "));
    presets()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(trimmed) || stripped.is_some_and(|s| p.folder == "Film Emulation" && p.name.eq_ignore_ascii_case(s)))
}

/// The procedural preview picture (`w`×`h`, opaque, linear premultiplied like every [`Image`]).
pub fn preview_image(w: usize, h: usize) -> Image {
    let mut img = Image::new(w, h);
    let (wf, hf) = (w as f32, h as f32);
    let horizon = 0.56;
    let ramp_w = (wf * 0.05).max(2.0);
    let chips: [[f32; 3]; 6] = [[0.85, 0.12, 0.1], [0.95, 0.8, 0.1], [0.15, 0.7, 0.2], [0.1, 0.75, 0.8], [0.15, 0.25, 0.85], [0.8, 0.2, 0.75]];
    let chip_h = (hf * 0.12).max(2.0);
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
    // mountain ridge heights (fraction of the height above the horizon), two layers
    let ridge = |u: f32, k: f32| 0.12 * k * (0.55 + 0.25 * (u * 7.3 + k).sin() + 0.15 * (u * 17.1 + 2.0 * k).sin() + 0.05 * (u * 41.0).sin());
    let sky = |v: f32| mix([0.98, 0.62, 0.32], [0.16, 0.3, 0.62], (1.0 - v / horizon).clamp(0.0, 1.0).powf(0.8));
    let (sun_x, sun_y, sun_r) = (0.68f32, 0.33f32, 0.07f32);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / wf, (y as f32 + 0.5) / hf);
            let mut c = if v < horizon {
                let mut c = sky(v);
                // the sun and its glow
                let d = (((u - sun_x) * wf / hf).powi(2) + (v - sun_y).powi(2)).sqrt();
                if d < sun_r {
                    c = [1.0, 0.93, 0.7];
                } else {
                    let g = (1.0 - (d - sun_r) / 0.25).clamp(0.0, 1.0).powi(2) * 0.35;
                    c = mix(c, [1.0, 0.85, 0.55], g);
                }
                // far ridge (blue-grey), near ridge (dark green-brown)
                if horizon - v < ridge(u, 1.6) {
                    c = mix([0.36, 0.4, 0.52], c, 0.2);
                }
                if horizon - v < ridge(u, 0.9) {
                    c = mix([0.16, 0.2, 0.12], [0.3, 0.28, 0.2], ((horizon - v) / 0.12).clamp(0.0, 1.0));
                }
                c
            } else {
                // the lake: a darker, bluer reflection of the sky, rippled
                let mirror = horizon - (v - horizon);
                let ripple = 0.04 * ((v * 140.0).sin() * (u * 9.0 + v * 30.0).cos());
                let r = sky(mirror.max(0.0));
                mix(mix(r, [0.05, 0.12, 0.22], 0.45 + (v - horizon) * 0.8), [0.0; 3], 0.1 + ripple)
            };
            // grey ramp down the left edge
            if (x as f32) < ramp_w {
                let g = 1.0 - v;
                c = [g, g, g];
            }
            // colour chips along the bottom
            if (y as f32) >= hf - chip_h && (x as f32) >= ramp_w {
                let k = (((x as f32 - ramp_w) / (wf - ramp_w)) * 6.0).floor().clamp(0.0, 5.0) as usize;
                c = chips[k];
            }
            let i = (y * w + x) * 4;
            img.px[i] = srgb_to_linear(c[0].clamp(0.0, 1.0));
            img.px[i + 1] = srgb_to_linear(c[1].clamp(0.0, 1.0));
            img.px[i + 2] = srgb_to_linear(c[2].clamp(0.0, 1.0));
            img.px[i + 3] = 1.0;
        }
    }
    img
}

/// A preset's thumbnail: the preview picture graded by the preset (Rec. 709).
pub fn thumbnail(preset: &LumetriPreset, w: usize, h: usize) -> Image {
    let mut img = preview_image(w, h);
    let cx = FxCtx {
        t: Tick::ZERO,
        px_scale: w as f32 / 1920.0,
        mask_scale: w as f32 / 1920.0,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: None,
        env: None,
        frame_rate: 24.0,
        working: filmcraft_color::WorkingSpace::Rec709,
    };
    apply(&mut img, &preset.instance(), &cx);
    img
}

/// A grid of thumbnails (`cols` columns of `w`×`h` cells, 4 px gaps over a dark background), in
/// the order given: the Effects panel's thumbnail view of a Color Grading Presets folder.
pub fn grid(presets: &[LumetriPreset], cols: usize, w: usize, h: usize) -> Image {
    const GAP: usize = 4;
    let cols = cols.max(1);
    let rows = presets.len().div_ceil(cols).max(1);
    let (gw, gh) = (cols * (w + GAP) + GAP, rows * (h + GAP) + GAP);
    let bg = srgb_to_linear(0.11);
    let mut out = Image::filled(gw, gh, [bg, bg, bg, 1.0]);
    for (k, p) in presets.iter().enumerate() {
        let t = thumbnail(p, w, h);
        let (x0, y0) = (GAP + (k % cols) * (w + GAP), GAP + (k / cols) * (h + GAP));
        for y in 0..h {
            // a buffer that could not be allocated comes back empty: skip it rather than panic
            let d = ((y0 + y) * gw + x0) * 4;
            if let (Some(src), Some(dst)) = (t.px.get(y * w * 4..(y + 1) * w * 4), out.px.get_mut(d..d + w * 4)) {
                dst.copy_from_slice(src);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_and_change_the_picture() {
        let all = presets();
        assert!(all.len() >= 16);
        let mut names: Vec<&str> = all.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "unique names");
        let def = find_effect("lumetri").unwrap();
        let base = preview_image(64, 36);
        for p in &all {
            assert!(FOLDERS.contains(&p.folder), "{}", p.name);
            for (k, _) in &p.params {
                assert!(def.param(k).is_some(), "{}: unknown param {k}", p.name);
            }
            let t = thumbnail(p, 64, 36);
            let diff: f32 = t.px.iter().zip(&base.px).map(|(a, b)| (a - b).abs()).sum::<f32>() / base.px.len() as f32;
            assert!(diff > 0.004, "{} barely changes the picture ({diff})", p.name);
            assert!(t.px.iter().all(|v| v.is_finite()));
        }
        // Monochrome presets are monochrome (Sepia / Cold are toned: low but not zero chroma)
        let t = thumbnail(&find("Neutral Mono").unwrap(), 64, 36);
        assert!(t.px.chunks(4).all(|p| (p[0] - p[1]).abs() < 1e-3 && (p[1] - p[2]).abs() < 1e-3));
        assert!(find("neutral mono").is_some() && find("nope").is_none());
    }

    #[test]
    fn preview_picture_has_ramp_chips_and_sky() {
        let img = preview_image(320, 180);
        let px = |x: usize, y: usize| img.get(x, y);
        assert!(px(4, 2)[0] > 0.9 && px(4, 177)[0] < 0.05, "grey ramp: white at the top, black at the bottom");
        let red = px(60, 176);
        assert!(red[0] > 0.5 && red[1] < 0.1, "a red chip: {red:?}");
        let sky = px(160, 10);
        assert!(sky[2] > sky[0], "blue sky at the top: {sky:?}");
        let grid = grid(&presets()[..5], 3, 32, 18);
        assert_eq!((grid.w, grid.h), (3 * 36 + 4, 2 * 22 + 4));
    }
}
