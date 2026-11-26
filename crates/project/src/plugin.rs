//! Extensibility model: OpenFX / VST3 / OGraf / Workflow plugin descriptors, embedded project
//! plugins, PowerGrade gallery stills, and a clean-room safe DCTL (DaVinci Color Transform
//! Language) parser and evaluator.
//!
//! Every parser and evaluator is total and bounded: untrusted `.dctl` text cannot panic, recurse
//! past [`MAX_PARSE_DEPTH`] or [`MAX_CALL_DEPTH`], call undeclared functions, or run past
//! [`MAX_EVAL_STEPS`] per pixel.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::effect::EffectInstance;

/// Largest DCTL or plugin manifest source accepted into a project (1 MiB).
pub const MAX_PLUGIN_TEXT_BYTES: usize = 1 << 20;
/// Maximum expression/block nesting depth accepted by the DCTL parser.
pub const MAX_PARSE_DEPTH: usize = 32;
/// Maximum nested `__DEVICE__` helper function call depth during evaluation.
pub const MAX_CALL_DEPTH: usize = 8;
/// Maximum AST evaluation steps allowed per pixel so hostile code never stalls rendering.
pub const MAX_EVAL_STEPS: usize = 512;

fn default_true() -> bool {
    true
}

/// Ecosystem family of a discovered or built-in plugin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginKind {
    #[default]
    OpenFxVideo,
    OpenFxTransition,
    DctlTransform,
    DctlTransition,
    AudioVst3,
    OgrafTitle,
    MacroBundle,
    WorkflowScript,
}

impl PluginKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenFxVideo => "OpenFX Video",
            Self::OpenFxTransition => "OpenFX Transition",
            Self::DctlTransform => "DCTL Transform",
            Self::DctlTransition => "DCTL Transition",
            Self::AudioVst3 => "Fairlight / VST3 Audio",
            Self::OgrafTitle => "OGraf Broadcast Title",
            Self::MacroBundle => "Macro / Template Bundle",
            Self::WorkflowScript => "Workflow Integration",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::OpenFxVideo | Self::OpenFxTransition => "OFX",
            Self::DctlTransform | Self::DctlTransition => "DCTL",
            Self::AudioVst3 => "VST3",
            Self::OgrafTitle => "OGraf",
            Self::MacroBundle => "DRFX",
            Self::WorkflowScript => "WFI",
        }
    }
}

/// UI control kind declared by `DEFINE_UI_PARAMS(...)` in DaVinci Resolve DCTL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DctlParamKind {
    Float { default: f32, min: f32, max: f32, step: f32 },
    Int { default: i32, min: i32, max: i32, step: i32 },
    Bool { default: bool },
    Combo { default: usize, ids: Vec<String>, labels: Vec<String> },
    Color { default: [f32; 3] },
}

/// One parameter extracted from a DCTL `DEFINE_UI_PARAMS` directive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DctlUiParam {
    pub name: String,
    pub label: String,
    pub kind: DctlParamKind,
}

/// A portable plugin/shader asset stored inside [`crate::Project::plugins`], analogous to
/// [`crate::ProjectLut`]. Projects referencing `plugin:<id>` render identically on every host and
/// on WebAssembly without external files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPlugin {
    pub id: String,
    pub name: String,
    pub kind: PluginKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<DctlUiParam>,
    pub code: Arc<str>,
}

/// A Gallery Still / PowerGrade captured on the Color page (`color.grabStill` / `color.applyStill`):
/// stores a reusable serial stack of grading and ResolveFX/plugin instances.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerGradeStill {
    pub id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub timecode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    pub effects: Vec<EffectInstance>,
}

// ---------------------------------------------------------------------------
// Clean-room safe DCTL parser & evaluator
// ---------------------------------------------------------------------------

/// Value held in a DCTL variable or expression result (`float`, `float2`, `float3`, `float4`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DctlVal {
    Scalar(f32),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    Vec4([f32; 4]),
}

impl DctlVal {
    pub fn as_f32(self) -> f32 {
        let v = match self {
            Self::Scalar(s) => s,
            Self::Vec2(v) => v[0],
            Self::Vec3(v) => v[0],
            Self::Vec4(v) => v[0],
        };
        if v.is_finite() { v } else { 0.0 }
    }

    pub fn into_rgb(self, fallback: [f32; 3]) -> [f32; 3] {
        let clean = |x: f32| if x.is_finite() { x.clamp(-64.0, 64.0) } else { 0.0 };
        match self {
            Self::Scalar(s) => [clean(s); 3],
            Self::Vec2(v) => [clean(v[0]), clean(v[1]), clean(fallback[2])],
            Self::Vec3(v) => [clean(v[0]), clean(v[1]), clean(v[2])],
            Self::Vec4(v) => [clean(v[0]), clean(v[1]), clean(v[2])],
        }
    }

    pub fn into_rgba(self, fallback_a: f32) -> [f32; 4] {
        let clean = |x: f32| if x.is_finite() { x.clamp(-64.0, 64.0) } else { 0.0 };
        match self {
            Self::Scalar(s) => [clean(s), clean(s), clean(s), fallback_a],
            Self::Vec2(v) => [clean(v[0]), clean(v[1]), 0.0, fallback_a],
            Self::Vec3(v) => [clean(v[0]), clean(v[1]), clean(v[2]), fallback_a],
            Self::Vec4(v) => [clean(v[0]), clean(v[1]), clean(v[2]), clean(v[3]).clamp(0.0, 1.0)],
        }
    }

    fn map_unary(self, f: impl Fn(f32) -> f32) -> Self {
        let c = |x: f32| {
            let y = f(x);
            if y.is_finite() { y } else { 0.0 }
        };
        match self {
            Self::Scalar(s) => Self::Scalar(c(s)),
            Self::Vec2(v) => Self::Vec2([c(v[0]), c(v[1])]),
            Self::Vec3(v) => Self::Vec3([c(v[0]), c(v[1]), c(v[2])]),
            Self::Vec4(v) => Self::Vec4([c(v[0]), c(v[1]), c(v[2]), c(v[3])]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Num(f32),
    Var(String),
    Component(Box<Expr>, usize),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Assign { name: String, comp: Option<usize>, expr: Expr },
    If { cond: Expr, then_body: Vec<Stmt>, else_body: Vec<Stmt> },
    Return(Expr),
}

/// User-defined `__DEVICE__` helper function inside a DCTL file.
#[derive(Clone, Debug, PartialEq)]
pub struct DctlHelperFn {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// Parsed, validated DCTL shader program ready for parallel per-pixel evaluation.
#[derive(Clone, Debug, PartialEq)]
pub struct DctlProgram {
    pub is_transition: bool,
    pub returns_alpha: bool,
    pub straight_alpha: bool,
    pub uses_tex2d: bool,
    pub ui_params: Vec<DctlUiParam>,
    pub enum_consts: BTreeMap<String, f32>,
    pub helpers: BTreeMap<String, DctlHelperFn>,
    pub luts: BTreeMap<String, DctlCubeLut>,
    pub declared_luts: BTreeMap<String, (String, DctlLutInterp)>,
    pub body: Vec<Stmt>,
}

/// Interpolation mode used when evaluating `APPLY_LUT(...)` on a [`DctlCubeLut`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DctlLutInterp {
    /// DaVinci Resolve's 6-tetrahedra simplex decomposition of the unit cube.
    #[default]
    Tetrahedral,
    /// Standard 8-corner trilinear interpolation.
    Trilinear,
}

/// Clean-room `.cube` 1D/3D LUT parsed for `DEFINE_CUBE_LUT` / `DEFINE_LUT` and `APPLY_LUT`.
#[derive(Clone, Debug, PartialEq)]
pub struct DctlCubeLut {
    pub title: String,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    pub lut_1d: Vec<[f32; 3]>,
    pub lut_3d_size: usize,
    pub lut_3d: Vec<[f32; 3]>,
    pub interp: DctlLutInterp,
}

impl DctlCubeLut {
    /// Identity 3D cube of edge length `size` (clamped to `2..=65`).
    pub fn identity(size: usize) -> Self {
        let n = size.clamp(2, 65);
        let denom = (n - 1) as f32;
        let mut lut_3d = Vec::with_capacity(n * n * n);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    lut_3d.push([r as f32 / denom, g as f32 / denom, b as f32 / denom]);
                }
            }
        }
        Self {
            title: "Identity".into(),
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            lut_1d: Vec::new(),
            lut_3d_size: n,
            lut_3d,
            interp: DctlLutInterp::Tetrahedral,
        }
    }

    /// Clean-room 3×3×3 film-shaper fallback used when a shader declares an external `DEFINE_LUT`
    /// without attaching a LUT table, so pure DCTL evaluation never panics.
    pub fn default_film_shaper() -> Self {
        let n = 3usize;
        let denom = (n - 1) as f32;
        let shape = |x: f32| {
            let t = x.clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t) * 0.85 + t * 0.15
        };
        let mut lut_3d = Vec::with_capacity(n * n * n);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    lut_3d.push([shape(r as f32 / denom), shape(g as f32 / denom), shape(b as f32 / denom)]);
                }
            }
        }
        Self {
            title: "Default Film Shaper".into(),
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            lut_1d: Vec::new(),
            lut_3d_size: n,
            lut_3d,
            interp: DctlLutInterp::Tetrahedral,
        }
    }

    /// Parse a `.cube` specification (`TITLE`, `LUT_1D_SIZE`, `LUT_3D_SIZE`, `DOMAIN_MIN`, `DOMAIN_MAX`).
    pub fn parse_cube(text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut title = String::new();
        let mut size_1d = 0usize;
        let mut size_3d = 0usize;
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];
        let mut rows: Vec<[f32; 3]> = Vec::new();

        for (line_idx, raw) in text.lines().enumerate() {
            let trimmed_raw = raw.trim();
            if trimmed_raw.is_empty() || trimmed_raw.starts_with('#') {
                continue;
            }
            if let Some(rest) = trimmed_raw.strip_prefix("TITLE")
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                let t = rest.trim();
                title = if let Some(inner) = t.strip_prefix('"').and_then(|q| q.split_once('"')) {
                    inner.0.to_string()
                } else {
                    t.split('#').next().unwrap_or("").trim().trim_matches('"').to_string()
                };
                continue;
            }
            let line = trimmed_raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut words = line.split_whitespace();
            let first = words.next().unwrap_or("");
            if first.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                if !rows.is_empty() {
                    return Err(format!("cube line {}: header keyword `{first}` after table rows", line_idx + 1));
                }
                match first {
                    "LUT_1D_SIZE" => {
                        let v: usize = words.next().and_then(|s| s.parse().ok()).ok_or_else(|| format!("cube line {}: invalid LUT_1D_SIZE", line_idx + 1))?;
                        if !(2..=65536).contains(&v) {
                            return Err(format!("cube line {}: LUT_1D_SIZE {v} out of bounds (2..=65536)", line_idx + 1));
                        }
                        size_1d = v;
                    }
                    "LUT_3D_SIZE" => {
                        let v: usize = words.next().and_then(|s| s.parse().ok()).ok_or_else(|| format!("cube line {}: invalid LUT_3D_SIZE", line_idx + 1))?;
                        if !(2..=129).contains(&v) {
                            return Err(format!("cube line {}: LUT_3D_SIZE {v} out of bounds (2..=129)", line_idx + 1));
                        }
                        size_3d = v;
                    }
                    "DOMAIN_MIN" | "DOMAIN_MAX" => {
                        let vals: Vec<f32> = words.filter_map(|s| s.parse::<f32>().ok().filter(|x| x.is_finite())).collect();
                        if vals.len() != 3 {
                            return Err(format!("cube line {}: `{first}` expects 3 finite floats", line_idx + 1));
                        }
                        let slot = if first == "DOMAIN_MIN" { &mut domain_min } else { &mut domain_max };
                        slot.copy_from_slice(&vals);
                    }
                    "LUT_1D_INPUT_RANGE" | "LUT_3D_INPUT_RANGE" => {
                        let vals: Vec<f32> = words.filter_map(|s| s.parse::<f32>().ok().filter(|x| x.is_finite())).collect();
                        if vals.len() != 2 || vals[1] <= vals[0] {
                            return Err(format!("cube line {}: `{first}` expects min < max", line_idx + 1));
                        }
                        domain_min = [vals[0]; 3];
                        domain_max = [vals[1]; 3];
                    }
                    _ => {}
                }
                continue;
            }
            let r = first.parse::<f32>().ok().filter(|x| x.is_finite());
            let g = words.next().and_then(|s| s.parse::<f32>().ok().filter(|x| x.is_finite()));
            let b = words.next().and_then(|s| s.parse::<f32>().ok().filter(|x| x.is_finite()));
            let (Some(r), Some(g), Some(b)) = (r, g, b) else {
                return Err(format!("cube line {}: expected 3 finite RGB floats", line_idx + 1));
            };
            rows.push([r, g, b]);
        }

        if size_1d == 0 && size_3d == 0 {
            return Err("cube LUT missing LUT_1D_SIZE or LUT_3D_SIZE".into());
        }
        for c in 0..3 {
            if domain_max[c] <= domain_min[c] {
                return Err("cube DOMAIN_MAX must exceed DOMAIN_MIN".into());
            }
        }
        let expected = size_1d.saturating_add(size_3d.saturating_mul(size_3d).saturating_mul(size_3d));
        if rows.len() != expected {
            return Err(format!("cube expected {expected} table rows, found {}", rows.len()));
        }
        let lut_1d = if size_1d > 0 { rows.get(..size_1d).unwrap_or(&[]).to_vec() } else { Vec::new() };
        let lut_3d = if size_3d > 0 { rows.get(size_1d..).unwrap_or(&[]).to_vec() } else { Vec::new() };
        Ok(Self { title, domain_min, domain_max, lut_1d, lut_3d_size: size_3d, lut_3d, interp: DctlLutInterp::Tetrahedral })
    }

    #[inline]
    fn norm_1d(&self, v: f32, ch: usize) -> f32 {
        let span = (self.domain_max[ch] - self.domain_min[ch]).max(1e-6);
        ((v - self.domain_min[ch]) / span).clamp(0.0, 1.0)
    }

    fn apply_1d(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.lut_1d.len() < 2 {
            return rgb;
        }
        let last = self.lut_1d.len() - 1;
        let mut out = [0.0f32; 3];
        for c in 0..3 {
            let pos = self.norm_1d(rgb[c], c) * last as f32;
            let i0 = (pos.floor() as usize).min(last - 1);
            let frac = pos - i0 as f32;
            let v0 = self.lut_1d.get(i0).map_or(rgb[c], |row| row[c]);
            let v1 = self.lut_1d.get(i0 + 1).map_or(v0, |row| row[c]);
            out[c] = v0 + (v1 - v0) * frac;
        }
        out
    }

    #[inline]
    fn corner(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        let n = self.lut_3d_size;
        let idx = r.saturating_add(g.saturating_mul(n)).saturating_add(b.saturating_mul(n).saturating_mul(n));
        self.lut_3d.get(idx).copied().unwrap_or([0.0; 3])
    }

    #[inline]
    fn cube_coords(&self, rgb: [f32; 3]) -> ((usize, usize, usize), (f32, f32, f32)) {
        let n = self.lut_3d_size;
        let scale = (n.saturating_sub(1)) as f32;
        let use_dom = self.lut_1d.is_empty();
        let map_c = |v: f32, ch: usize| {
            let u = if use_dom { self.norm_1d(v, ch) } else { v.clamp(0.0, 1.0) };
            u * scale
        };
        let (x, y, z) = (map_c(rgb[0], 0), map_c(rgb[1], 1), map_c(rgb[2], 2));
        let max_base = n.saturating_sub(2);
        let (r0, g0, b0) = ((x.floor() as usize).min(max_base), (y.floor() as usize).min(max_base), (z.floor() as usize).min(max_base));
        ((r0, g0, b0), (x - r0 as f32, y - g0 as f32, z - b0 as f32))
    }

    /// Evaluate using DaVinci Resolve's 6-tetrahedra simplex decomposition of the unit cube.
    pub fn eval_tetrahedral(&self, rgb: [f32; 3]) -> [f32; 3] {
        let shaped = self.apply_1d(rgb);
        if self.lut_3d_size < 2 || self.lut_3d.is_empty() {
            return shaped;
        }
        let ((r0, g0, b0), (fr, fg, fb)) = self.cube_coords(shaped);
        let c = |dr: usize, dg: usize, db: usize| self.corner(r0 + dr, g0 + dg, b0 + db);
        let c000 = c(0, 0, 0);
        let c111 = c(1, 1, 1);
        let (ca, cb, w) = if fr >= fg {
            if fg >= fb {
                (c(1, 0, 0), c(1, 1, 0), [1.0 - fr, fr - fg, fg - fb, fb])
            } else if fr >= fb {
                (c(1, 0, 0), c(1, 0, 1), [1.0 - fr, fr - fb, fb - fg, fg])
            } else {
                (c(0, 0, 1), c(1, 0, 1), [1.0 - fb, fb - fr, fr - fg, fg])
            }
        } else if fb > fg {
            (c(0, 0, 1), c(0, 1, 1), [1.0 - fb, fb - fg, fg - fr, fr])
        } else if fb > fr {
            (c(0, 1, 0), c(0, 1, 1), [1.0 - fg, fg - fb, fb - fr, fr])
        } else {
            (c(0, 1, 0), c(1, 1, 0), [1.0 - fg, fg - fr, fr - fb, fb])
        };
        [
            w[0] * c000[0] + w[1] * ca[0] + w[2] * cb[0] + w[3] * c111[0],
            w[0] * c000[1] + w[1] * ca[1] + w[2] * cb[1] + w[3] * c111[1],
            w[0] * c000[2] + w[1] * ca[2] + w[2] * cb[2] + w[3] * c111[2],
        ]
    }

    /// Evaluate using standard 8-corner trilinear interpolation.
    pub fn eval_trilinear(&self, rgb: [f32; 3]) -> [f32; 3] {
        let shaped = self.apply_1d(rgb);
        if self.lut_3d_size < 2 || self.lut_3d.is_empty() {
            return shaped;
        }
        let ((r0, g0, b0), (fr, fg, fb)) = self.cube_coords(shaped);
        let mut out = [0.0f32; 3];
        for ch in 0..3 {
            let c = |dr: usize, dg: usize, db: usize| self.corner(r0 + dr, g0 + dg, b0 + db)[ch];
            let x00 = c(0, 0, 0) + (c(1, 0, 0) - c(0, 0, 0)) * fr;
            let x10 = c(0, 1, 0) + (c(1, 1, 0) - c(0, 1, 0)) * fr;
            let x01 = c(0, 0, 1) + (c(1, 0, 1) - c(0, 0, 1)) * fr;
            let x11 = c(0, 1, 1) + (c(1, 1, 1) - c(0, 1, 1)) * fr;
            let y0 = x00 + (x10 - x00) * fg;
            let y1 = x01 + (x11 - x01) * fg;
            out[ch] = y0 + (y1 - y0) * fb;
        }
        out
    }

    /// Evaluate using [`Self::interp`].
    pub fn eval(&self, rgb: [f32; 3]) -> [f32; 3] {
        match self.interp {
            DctlLutInterp::Tetrahedral => self.eval_tetrahedral(rgb),
            DctlLutInterp::Trilinear => self.eval_trilinear(rgb),
        }
    }
}

/// Optional spatial texture sampler `(x, y) -> [r, g, b, a]` in DCTL working space.
pub type DctlTexSampler<'a> = &'a (dyn Fn(isize, isize) -> [f32; 4] + Send + Sync);

/// Per-pixel inputs passed to [`DctlProgram::eval_pixel`].
#[derive(Clone, Copy)]
pub struct DctlPixelCtx<'a> {
    pub width: usize,
    pub height: usize,
    pub x: usize,
    pub y: usize,
    pub rgba: [f32; 4],
    pub to_rgba: [f32; 4],
    pub frame_index: i64,
    pub transition_progress: f32,
    pub params: &'a [(&'a str, f32)],
    pub luts: &'a [(&'a str, &'a DctlCubeLut)],
    pub sample_from: Option<DctlTexSampler<'a>>,
    pub sample_to: Option<DctlTexSampler<'a>>,
}

impl<'a> DctlPixelCtx<'a> {
    pub fn simple(width: usize, height: usize, x: usize, y: usize, rgba: [f32; 4], params: &'a [(&'a str, f32)]) -> Self {
        Self { width, height, x, y, rgba, to_rgba: rgba, frame_index: 1, transition_progress: 0.0, params, luts: &[], sample_from: None, sample_to: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(f32),
    Sym(char),
    EqEq,
    BangEq,
    LtEq,
    GtEq,
    AndAnd,
    OrOr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
}

/// Strip `//` and `/* ... */` comments while preserving newlines for directive scanning.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if let Some(ch) = src.get(i..).and_then(|s| s.chars().next()) {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn split_top_level(s: &str, delim: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth = (depth - 1).max(0),
            c if c == delim && depth == 0 => {
                parts.push(s.get(start..idx).unwrap_or("").trim());
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(s.get(start..).unwrap_or("").trim());
    parts
}

fn parse_brace_list(s: &str) -> Vec<String> {
    let trimmed = s.trim().trim_start_matches('{').trim_end_matches('}');
    trimmed.split(',').map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect()
}

fn parse_f32(s: &str, def: f32) -> f32 {
    s.trim().trim_end_matches('f').trim_end_matches('F').parse::<f32>().ok().filter(|v| v.is_finite()).unwrap_or(def)
}

fn parse_ui_directive(args: &str, enum_consts: &mut BTreeMap<String, f32>) -> Option<DctlUiParam> {
    let cols = split_top_level(args, ',');
    if cols.len() < 4 {
        return None;
    }
    let name = cols.first()?.to_string();
    let label = cols.get(1)?.to_string();
    let kind_str = *cols.get(2)?;
    let kind = match kind_str {
        "DCTLUI_SLIDER_FLOAT" | "DCTLUI_VALUE_BOX" => {
            let default = cols.get(3).map_or(0.0, |v| parse_f32(v, 0.0));
            let min = cols.get(4).map_or(-10.0, |v| parse_f32(v, -10.0));
            let max = cols.get(5).map_or(10.0, |v| parse_f32(v, 10.0)).max(min);
            let step = cols.get(6).map_or(0.01, |v| parse_f32(v, 0.01)).max(0.0001);
            DctlParamKind::Float { default, min, max, step }
        }
        "DCTLUI_SLIDER_INT" => {
            let default = cols.get(3).map_or(0, |v| parse_f32(v, 0.0) as i32);
            let min = cols.get(4).map_or(0, |v| parse_f32(v, 0.0) as i32);
            let max = cols.get(5).map_or(100, |v| parse_f32(v, 100.0) as i32).max(min);
            let step = cols.get(6).map_or(1, |v| (parse_f32(v, 1.0) as i32).max(1));
            DctlParamKind::Int { default, min, max, step }
        }
        "DCTLUI_CHECK_BOX" => {
            let default = cols.get(3).is_some_and(|v| parse_f32(v, 0.0) != 0.0);
            DctlParamKind::Bool { default }
        }
        "DCTLUI_COMBO_BOX" => {
            let default = cols.get(3).map_or(0usize, |v| parse_f32(v, 0.0).max(0.0) as usize);
            let ids = cols.get(4).map_or_else(Vec::new, |v| parse_brace_list(v));
            let labels = cols.get(5).map_or_else(|| ids.clone(), |v| parse_brace_list(v));
            for (idx, id) in ids.iter().enumerate() {
                enum_consts.insert(id.clone(), idx as f32);
            }
            DctlParamKind::Combo { default, ids, labels }
        }
        "DCTLUI_COLOR_PICKER" => {
            let r = cols.get(3).map_or(1.0, |v| parse_f32(v, 1.0));
            let g = cols.get(4).map_or(1.0, |v| parse_f32(v, 1.0));
            let b = cols.get(5).map_or(1.0, |v| parse_f32(v, 1.0));
            DctlParamKind::Color { default: [r, g, b] }
        }
        _ => return None,
    };
    Some(DctlUiParam { name, label, kind })
}

struct ExtractedSource {
    ui_params: Vec<DctlUiParam>,
    enum_consts: BTreeMap<String, f32>,
    declared_luts: BTreeMap<String, (String, DctlLutInterp)>,
    straight_alpha: bool,
    rest: String,
}

/// Extract inline `DEFINE_CUBE_LUT(ident) { ... }` blocks before comment/directive stripping so
/// `.cube` `#` comments and `TITLE` lines inside `{ ... }` are preserved intact.
fn extract_inline_cube_luts(src: &str) -> Result<(String, BTreeMap<String, DctlCubeLut>, BTreeMap<String, (String, DctlLutInterp)>), String> {
    let mut out = String::with_capacity(src.len());
    let mut luts = BTreeMap::new();
    let mut declared = BTreeMap::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push_str(src.get(start..i).unwrap_or(""));
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push_str(src.get(start..i).unwrap_or(""));
            continue;
        }
        if let Some(tail) = src.get(i..)
            && let Some(after_kw) = tail.strip_prefix("DEFINE_CUBE_LUT")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
        {
            let trimmed = after_kw.trim_start();
            if trimmed.starts_with('(')
                && let Some(close_paren) = skip_balanced(trimmed, 0, '(', ')')
            {
                let args_str = trimmed.get(1..close_paren - 1).unwrap_or("").trim();
                let cols = split_top_level(args_str, ',');
                let ident = cols.first().copied().unwrap_or("").trim();
                let interp = if cols.iter().any(|c| c.trim().eq_ignore_ascii_case("TRILINEAR") || c.trim().eq_ignore_ascii_case("DCTL_LUT_TRILINEAR")) {
                    DctlLutInterp::Trilinear
                } else {
                    DctlLutInterp::Tetrahedral
                };
                let post_paren = trimmed.get(close_paren..).unwrap_or("");
                let ws_len = post_paren.len() - post_paren.trim_start().len();
                let after_ws = post_paren.get(ws_len..).unwrap_or("");
                if after_ws.starts_with('{') {
                    let close_brace = skip_balanced(after_ws, 0, '{', '}').ok_or_else(|| format!("unclosed `{{` in DEFINE_CUBE_LUT({ident})"))?;
                    let cube_body = after_ws.get(1..close_brace - 1).unwrap_or("");
                    let mut parsed = DctlCubeLut::parse_cube(cube_body)?;
                    parsed.interp = interp;
                    if !ident.is_empty() {
                        luts.insert(ident.to_string(), parsed);
                        declared.insert(ident.to_string(), ("inline".into(), interp));
                    }
                    let consumed = "DEFINE_CUBE_LUT".len() + (after_kw.len() - trimmed.len()) + close_paren + ws_len + close_brace;
                    i += consumed;
                    let after_semi = src.get(i..).unwrap_or("").trim_start_matches([' ', '\t']);
                    if after_semi.starts_with(';') {
                        i += (src.len() - i - after_semi.len()) + 1;
                    }
                    out.push('\n');
                    continue;
                }
            }
        }
        if let Some(ch) = src.get(i..).and_then(|s| s.chars().next()) {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    Ok((out, luts, declared))
}

fn parse_lut_directive(args: &str, default_tag: &str) -> Option<(String, String, DctlLutInterp)> {
    let cols = split_top_level(args, ',');
    let ident = cols.first()?.trim();
    if ident.is_empty() {
        return None;
    }
    let tag = cols.get(1).map_or(default_tag, |s| s.trim().trim_matches('"'));
    let interp = if cols.iter().skip(1).any(|c| {
        let u = c.trim().trim_matches('"');
        u.eq_ignore_ascii_case("TRILINEAR") || u.eq_ignore_ascii_case("DCTL_LUT_TRILINEAR")
    }) {
        DctlLutInterp::Trilinear
    } else {
        DctlLutInterp::Tetrahedral
    };
    Some((ident.to_string(), tag.to_string(), interp))
}

fn extract_directives(cleaned: &str, mut declared_luts: BTreeMap<String, (String, DctlLutInterp)>) -> ExtractedSource {
    let mut ui_params = Vec::new();
    let mut enum_consts = BTreeMap::new();
    let mut straight_alpha = false;
    let mut rest = String::with_capacity(cleaned.len());
    for line in cleaned.lines() {
        let t = line.trim();
        if let Some(after) = t.strip_prefix("DEFINE_UI_PARAMS") {
            let inner = after.trim().trim_end_matches(';').trim();
            if let Some(stripped) = inner.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
                && let Some(param) = parse_ui_directive(stripped, &mut enum_consts)
            {
                ui_params.push(param);
            }
            continue;
        }
        if let Some(after) = t.strip_prefix("DEFINE_CUBE_LUT") {
            let inner = after.trim().trim_end_matches(';').trim();
            if let Some(stripped) = inner.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
                && let Some((ident, tag, interp)) = parse_lut_directive(stripped, "cube")
            {
                declared_luts.entry(ident).or_insert((tag, interp));
            }
            continue;
        }
        if let Some(after) = t.strip_prefix("DEFINE_LUT") {
            let inner = after.trim().trim_end_matches(';').trim();
            if let Some(stripped) = inner.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
                && let Some((ident, tag, interp)) = parse_lut_directive(stripped, "external")
            {
                declared_luts.insert(ident, (tag, interp));
            }
            continue;
        }
        if t.starts_with("DEFINE_DCTL_ALPHA_MODE_STRAIGHT") {
            straight_alpha = true;
            continue;
        }
        if t.starts_with("DEFINE_DCTL_ALPHA_MODE_PREMULTIPLY")
            || t.starts_with("DEFINE_UI_TOOLTIP")
            || t.starts_with("DEFINE_ACES_PARAM")
            || t.starts_with('#')
        {
            continue;
        }
        rest.push_str(line);
        rest.push('\n');
    }
    ExtractedSource { ui_params, enum_consts, declared_luts, straight_alpha, rest }
}

struct RawFnDef {
    ret_type: String,
    name: String,
    params: Vec<String>,
    body_src: String,
}

/// Extract all `__DEVICE__ <ret> <name>(<params>) { <body> }` functions in the DCTL source.
fn extract_device_functions(src: &str) -> Result<Vec<RawFnDef>, String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = src.get(cursor..).and_then(|s| s.find("__DEVICE__")) {
        let start = cursor + rel + "__DEVICE__".len();
        let tail = src.get(start..).ok_or_else(|| "invalid `__DEVICE__` offset".to_string())?;
        let Some(open_paren) = tail.find('(') else { break };
        let header = tail.get(..open_paren).unwrap_or("").trim();
        // Skip forward declarations ending in `;` before `{`
        let after_sig = skip_balanced(tail, open_paren, '(', ')').ok_or_else(|| "unclosed `(` in DCTL function signature".to_string())?;
        let post_sig = tail.get(after_sig..).unwrap_or("").trim_start();
        if post_sig.starts_with(';') {
            cursor = start + after_sig + 1;
            continue;
        }
        let brace_rel = tail.get(after_sig..).and_then(|s| s.find('{')).ok_or_else(|| "missing `{` for DCTL function body".to_string())?;
        let brace_open = after_sig + brace_rel;
        let brace_close = skip_balanced(tail, brace_open, '{', '}').ok_or_else(|| "unclosed `{` in DCTL function body".to_string())?;

        let mut hdr_words: Vec<&str> = header.split_whitespace().filter(|w| !matches!(*w, "__INLINE__" | "inline" | "static")).collect();
        let name = hdr_words.pop().unwrap_or("").trim().to_string();
        let ret_type = hdr_words.pop().unwrap_or("float3").trim().to_string();

        let sig_inner = tail.get(open_paren + 1..after_sig - 1).unwrap_or("");
        let mut params = Vec::new();
        for arg in split_top_level(sig_inner, ',') {
            if let Some(pname) = arg.split_whitespace().last() {
                let clean_name = pname.trim_start_matches('*').trim_start_matches('&').trim();
                if !clean_name.is_empty() && clean_name != "void" {
                    params.push(clean_name.to_string());
                }
            }
        }
        let body_src = tail.get(brace_open + 1..brace_close - 1).unwrap_or("").to_string();
        out.push(RawFnDef { ret_type, name, params, body_src });
        cursor = start + brace_close;
    }
    Ok(out)
}

fn skip_balanced(s: &str, open_idx: usize, open_ch: char, close_ch: char) -> Option<usize> {
    let mut depth = 0i32;
    for (off, ch) in s.get(open_idx..)?.char_indices() {
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(open_idx + off + ch.len_utf8());
            }
        }
    }
    None
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            toks.push(Tok::Ident(src.get(start..i).unwrap_or("").to_string()));
            continue;
        }
        if b.is_ascii_digit() || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || bytes[i] == b'.'
                    || bytes[i] == b'e'
                    || bytes[i] == b'E'
                    || ((bytes[i] == b'+' || bytes[i] == b'-') && (bytes[i - 1] == b'e' || bytes[i - 1] == b'E')))
            {
                i += 1;
            }
            let num_str = src.get(start..i).unwrap_or("0");
            if i < bytes.len() && (bytes[i] == b'f' || bytes[i] == b'F' || bytes[i] == b'u' || bytes[i] == b'U') {
                i += 1;
            }
            let val = num_str.parse::<f32>().map_err(|_| format!("invalid number `{num_str}`"))?;
            toks.push(Tok::Num(if val.is_finite() { val } else { 0.0 }));
            continue;
        }
        let two = (b, bytes.get(i + 1).copied().unwrap_or(0));
        let paired = match two {
            (b'=', b'=') => Some(Tok::EqEq),
            (b'!', b'=') => Some(Tok::BangEq),
            (b'<', b'=') => Some(Tok::LtEq),
            (b'>', b'=') => Some(Tok::GtEq),
            (b'&', b'&') => Some(Tok::AndAnd),
            (b'|', b'|') => Some(Tok::OrOr),
            (b'+', b'=') => Some(Tok::PlusEq),
            (b'-', b'=') => Some(Tok::MinusEq),
            (b'*', b'=') => Some(Tok::StarEq),
            (b'/', b'=') => Some(Tok::SlashEq),
            _ => None,
        };
        if let Some(tok) = paired {
            toks.push(tok);
            i += 2;
            continue;
        }
        toks.push(Tok::Sym(b as char));
        i += 1;
    }
    Ok(toks)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<&'a Tok> {
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn eat_sym(&mut self, c: char) -> bool {
        if self.peek() == Some(&Tok::Sym(c)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, c: char) -> Result<(), String> {
        if self.eat_sym(c) { Ok(()) } else { Err(format!("expected `{c}` in DCTL")) }
    }

    fn parse_block(&mut self, depth: usize, until_brace: bool) -> Result<Vec<Stmt>, String> {
        if depth > MAX_PARSE_DEPTH {
            return Err("DCTL nesting exceeds maximum depth".into());
        }
        let mut stmts = Vec::new();
        while let Some(tok) = self.peek() {
            if until_brace && *tok == Tok::Sym('}') {
                self.pos += 1;
                return Ok(stmts);
            }
            if self.eat_sym(';') {
                continue;
            }
            self.parse_stmt_into(&mut stmts, depth + 1)?;
        }
        if until_brace { Err("unclosed `{` block in DCTL".into()) } else { Ok(stmts) }
    }

    fn parse_stmt_into(&mut self, out: &mut Vec<Stmt>, depth: usize) -> Result<(), String> {
        if depth > MAX_PARSE_DEPTH {
            return Err("DCTL statement nesting exceeds maximum depth".into());
        }
        match self.peek() {
            Some(Tok::Ident(k)) if k == "return" => {
                self.pos += 1;
                let e = self.parse_expr(depth + 1)?;
                self.expect_sym(';')?;
                out.push(Stmt::Return(e));
                Ok(())
            }
            Some(Tok::Ident(k)) if k == "if" => {
                out.push(self.parse_if(depth + 1)?);
                Ok(())
            }
            Some(Tok::Ident(k)) if k == "const" => {
                self.pos += 1;
                self.parse_stmt_into(out, depth + 1)
            }
            Some(Tok::Ident(k)) if is_type_name(k) => {
                self.pos += 1;
                // Support comma-separated declarations: `float r = 0.0f, g = 0.0f;`
                loop {
                    let Some(Tok::Ident(name)) = self.next().cloned() else {
                        return Err("expected variable name after DCTL type".into());
                    };
                    let expr = if self.eat_sym('=') { self.parse_expr(depth + 1)? } else { Expr::Num(0.0) };
                    out.push(Stmt::Assign { name, comp: None, expr });
                    if self.eat_sym(',') {
                        continue;
                    }
                    break;
                }
                self.expect_sym(';')
            }
            Some(Tok::Ident(_)) => {
                out.push(self.parse_assign(depth + 1)?);
                Ok(())
            }
            _ => Err("unexpected token in DCTL statement".into()),
        }
    }

    fn parse_if(&mut self, depth: usize) -> Result<Stmt, String> {
        self.pos += 1;
        self.expect_sym('(')?;
        let cond = self.parse_expr(depth + 1)?;
        self.expect_sym(')')?;
        let then_body = self.parse_branch_body(depth + 1)?;
        let else_body = if matches!(self.peek(), Some(Tok::Ident(k)) if k == "else") {
            self.pos += 1;
            self.parse_branch_body(depth + 1)?
        } else {
            Vec::new()
        };
        Ok(Stmt::If { cond, then_body, else_body })
    }

    fn parse_branch_body(&mut self, depth: usize) -> Result<Vec<Stmt>, String> {
        if self.eat_sym('{') {
            self.parse_block(depth + 1, true)
        } else {
            let mut single = Vec::new();
            self.parse_stmt_into(&mut single, depth + 1)?;
            Ok(single)
        }
    }

    fn parse_assign(&mut self, depth: usize) -> Result<Stmt, String> {
        let Some(Tok::Ident(name)) = self.next().cloned() else {
            return Err("expected identifier".into());
        };
        let comp = if self.eat_sym('.') {
            let Some(Tok::Ident(swz)) = self.next() else { return Err("expected swizzle after `.`".into()) };
            Some(swizzle_index(swz)?)
        } else {
            None
        };
        let base_lhs = if let Some(idx) = comp { Expr::Component(Box::new(Expr::Var(name.clone())), idx) } else { Expr::Var(name.clone()) };
        let expr = match self.next() {
            Some(Tok::Sym('=')) => self.parse_expr(depth + 1)?,
            Some(Tok::PlusEq) => Expr::Bin(BinOp::Add, Box::new(base_lhs), Box::new(self.parse_expr(depth + 1)?)),
            Some(Tok::MinusEq) => Expr::Bin(BinOp::Sub, Box::new(base_lhs), Box::new(self.parse_expr(depth + 1)?)),
            Some(Tok::StarEq) => Expr::Bin(BinOp::Mul, Box::new(base_lhs), Box::new(self.parse_expr(depth + 1)?)),
            Some(Tok::SlashEq) => Expr::Bin(BinOp::Div, Box::new(base_lhs), Box::new(self.parse_expr(depth + 1)?)),
            _ => return Err(format!("expected assignment operator after `{name}`")),
        };
        self.expect_sym(';')?;
        Ok(Stmt::Assign { name, comp, expr })
    }

    fn parse_expr(&mut self, depth: usize) -> Result<Expr, String> {
        if depth > MAX_PARSE_DEPTH {
            return Err("DCTL expression nesting exceeds maximum depth".into());
        }
        let cond = self.parse_or(depth)?;
        if self.eat_sym('?') {
            let a = self.parse_expr(depth + 1)?;
            self.expect_sym(':')?;
            let b = self.parse_expr(depth + 1)?;
            Ok(Expr::Ternary(Box::new(cond), Box::new(a), Box::new(b)))
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_and(depth)?;
        while self.peek() == Some(&Tok::OrOr) {
            self.pos += 1;
            left = Expr::Bin(BinOp::Or, Box::new(left), Box::new(self.parse_and(depth)?));
        }
        Ok(left)
    }

    fn parse_and(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_eq(depth)?;
        while self.peek() == Some(&Tok::AndAnd) {
            self.pos += 1;
            left = Expr::Bin(BinOp::And, Box::new(left), Box::new(self.parse_eq(depth)?));
        }
        Ok(left)
    }

    fn parse_eq(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_rel(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => BinOp::Eq,
                Some(Tok::BangEq) => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            left = Expr::Bin(op, Box::new(left), Box::new(self.parse_rel(depth)?));
        }
        Ok(left)
    }

    fn parse_rel(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_add(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym('<')) => BinOp::Lt,
                Some(Tok::LtEq) => BinOp::Le,
                Some(Tok::Sym('>')) => BinOp::Gt,
                Some(Tok::GtEq) => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            left = Expr::Bin(op, Box::new(left), Box::new(self.parse_add(depth)?));
        }
        Ok(left)
    }

    fn parse_add(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_mul(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym('+')) => BinOp::Add,
                Some(Tok::Sym('-')) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            left = Expr::Bin(op, Box::new(left), Box::new(self.parse_mul(depth)?));
        }
        Ok(left)
    }

    fn parse_mul(&mut self, depth: usize) -> Result<Expr, String> {
        let mut left = self.parse_unary(depth)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym('*')) => BinOp::Mul,
                Some(Tok::Sym('/')) => BinOp::Div,
                Some(Tok::Sym('%')) => BinOp::Rem,
                _ => break,
            };
            self.pos += 1;
            left = Expr::Bin(op, Box::new(left), Box::new(self.parse_unary(depth)?));
        }
        Ok(left)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Expr, String> {
        if depth > MAX_PARSE_DEPTH {
            return Err("DCTL expression nesting exceeds maximum depth".into());
        }
        if self.eat_sym('-') {
            return Ok(Expr::Neg(Box::new(self.parse_unary(depth + 1)?)));
        }
        if self.eat_sym('+') {
            return self.parse_unary(depth + 1);
        }
        if self.eat_sym('!') {
            return Ok(Expr::Not(Box::new(self.parse_unary(depth + 1)?)));
        }
        self.parse_postfix(depth)
    }

    fn parse_postfix(&mut self, depth: usize) -> Result<Expr, String> {
        let mut base = self.parse_primary(depth)?;
        while self.eat_sym('.') {
            let Some(Tok::Ident(swz)) = self.next() else { return Err("expected component name after `.`".into()) };
            base = Expr::Component(Box::new(base), swizzle_index(swz)?);
        }
        Ok(base)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Expr, String> {
        if self.eat_sym('(') {
            if let Some(Tok::Ident(ty)) = self.peek()
                && is_type_name(ty)
                && self.toks.get(self.pos + 1) == Some(&Tok::Sym(')'))
            {
                self.pos += 2;
                return self.parse_unary(depth + 1);
            }
            let inner = self.parse_expr(depth + 1)?;
            self.expect_sym(')')?;
            return Ok(inner);
        }
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Num(*n)),
            Some(Tok::Ident(id)) => {
                let name = id.clone();
                if self.eat_sym('(') {
                    let mut args = Vec::new();
                    if !self.eat_sym(')') {
                        loop {
                            args.push(self.parse_expr(depth + 1)?);
                            if self.eat_sym(')') {
                                break;
                            }
                            self.expect_sym(',')?;
                        }
                    }
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            _ => Err("expected expression in DCTL".into()),
        }
    }
}

fn is_type_name(s: &str) -> bool {
    matches!(s, "float" | "float2" | "float3" | "float4" | "int" | "uint" | "bool")
}

fn swizzle_index(s: &str) -> Result<usize, String> {
    match s {
        "x" | "r" => Ok(0),
        "y" | "g" => Ok(1),
        "z" | "b" => Ok(2),
        "w" | "a" => Ok(3),
        _ => Err(format!("unsupported vector component `.{s}`")),
    }
}

fn is_builtin_dctl_fn(name: &str) -> bool {
    matches!(
        name,
        "_tex2D"
            | "make_float2"
            | "make_float3"
            | "make_float4"
            | "_fminf"
            | "fminf"
            | "min"
            | "_fmaxf"
            | "fmaxf"
            | "max"
            | "_clampf"
            | "clamp"
            | "_saturatef"
            | "_mix"
            | "lerp"
            | "_fabs"
            | "fabs"
            | "abs"
            | "_sqrtf"
            | "sqrtf"
            | "sqrt"
            | "_rsqrtf"
            | "_powf"
            | "powf"
            | "pow"
            | "_expf"
            | "expf"
            | "exp"
            | "_exp2f"
            | "exp2f"
            | "_exp10f"
            | "_logf"
            | "logf"
            | "log"
            | "_log2f"
            | "log2f"
            | "_log10f"
            | "log10f"
            | "_sinf"
            | "sinf"
            | "sin"
            | "_cosf"
            | "cosf"
            | "cos"
            | "_tanf"
            | "tanf"
            | "tan"
            | "_sinpif"
            | "_cospif"
            | "_asinf"
            | "_acosf"
            | "_atanf"
            | "_atan2f"
            | "atan2f"
            | "_sinhf"
            | "_coshf"
            | "_tanhf"
            | "_asinhf"
            | "_acoshf"
            | "_atanhf"
            | "_floorf"
            | "floorf"
            | "_ceilf"
            | "ceilf"
            | "_truncf"
            | "truncf"
            | "_round"
            | "_roundf"
            | "roundf"
            | "_fmod"
            | "fmodf"
            | "_copysignf"
            | "_hypotf"
            | "_fdimf"
            | "_fmaf"
            | "_fdivide"
            | "_frecip"
            | "_ldexp"
            | "_smoothstepf"
            | "smoothstep"
            | "isinf"
            | "isnan"
            | "signbit"
            | "RAND"
            | "APPLY_LUT"
            | "APPLY_LUT_TRILINEAR"
    )
}

fn validate_stmts_calls(stmts: &[Stmt], helpers: &BTreeMap<String, DctlHelperFn>, uses_tex2d: &mut bool) -> Result<(), String> {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { expr, .. } | Stmt::Return(expr) => validate_expr_calls(expr, helpers, uses_tex2d)?,
            Stmt::If { cond, then_body, else_body } => {
                validate_expr_calls(cond, helpers, uses_tex2d)?;
                validate_stmts_calls(then_body, helpers, uses_tex2d)?;
                validate_stmts_calls(else_body, helpers, uses_tex2d)?;
            }
        }
    }
    Ok(())
}

fn validate_expr_calls(expr: &Expr, helpers: &BTreeMap<String, DctlHelperFn>, uses_tex2d: &mut bool) -> Result<(), String> {
    match expr {
        Expr::Num(_) | Expr::Var(_) => Ok(()),
        Expr::Component(a, _) | Expr::Neg(a) | Expr::Not(a) => validate_expr_calls(a, helpers, uses_tex2d),
        Expr::Bin(_, a, b) => {
            validate_expr_calls(a, helpers, uses_tex2d)?;
            validate_expr_calls(b, helpers, uses_tex2d)
        }
        Expr::Ternary(c, a, b) => {
            validate_expr_calls(c, helpers, uses_tex2d)?;
            validate_expr_calls(a, helpers, uses_tex2d)?;
            validate_expr_calls(b, helpers, uses_tex2d)
        }
        Expr::Call(name, args) => {
            if name == "_tex2D" {
                *uses_tex2d = true;
            }
            if matches!(name.as_str(), "APPLY_LUT" | "APPLY_LUT_TRILINEAR")
                && (args.len() < 4 || !matches!(args.get(3), Some(Expr::Var(_))))
            {
                return Err(format!("`{name}` expects `(r, g, b, lutIdent)`"));
            }
            if !is_builtin_dctl_fn(name) && !helpers.contains_key(name) {
                return Err(format!("unknown DCTL function `{name}`"));
            }
            if let Some(h) = helpers.get(name)
                && h.params.len() != args.len()
            {
                return Err(format!("DCTL helper `{name}` expects {} argument(s), got {}", h.params.len(), args.len()));
            }
            for a in args {
                validate_expr_calls(a, helpers, uses_tex2d)?;
            }
            Ok(())
        }
    }
}

/// Parse a `.dctl` shader string into a validated [`DctlProgram`].
pub fn parse_dctl(src: &str) -> Result<DctlProgram, String> {
    if src.len() > MAX_PLUGIN_TEXT_BYTES {
        return Err(format!("DCTL source exceeds the {MAX_PLUGIN_TEXT_BYTES}-byte limit"));
    }
    let (without_cubes, inline_luts, pre_declared) = extract_inline_cube_luts(src)?;
    let cleaned = strip_comments(&without_cubes);
    let mut extracted = extract_directives(&cleaned, pre_declared);
    extracted.enum_consts.insert("true".into(), 1.0);
    extracted.enum_consts.insert("false".into(), 0.0);
    extracted.enum_consts.insert("DCTL_LUT_TETRAHEDRAL".into(), 0.0);
    extracted.enum_consts.insert("DCTL_LUT_TRILINEAR".into(), 1.0);

    let raw_fns = extract_device_functions(&extracted.rest)?;
    if raw_fns.is_empty() {
        return Err("DCTL must define a `__DEVICE__ ... transform(...)` or `transition(...)` entry function".into());
    }

    let mut helpers = BTreeMap::new();
    let mut entry_opt: Option<(bool, bool, Vec<Stmt>)> = None;

    for rf in raw_fns {
        let toks = tokenize(&rf.body_src)?;
        let mut parser = Parser { toks: &toks, pos: 0 };
        let stmts = parser.parse_block(0, false)?;
        if rf.name == "transform" || rf.name == "transition" {
            if stmts.is_empty() {
                return Err("DCTL entry function body is empty".into());
            }
            let is_tr = rf.name == "transition";
            let returns_alpha = rf.ret_type == "float4" || is_tr;
            entry_opt = Some((is_tr, returns_alpha, stmts));
        } else {
            helpers.insert(rf.name.clone(), DctlHelperFn { name: rf.name, params: rf.params, body: stmts });
        }
    }

    let Some((is_transition, returns_alpha, body)) = entry_opt else {
        return Err("DCTL must define a `transform(...)` or `transition(...)` entry function".into());
    };

    let mut uses_tex2d = false;
    for h in helpers.values() {
        validate_stmts_calls(&h.body, &helpers, &mut uses_tex2d)?;
    }
    validate_stmts_calls(&body, &helpers, &mut uses_tex2d)?;

    Ok(DctlProgram {
        is_transition,
        returns_alpha,
        straight_alpha: extracted.straight_alpha,
        uses_tex2d,
        ui_params: extracted.ui_params,
        enum_consts: extracted.enum_consts,
        helpers,
        luts: inline_luts,
        declared_luts: extracted.declared_luts,
        body,
    })
}

struct EvalRuntime<'a> {
    prog: &'a DctlProgram,
    base: BTreeMap<&'a str, DctlVal>,
    ctx: &'a DctlPixelCtx<'a>,
    steps: usize,
}

impl DctlProgram {
    /// Evaluate the DCTL program for one pixel with a hard step ceiling ([`MAX_EVAL_STEPS`]).
    pub fn eval_pixel(&self, ctx: &DctlPixelCtx<'_>) -> DctlVal {
        let mut base: BTreeMap<&str, DctlVal> = BTreeMap::new();
        for (k, v) in &self.enum_consts {
            base.insert(k.as_str(), DctlVal::Scalar(*v));
        }
        for p in &self.ui_params {
            let val = match &p.kind {
                DctlParamKind::Float { default, .. } => DctlVal::Scalar(*default),
                DctlParamKind::Int { default, .. } => DctlVal::Scalar(*default as f32),
                DctlParamKind::Bool { default } => DctlVal::Scalar(if *default { 1.0 } else { 0.0 }),
                DctlParamKind::Combo { default, .. } => DctlVal::Scalar(*default as f32),
                DctlParamKind::Color { default } => DctlVal::Vec3(*default),
            };
            base.insert(p.name.as_str(), val);
        }
        for (k, v) in ctx.params {
            base.insert(*k, DctlVal::Scalar(*v));
        }
        base.insert("p_Width", DctlVal::Scalar(ctx.width as f32));
        base.insert("p_Height", DctlVal::Scalar(ctx.height as f32));
        base.insert("p_X", DctlVal::Scalar(ctx.x as f32));
        base.insert("p_Y", DctlVal::Scalar(ctx.y as f32));
        base.insert("p_R", DctlVal::Scalar(ctx.rgba[0]));
        base.insert("p_G", DctlVal::Scalar(ctx.rgba[1]));
        base.insert("p_B", DctlVal::Scalar(ctx.rgba[2]));
        base.insert("p_A", DctlVal::Scalar(ctx.rgba[3]));
        base.insert("p_FromR", DctlVal::Scalar(ctx.rgba[0]));
        base.insert("p_FromG", DctlVal::Scalar(ctx.rgba[1]));
        base.insert("p_FromB", DctlVal::Scalar(ctx.rgba[2]));
        base.insert("p_FromA", DctlVal::Scalar(ctx.rgba[3]));
        base.insert("p_ToR", DctlVal::Scalar(ctx.to_rgba[0]));
        base.insert("p_ToG", DctlVal::Scalar(ctx.to_rgba[1]));
        base.insert("p_ToB", DctlVal::Scalar(ctx.to_rgba[2]));
        base.insert("p_ToA", DctlVal::Scalar(ctx.to_rgba[3]));
        base.insert("TIMELINE_FRAME_INDEX", DctlVal::Scalar(ctx.frame_index as f32));
        base.insert("TRANSITION_PROGRESS", DctlVal::Scalar(ctx.transition_progress));
        base.insert("p_Transition", DctlVal::Scalar(ctx.transition_progress));

        let mut rt = EvalRuntime { prog: self, base, ctx, steps: 0 };
        let mut owned: BTreeMap<String, DctlVal> = BTreeMap::new();
        if let Some(ret) = eval_stmts(&self.body, &mut rt, &mut owned, 0) {
            ret
        } else if self.returns_alpha {
            DctlVal::Vec4(ctx.rgba)
        } else {
            DctlVal::Vec3([ctx.rgba[0], ctx.rgba[1], ctx.rgba[2]])
        }
    }
}

fn eval_stmts(stmts: &[Stmt], rt: &mut EvalRuntime<'_>, owned: &mut BTreeMap<String, DctlVal>, call_depth: usize) -> Option<DctlVal> {
    for stmt in stmts {
        rt.steps += 1;
        if rt.steps > MAX_EVAL_STEPS {
            return None;
        }
        match stmt {
            Stmt::Assign { name, comp: None, expr } => {
                let v = eval_expr(expr, rt, owned, call_depth);
                owned.insert(name.clone(), v);
            }
            Stmt::Assign { name, comp: Some(idx), expr } => {
                let rhs = eval_expr(expr, rt, owned, call_depth).as_f32();
                let cur = owned.get(name.as_str()).or_else(|| rt.base.get(name.as_str())).copied().unwrap_or(DctlVal::Vec4([0.0; 4]));
                let next = match cur {
                    DctlVal::Vec2(mut v) => {
                        if let Some(slot) = v.get_mut(*idx) {
                            *slot = rhs;
                        }
                        DctlVal::Vec2(v)
                    }
                    DctlVal::Vec3(mut v) => {
                        if let Some(slot) = v.get_mut(*idx) {
                            *slot = rhs;
                        }
                        DctlVal::Vec3(v)
                    }
                    DctlVal::Vec4(mut v) => {
                        if let Some(slot) = v.get_mut(*idx) {
                            *slot = rhs;
                        }
                        DctlVal::Vec4(v)
                    }
                    DctlVal::Scalar(_) => DctlVal::Scalar(rhs),
                };
                owned.insert(name.clone(), next);
            }
            Stmt::If { cond, then_body, else_body } => {
                if eval_expr(cond, rt, owned, call_depth).as_f32() != 0.0 {
                    if let Some(r) = eval_stmts(then_body, rt, owned, call_depth) {
                        return Some(r);
                    }
                } else if let Some(r) = eval_stmts(else_body, rt, owned, call_depth) {
                    return Some(r);
                }
            }
            Stmt::Return(expr) => return Some(eval_expr(expr, rt, owned, call_depth)),
        }
    }
    None
}

fn eval_expr(expr: &Expr, rt: &mut EvalRuntime<'_>, owned: &BTreeMap<String, DctlVal>, call_depth: usize) -> DctlVal {
    rt.steps += 1;
    if rt.steps > MAX_EVAL_STEPS {
        return DctlVal::Scalar(0.0);
    }
    match expr {
        Expr::Num(n) => DctlVal::Scalar(*n),
        Expr::Var(name) => owned.get(name.as_str()).or_else(|| rt.base.get(name.as_str())).copied().unwrap_or(DctlVal::Scalar(0.0)),
        Expr::Component(inner, idx) => match eval_expr(inner, rt, owned, call_depth) {
            DctlVal::Scalar(s) => DctlVal::Scalar(s),
            DctlVal::Vec2(v) => DctlVal::Scalar(v.get(*idx).copied().unwrap_or(0.0)),
            DctlVal::Vec3(v) => DctlVal::Scalar(v.get(*idx).copied().unwrap_or(0.0)),
            DctlVal::Vec4(v) => DctlVal::Scalar(v.get(*idx).copied().unwrap_or(0.0)),
        },
        Expr::Neg(inner) => eval_expr(inner, rt, owned, call_depth).map_unary(|x| -x),
        Expr::Not(inner) => DctlVal::Scalar(if eval_expr(inner, rt, owned, call_depth).as_f32() == 0.0 { 1.0 } else { 0.0 }),
        Expr::Ternary(c, a, b) => {
            if eval_expr(c, rt, owned, call_depth).as_f32() != 0.0 {
                eval_expr(a, rt, owned, call_depth)
            } else {
                eval_expr(b, rt, owned, call_depth)
            }
        }
        Expr::Bin(op, l, r) => eval_bin(*op, eval_expr(l, rt, owned, call_depth), eval_expr(r, rt, owned, call_depth)),
        Expr::Call(name, args) => eval_call(name, args, rt, owned, call_depth),
    }
}

fn scalar_op(op: BinOp, a: f32, b: f32) -> f32 {
    let out = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b.abs() > 1e-12 {
                a / b
            } else {
                0.0
            }
        }
        BinOp::Rem => {
            if b.abs() > 1e-12 {
                a % b
            } else {
                0.0
            }
        }
        BinOp::Eq => {
            if (a - b).abs() <= 1e-6 {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Ne => {
            if (a - b).abs() > 1e-6 {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Lt => {
            if a < b {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Le => {
            if a <= b {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Gt => {
            if a > b {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Ge => {
            if a >= b {
                1.0
            } else {
                0.0
            }
        }
        BinOp::And => {
            if a != 0.0 && b != 0.0 {
                1.0
            } else {
                0.0
            }
        }
        BinOp::Or => {
            if a != 0.0 || b != 0.0 {
                1.0
            } else {
                0.0
            }
        }
    };
    if out.is_finite() { out } else { 0.0 }
}

fn eval_bin(op: BinOp, l: DctlVal, r: DctlVal) -> DctlVal {
    match (l, r) {
        (DctlVal::Vec2(a), DctlVal::Vec2(b)) => DctlVal::Vec2([scalar_op(op, a[0], b[0]), scalar_op(op, a[1], b[1])]),
        (DctlVal::Vec2(a), DctlVal::Scalar(b)) => DctlVal::Vec2([scalar_op(op, a[0], b), scalar_op(op, a[1], b)]),
        (DctlVal::Scalar(a), DctlVal::Vec2(b)) => DctlVal::Vec2([scalar_op(op, a, b[0]), scalar_op(op, a, b[1])]),
        (DctlVal::Vec3(a), DctlVal::Vec3(b)) => DctlVal::Vec3([scalar_op(op, a[0], b[0]), scalar_op(op, a[1], b[1]), scalar_op(op, a[2], b[2])]),
        (DctlVal::Vec3(a), DctlVal::Scalar(b)) => DctlVal::Vec3([scalar_op(op, a[0], b), scalar_op(op, a[1], b), scalar_op(op, a[2], b)]),
        (DctlVal::Scalar(a), DctlVal::Vec3(b)) => DctlVal::Vec3([scalar_op(op, a, b[0]), scalar_op(op, a, b[1]), scalar_op(op, a, b[2])]),
        (DctlVal::Vec4(a), DctlVal::Vec4(b)) => {
            DctlVal::Vec4([scalar_op(op, a[0], b[0]), scalar_op(op, a[1], b[1]), scalar_op(op, a[2], b[2]), scalar_op(op, a[3], b[3])])
        }
        (DctlVal::Vec4(a), DctlVal::Scalar(b)) => {
            DctlVal::Vec4([scalar_op(op, a[0], b), scalar_op(op, a[1], b), scalar_op(op, a[2], b), scalar_op(op, a[3], b)])
        }
        (DctlVal::Scalar(a), DctlVal::Vec4(b)) => {
            DctlVal::Vec4([scalar_op(op, a, b[0]), scalar_op(op, a, b[1]), scalar_op(op, a, b[2]), scalar_op(op, a, b[3])])
        }
        (a, b) => DctlVal::Scalar(scalar_op(op, a.as_f32(), b.as_f32())),
    }
}

fn eval_tex2d(args: &[Expr], rt: &mut EvalRuntime<'_>, owned: &BTreeMap<String, DctlVal>, call_depth: usize) -> DctlVal {
    let tex_name = match args.first() {
        Some(Expr::Var(v)) => v.as_str(),
        _ => "",
    };
    let sx = args.get(1).map_or(rt.ctx.x as isize, |e| eval_expr(e, rt, owned, call_depth).as_f32().round() as isize);
    let sy = args.get(2).map_or(rt.ctx.y as isize, |e| eval_expr(e, rt, owned, call_depth).as_f32().round() as isize);

    let from_px = if sx == rt.ctx.x as isize && sy == rt.ctx.y as isize {
        rt.ctx.rgba
    } else if let Some(sampler) = rt.ctx.sample_from {
        sampler(sx, sy)
    } else {
        rt.ctx.rgba
    };
    let to_px = if sx == rt.ctx.x as isize && sy == rt.ctx.y as isize {
        rt.ctx.to_rgba
    } else if let Some(sampler) = rt.ctx.sample_to {
        sampler(sx, sy)
    } else {
        rt.ctx.to_rgba
    };

    let val = match tex_name {
        "p_TexR" | "p_FromTexR" => from_px[0],
        "p_TexG" | "p_FromTexG" => from_px[1],
        "p_TexB" | "p_FromTexB" => from_px[2],
        "p_TexA" | "p_FromTexA" => from_px[3],
        "p_ToTexR" => to_px[0],
        "p_ToTexG" => to_px[1],
        "p_ToTexB" => to_px[2],
        "p_ToTexA" => to_px[3],
        _ => 0.0,
    };
    DctlVal::Scalar(val)
}

fn eval_apply_lut(name: &str, args: &[Expr], rt: &mut EvalRuntime<'_>, owned: &BTreeMap<String, DctlVal>, call_depth: usize) -> DctlVal {
    let r = args.first().map_or(0.0, |e| eval_expr(e, rt, owned, call_depth).as_f32());
    let g = args.get(1).map_or(0.0, |e| eval_expr(e, rt, owned, call_depth).as_f32());
    let b = args.get(2).map_or(0.0, |e| eval_expr(e, rt, owned, call_depth).as_f32());
    let lut_ident = match args.get(3) {
        Some(Expr::Var(v)) => v.as_str(),
        _ => "",
    };
    let explicit_trilinear = name == "APPLY_LUT_TRILINEAR"
        || args.get(4).is_some_and(|e| (eval_expr(e, rt, owned, call_depth).as_f32() - 1.0).abs() < 0.5);
    let eval_with = |lut: &DctlCubeLut, force_tri: bool, decl_interp: Option<DctlLutInterp>| -> [f32; 3] {
        let mode = if force_tri { DctlLutInterp::Trilinear } else { decl_interp.unwrap_or(lut.interp) };
        match mode {
            DctlLutInterp::Tetrahedral => lut.eval_tetrahedral([r, g, b]),
            DctlLutInterp::Trilinear => lut.eval_trilinear([r, g, b]),
        }
    };
    let decl = rt.prog.declared_luts.get(lut_ident);
    let decl_interp = decl.map(|(_, interp)| *interp);
    if let Some((_, ctx_lut)) = rt.ctx.luts.iter().find(|(id, _)| *id == lut_ident) {
        return DctlVal::Vec3(eval_with(ctx_lut, explicit_trilinear, decl_interp));
    }
    if let Some(inline_lut) = rt.prog.luts.get(lut_ident) {
        return DctlVal::Vec3(eval_with(inline_lut, explicit_trilinear, None));
    }
    let fallback = if decl.is_some_and(|(tag, _)| tag.to_ascii_lowercase().contains("identity")) {
        DctlCubeLut::identity(2)
    } else {
        DctlCubeLut::default_film_shaper()
    };
    DctlVal::Vec3(eval_with(&fallback, explicit_trilinear, decl_interp))
}

fn eval_call(name: &str, args: &[Expr], rt: &mut EvalRuntime<'_>, owned: &BTreeMap<String, DctlVal>, call_depth: usize) -> DctlVal {
    if name == "_tex2D" {
        return eval_tex2d(args, rt, owned, call_depth);
    }
    if matches!(name, "APPLY_LUT" | "APPLY_LUT_TRILINEAR") {
        return eval_apply_lut(name, args, rt, owned, call_depth);
    }

    let arg_vals: Vec<DctlVal> = args.iter().map(|a| eval_expr(a, rt, owned, call_depth)).collect();

    // User-defined `__DEVICE__` helper function
    if let Some(helper) = rt.prog.helpers.get(name) {
        if call_depth >= MAX_CALL_DEPTH {
            return DctlVal::Scalar(0.0);
        }
        let mut local = BTreeMap::new();
        for (pname, val) in helper.params.iter().zip(arg_vals.iter().copied()) {
            local.insert(pname.clone(), val);
        }
        return eval_stmts(&helper.body, rt, &mut local, call_depth + 1).unwrap_or(DctlVal::Scalar(0.0));
    }

    // Polymorphic vector/scalar DCTL intrinsics (`T _mix(T x, T y, float a)` etc.)
    if matches!(name, "_mix" | "lerp")
        && let (Some(&x), Some(&y), Some(&a)) = (arg_vals.first(), arg_vals.get(1), arg_vals.get(2))
    {
        let t = a.as_f32();
        return eval_bin(BinOp::Add, x, eval_bin(BinOp::Mul, eval_bin(BinOp::Sub, y, x), DctlVal::Scalar(t)));
    }
    if matches!(name, "_saturatef")
        && let Some(&x) = arg_vals.first()
    {
        return x.map_unary(|v| v.clamp(0.0, 1.0));
    }
    if matches!(name, "_fabs" | "fabs" | "abs")
        && let Some(&x) = arg_vals.first()
    {
        return x.map_unary(f32::abs);
    }

    let a0 = arg_vals.first().map_or(0.0, |v| v.as_f32());
    let a1 = arg_vals.get(1).map_or(0.0, |v| v.as_f32());
    let a2 = arg_vals.get(2).map_or(0.0, |v| v.as_f32());
    let a3 = arg_vals.get(3).map_or(1.0, |v| v.as_f32());

    let scalar = match name {
        "make_float2" => return DctlVal::Vec2([a0, a1]),
        "make_float3" => return DctlVal::Vec3([a0, a1, a2]),
        "make_float4" => return DctlVal::Vec4([a0, a1, a2, a3]),
        "_fminf" | "fminf" | "min" => a0.min(a1),
        "_fmaxf" | "fmaxf" | "max" => a0.max(a1),
        "_clampf" | "clamp" => a0.clamp(a1.min(a2), a1.max(a2)),
        "_sqrtf" | "sqrtf" | "sqrt" => a0.max(0.0).sqrt(),
        "_rsqrtf" => 1.0 / a0.max(1e-12).sqrt(),
        "_powf" | "powf" | "pow" => {
            if a0 >= 0.0 {
                a0.powf(a1)
            } else {
                -((-a0).powf(a1))
            }
        }
        "_expf" | "expf" | "exp" => a0.clamp(-40.0, 40.0).exp(),
        "_exp2f" | "exp2f" => a0.clamp(-40.0, 40.0).exp2(),
        "_exp10f" => 10.0f32.powf(a0.clamp(-20.0, 20.0)),
        "_logf" | "logf" | "log" => a0.max(1e-12).ln(),
        "_log2f" | "log2f" => a0.max(1e-12).log2(),
        "_log10f" | "log10f" => a0.max(1e-12).log10(),
        "_sinf" | "sinf" | "sin" => a0.sin(),
        "_cosf" | "cosf" | "cos" => a0.cos(),
        "_tanf" | "tanf" | "tan" => a0.tan(),
        "_sinpif" => (a0 * std::f32::consts::PI).sin(),
        "_cospif" => (a0 * std::f32::consts::PI).cos(),
        "_asinf" => a0.clamp(-1.0, 1.0).asin(),
        "_acosf" => a0.clamp(-1.0, 1.0).acos(),
        "_atanf" => a0.atan(),
        "_atan2f" | "atan2f" => a0.atan2(a1),
        "_sinhf" => a0.clamp(-20.0, 20.0).sinh(),
        "_coshf" => a0.clamp(-20.0, 20.0).cosh(),
        "_tanhf" => a0.tanh(),
        "_asinhf" => a0.asinh(),
        "_acoshf" => a0.max(1.0).acosh(),
        "_atanhf" => a0.clamp(-0.999_999, 0.999_999).atanh(),
        "_floorf" | "floorf" => a0.floor(),
        "_ceilf" | "ceilf" => a0.ceil(),
        "_truncf" | "truncf" => a0.trunc(),
        "_round" | "_roundf" | "roundf" => a0.round(),
        "_fmod" | "fmodf" => {
            if a1.abs() > 1e-12 {
                a0 % a1
            } else {
                0.0
            }
        }
        "_copysignf" => a0.copysign(a1),
        "_hypotf" => a0.hypot(a1),
        "_fdimf" => (a0 - a1).max(0.0),
        "_fmaf" => a0.mul_add(a1, a2),
        "_fdivide" => {
            if a1.abs() > 1e-12 {
                a0 / a1
            } else {
                0.0
            }
        }
        "_frecip" => {
            if a0.abs() > 1e-12 {
                1.0 / a0
            } else {
                0.0
            }
        }
        "_ldexp" => a0 * 2.0f32.powi((a1.round() as i32).clamp(-64, 64)),
        "_smoothstepf" | "smoothstep" => {
            let denom = (a1 - a0).abs().max(1e-6) * if a1 >= a0 { 1.0 } else { -1.0 };
            let t = ((a2 - a0) / denom).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        }
        "isinf" => {
            if a0.is_infinite() {
                1.0
            } else {
                0.0
            }
        }
        "isnan" => {
            if a0.is_nan() {
                1.0
            } else {
                0.0
            }
        }
        "signbit" => {
            if a0.is_sign_negative() {
                1.0
            } else {
                0.0
            }
        }
        "RAND" => {
            let mut h =
                (a0.to_bits() as u64) ^ ((rt.ctx.x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ ((rt.ctx.y as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
            h ^= h >> 30;
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            ((h >> 40) as f32) / ((1u64 << 24) as f32)
        }
        _ => 0.0,
    };
    DctlVal::Scalar(if scalar.is_finite() { scalar } else { 0.0 })
}

/// Built-in DCTL starter presets shipped in pure code (original clean-room expressions).
pub fn builtin_dctls() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            "cinematic_split_tone",
            "Cinematic Teal & Amber DCTL",
            concat!(
                "DEFINE_UI_PARAMS(p1, Warmth Lift, DCTLUI_SLIDER_FLOAT, 0.18, -1.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Cool Shadow, DCTLUI_SLIDER_FLOAT, 0.16, -1.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p3, Contrast Pivot, DCTLUI_SLIDER_FLOAT, 1.12, 0.2, 3.0, 0.01)\n",
                "__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {\n",
                "    float luma = 0.2126f * p_R + 0.7152f * p_G + 0.0722f * p_B;\n",
                "    float hi = _saturatef(luma);\n",
                "    float lo = 1.0f - hi;\n",
                "    float r = _powf(_fmaxf(p_R, 0.0f), p3) + p1 * hi * 0.15f - p2 * lo * 0.06f;\n",
                "    float g = _powf(_fmaxf(p_G, 0.0f), p3) + p1 * hi * 0.04f + p2 * lo * 0.03f;\n",
                "    float b = _powf(_fmaxf(p_B, 0.0f), p3) - p1 * hi * 0.08f + p2 * lo * 0.14f;\n",
                "    return make_float3(r, g, b);\n",
                "}\n"
            ),
        ),
        (
            "bleach_bypass_dctl",
            "Silver Retention / Bleach Bypass DCTL",
            concat!(
                "DEFINE_UI_PARAMS(p1, Silver Blend, DCTLUI_SLIDER_FLOAT, 0.55, 0.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Gamma Roll, DCTLUI_SLIDER_FLOAT, 1.15, 0.5, 2.5, 0.01)\n",
                "__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {\n",
                "    float y = _saturatef(0.2126f * p_R + 0.7152f * p_G + 0.0722f * p_B);\n",
                "    float overlay = y < 0.5f ? (2.0f * y * y) : (1.0f - 2.0f * (1.0f - y) * (1.0f - y));\n",
                "    float s = _powf(_fmaxf(overlay, 0.0f), p2);\n",
                "    return _mix(make_float3(p_R, p_G, p_B), make_float3(s, s, s), p1);\n",
                "}\n"
            ),
        ),
        (
            "highlight_soft_clip",
            "Filmic Shoulder Soft-Clip DCTL",
            concat!(
                "DEFINE_UI_PARAMS(p1, Knee Start, DCTLUI_SLIDER_FLOAT, 0.72, 0.2, 0.98, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Exposure Gain, DCTLUI_SLIDER_FLOAT, 1.0, 0.1, 4.0, 0.01)\n",
                "__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {\n",
                "    float r = p_R * p2, g = p_G * p2, b = p_B * p2;\n",
                "    r = r > p1 ? (p1 + (1.0f - p1) * (1.0f - _expf(-(r - p1) / (1.0f - p1)))) : r;\n",
                "    g = g > p1 ? (p1 + (1.0f - p1) * (1.0f - _expf(-(g - p1) / (1.0f - p1)))) : g;\n",
                "    b = b > p1 ? (p1 + (1.0f - p1) * (1.0f - _expf(-(b - p1) / (1.0f - p1)))) : b;\n",
                "    return make_float3(r, g, b);\n",
                "}\n"
            ),
        ),
    ]
}

/// Built-in DaVinci Resolve-style DCTL transition shaders (`transition(...)` entry point).
pub fn builtin_transition_dctls() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            "dissolve_glow",
            "Dissolve Glow",
            concat!(
                "DEFINE_UI_PARAMS(p1, Glow Intensity, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Warm Tint, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "__DEVICE__ float4 transition(int p_Width, int p_Height, int p_X, int p_Y, ",
                "__TEXTURE__ p_FromTexR, __TEXTURE__ p_FromTexG, __TEXTURE__ p_FromTexB, __TEXTURE__ p_FromTexA, ",
                "__TEXTURE__ p_ToTexR, __TEXTURE__ p_ToTexG, __TEXTURE__ p_ToTexB, __TEXTURE__ p_ToTexA, float p_Transition) {\n",
                "    float ar = _tex2D(p_FromTexR, p_X, p_Y);\n",
                "    float ag = _tex2D(p_FromTexG, p_X, p_Y);\n",
                "    float ab = _tex2D(p_FromTexB, p_X, p_Y);\n",
                "    float aa = _tex2D(p_FromTexA, p_X, p_Y);\n",
                "    float br = _tex2D(p_ToTexR, p_X, p_Y);\n",
                "    float bg = _tex2D(p_ToTexG, p_X, p_Y);\n",
                "    float bb = _tex2D(p_ToTexB, p_X, p_Y);\n",
                "    float ba = _tex2D(p_ToTexA, p_X, p_Y);\n",
                "    float t = _saturatef(TRANSITION_PROGRESS);\n",
                "    float s = t * t * (3.0f - 2.0f * t);\n",
                "    float bell = 4.0f * t * (1.0f - t);\n",
                "    float r = _mix(ar, br, s);\n",
                "    float g = _mix(ag, bg, s);\n",
                "    float b = _mix(ab, bb, s);\n",
                "    float a = _mix(aa, ba, s);\n",
                "    float luma = 0.2126f * r + 0.7152f * g + 0.0722f * b;\n",
                "    float glow = bell * (0.12f + 0.95f * p1) * (0.25f + luma);\n",
                "    float warm = p2 - 0.5f;\n",
                "    return make_float4(r + glow * (1.0f + 0.45f * warm), g + glow * 0.85f, b + glow * (0.75f - 0.45f * warm), _saturatef(a));\n",
                "}\n"
            ),
        ),
        (
            "luma_film_burn",
            "Luma Film Burn",
            concat!(
                "DEFINE_UI_PARAMS(p1, Burn Heat, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Edge Softness, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "__DEVICE__ float4 transition(int p_Width, int p_Height, int p_X, int p_Y, ",
                "__TEXTURE__ p_FromTexR, __TEXTURE__ p_FromTexG, __TEXTURE__ p_FromTexB, __TEXTURE__ p_FromTexA, ",
                "__TEXTURE__ p_ToTexR, __TEXTURE__ p_ToTexG, __TEXTURE__ p_ToTexB, __TEXTURE__ p_ToTexA, float p_Transition) {\n",
                "    float ar = _tex2D(p_FromTexR, p_X, p_Y);\n",
                "    float ag = _tex2D(p_FromTexG, p_X, p_Y);\n",
                "    float ab = _tex2D(p_FromTexB, p_X, p_Y);\n",
                "    float aa = _tex2D(p_FromTexA, p_X, p_Y);\n",
                "    float br = _tex2D(p_ToTexR, p_X, p_Y);\n",
                "    float bg = _tex2D(p_ToTexG, p_X, p_Y);\n",
                "    float bb = _tex2D(p_ToTexB, p_X, p_Y);\n",
                "    float ba = _tex2D(p_ToTexA, p_X, p_Y);\n",
                "    float t = _saturatef(p_Transition);\n",
                "    float lumA = 0.2126f * ar + 0.7152f * ag + 0.0722f * ab;\n",
                "    float soft = 0.08f + 0.42f * p2;\n",
                "    float thr = -soft + t * (1.0f + 2.0f * soft);\n",
                "    float k = _smoothstepf(thr - soft, thr + soft, 1.0f - _saturatef(lumA));\n",
                "    float edge = 4.0f * k * (1.0f - k) * 4.0f * t * (1.0f - t);\n",
                "    float heat = edge * (0.2f + 1.1f * p1);\n",
                "    float r = _mix(ar, br, k) + heat * 1.15f;\n",
                "    float g = _mix(ag, bg, k) + heat * 0.48f;\n",
                "    float b = _mix(ab, bb, k) + heat * 0.08f;\n",
                "    return make_float4(r, g, b, _saturatef(_mix(aa, ba, k)));\n",
                "}\n"
            ),
        ),
        (
            "chromatic_whip",
            "Chromatic Whip",
            concat!(
                "DEFINE_UI_PARAMS(p1, Whip Travel, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "DEFINE_UI_PARAMS(p2, Chroma Split, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 1.0, 0.01)\n",
                "__DEVICE__ float4 transition(int p_Width, int p_Height, int p_X, int p_Y, ",
                "__TEXTURE__ p_FromTexR, __TEXTURE__ p_FromTexG, __TEXTURE__ p_FromTexB, __TEXTURE__ p_FromTexA, ",
                "__TEXTURE__ p_ToTexR, __TEXTURE__ p_ToTexG, __TEXTURE__ p_ToTexB, __TEXTURE__ p_ToTexA, float p_Transition) {\n",
                "    float t = _saturatef(p_Transition);\n",
                "    float bell = 4.0f * t * (1.0f - t);\n",
                "    float shift = bell * p_Width * (0.04f + 0.22f * p1);\n",
                "    float split = bell * p_Width * (0.01f + 0.06f * p2);\n",
                "    float ar = _tex2D(p_FromTexR, p_X + shift + split, p_Y);\n",
                "    float ag = _tex2D(p_FromTexG, p_X + shift, p_Y);\n",
                "    float ab = _tex2D(p_FromTexB, p_X + shift - split, p_Y);\n",
                "    float aa = _tex2D(p_FromTexA, p_X + shift, p_Y);\n",
                "    float br = _tex2D(p_ToTexR, p_X - shift + split, p_Y);\n",
                "    float bg = _tex2D(p_ToTexG, p_X - shift, p_Y);\n",
                "    float bb = _tex2D(p_ToTexB, p_X - shift - split, p_Y);\n",
                "    float ba = _tex2D(p_ToTexA, p_X - shift, p_Y);\n",
                "    float k = _smoothstepf(0.15f, 0.85f, t);\n",
                "    return make_float4(_mix(ar, br, k), _mix(ag, bg, k), _mix(ab, bb, k), _saturatef(_mix(aa, ba, k)));\n",
                "}\n"
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{DctlCubeLut, DctlLutInterp, DctlParamKind, DctlPixelCtx, builtin_dctls, builtin_transition_dctls, parse_dctl};

    #[test]
    fn parses_cube_lut_and_evaluates_tetrahedral_vs_trilinear() {
        // Off-diagonal corners carry strong chromaticity while (0,0,0) and (1,1,1) lie on the neutral grey axis.
        // Tetrahedral simplex interpolation uses ONLY c000 and c111 along r == g == b, preserving exact neutrality,
        // whereas trilinear interpolation blends all 8 corners.
        let cube_src = "\
TITLE \"Neutral Axis Test\"
DOMAIN_MIN 0.0 0.0 0.0
DOMAIN_MAX 1.0 1.0 1.0
LUT_1D_SIZE 2
LUT_3D_SIZE 2
0.0 0.0 0.0
1.0 1.0 1.0
0.0 0.0 0.0
1.0 0.2 0.0
0.0 1.0 0.3
1.0 1.0 0.1
0.1 0.0 1.0
0.9 0.1 1.0
0.0 0.8 1.0
1.0 1.0 1.0
";
        let lut = DctlCubeLut::parse_cube(cube_src).expect("valid .cube");
        assert_eq!(lut.title, "Neutral Axis Test");
        assert_eq!(lut.lut_3d_size, 2);
        assert_eq!(lut.lut_1d.len(), 2);

        let tet = lut.eval_tetrahedral([0.35, 0.35, 0.35]);
        assert!((tet[0] - 0.35).abs() < 1e-6 && (tet[1] - 0.35).abs() < 1e-6 && (tet[2] - 0.35).abs() < 1e-6, "tetrahedral neutral: {tet:?}");

        let tri = lut.eval_trilinear([0.35, 0.35, 0.35]);
        assert!((tri[2] - 0.35).abs() > 0.02, "trilinear mixes off-diagonal corners: {tri:?}");

        // On any linear/affine 3D function f(r,g,b), all 6 tetrahedra and trilinear agree identically.
        let mut affine_lines = String::from("LUT_3D_SIZE 3\n");
        for bi in 0..3 {
            for gi in 0..3 {
                for ri in 0..3 {
                    let (r, g, b) = (ri as f32 * 0.5, gi as f32 * 0.5, bi as f32 * 0.5);
                    affine_lines.push_str(&format!(
                        "{:.6} {:.6} {:.6}\n",
                        0.1 + 0.5 * r + 0.2 * g + 0.1 * b,
                        0.05 + 0.1 * r + 0.6 * g + 0.15 * b,
                        0.2 + 0.05 * r + 0.15 * g + 0.55 * b
                    ));
                }
            }
        }
        let affine_lut = DctlCubeLut::parse_cube(&affine_lines).expect("3x3x3 affine cube");
        for pt in [[0.12, 0.77, 0.41], [0.83, 0.24, 0.62], [0.45, 0.18, 0.91], [0.72, 0.88, 0.15], [0.29, 0.64, 0.81], [0.66, 0.31, 0.19]] {
            let a = affine_lut.eval_tetrahedral(pt);
            let b = affine_lut.eval_trilinear(pt);
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-5, "pt={pt:?} tet={a:?} tri={b:?}");
            }
        }
    }

    #[test]
    fn supports_define_cube_lut_and_define_lut_with_apply_lut() {
        let inline_shader = r#"
DEFINE_CUBE_LUT(warmCube)
{
TITLE "Warm Inline"
LUT_3D_SIZE 2
0.0 0.0 0.0
0.9 0.1 0.0
0.0 0.9 0.0
0.95 0.92 0.05
0.0 0.0 0.85
0.9 0.1 0.85
0.0 0.9 0.85
1.0 0.95 0.88
}
DEFINE_LUT(externalFilm, ../Looks/FilmPrint.cube)
__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {
    float3 c = APPLY_LUT(p_R, p_G, p_B, warmCube);
    float3 f = APPLY_LUT(c.x, c.y, c.z, externalFilm);
    return f;
}
"#;
        let prog = parse_dctl(inline_shader).expect("inline + external LUT DCTL");
        assert!(prog.luts.contains_key("warmCube"));
        assert!(prog.declared_luts.contains_key("externalFilm"));

        // Without external LUT attached: uses safe built-in film-shaper fallback (never panics).
        let fallback_rgb = prog.eval_pixel(&DctlPixelCtx::simple(16, 16, 4, 4, [1.0, 1.0, 1.0, 1.0], &[])).into_rgb([0.0; 3]);
        assert!(fallback_rgb.iter().all(|v| v.is_finite()));

        // With explicit ctx.luts override: uses caller-supplied LUT table.
        let mut custom = DctlCubeLut::identity(2);
        custom.interp = DctlLutInterp::Trilinear;
        for entry in &mut custom.lut_3d {
            entry[0] *= 0.5;
        }
        let mut ctx = DctlPixelCtx::simple(16, 16, 4, 4, [1.0, 1.0, 1.0, 1.0], &[]);
        let bindings = [("externalFilm", &custom)];
        ctx.luts = &bindings;
        let overridden_rgb = prog.eval_pixel(&ctx).into_rgb([0.0; 3]);
        assert!((overridden_rgb[0] - 0.5).abs() < 1e-4, "expected 0.5 red from attached LUT, got {overridden_rgb:?}");
    }

    #[test]
    fn builtin_transition_dctls_parse_and_interpolate_frames() {
        for (id, _, code) in builtin_transition_dctls() {
            let prog = parse_dctl(code).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(prog.is_transition && prog.returns_alpha, "{id}");
            let mut ctx = DctlPixelCtx::simple(32, 32, 16, 16, [0.2, 0.3, 0.8, 1.0], &[("p1", 0.6), ("p2", 0.4)]);
            ctx.to_rgba = [0.9, 0.5, 0.1, 1.0];
            ctx.transition_progress = 0.5;
            let out = prog.eval_pixel(&ctx).into_rgba(1.0);
            assert!(out.iter().all(|v| v.is_finite()), "{id}: {out:?}");
        }
    }

    #[test]
    fn parses_resolve_gain_dctl_ui_params_and_evaluates_transform() {
        let src = r#"
DEFINE_UI_PARAMS(gainR, Red Gain, DCTLUI_SLIDER_FLOAT, 2.0, 0.0, 10.0, 0.1)
DEFINE_UI_PARAMS(gainG, Green Gain, DCTLUI_SLIDER_FLOAT, 1.5, 0.0, 10.0, 0.1)
DEFINE_UI_PARAMS(gainB, Blue Gain, DCTLUI_SLIDER_FLOAT, 0.5, 0.0, 10.0, 0.1)
DEFINE_UI_PARAMS(apply, Apply, DCTLUI_CHECK_BOX, 1)
DEFINE_UI_PARAMS(opt, Channel Option, DCTLUI_COMBO_BOX, 0, { ALL, RED, GREEN, BLUE }, { All Channels, Channel Red, Channel Green, Channel Blue })

__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B)
{
    if (apply)
    {
        p_R = p_R * (((opt == ALL) || (opt == RED))   ? gainR : 1.0f);
        p_G = p_G * (((opt == ALL) || (opt == GREEN)) ? gainG : 1.0f);
        p_B = p_B * (((opt == ALL) || (opt == BLUE))  ? gainB : 1.0f);
    }
    return make_float3(p_R, p_G, p_B);
}
"#;
        let prog = parse_dctl(src).unwrap();
        assert!(!prog.is_transition);
        assert_eq!(prog.ui_params.len(), 5);
        assert!(matches!(prog.ui_params[0].kind, DctlParamKind::Float { default: 2.0, .. }));
        let px = DctlPixelCtx::simple(1920, 1080, 10, 20, [0.25, 0.4, 0.8, 1.0], &[]);
        let rgb = prog.eval_pixel(&px).into_rgb([0.0; 3]);
        assert!((rgb[0] - 0.5).abs() < 1e-5);
        assert!((rgb[1] - 0.6).abs() < 1e-5);
        assert!((rgb[2] - 0.4).abs() < 1e-5);
    }

    #[test]
    fn supports_color_picker_helper_functions_alpha_dctl_and_spatial_tex2d() {
        // 1. ColorPicker + DEFINE_UI_TOOLTIP
        let picker_dctl = r#"
DEFINE_UI_PARAMS(tgtColor, Target Color, DCTLUI_COLOR_PICKER, 0.2f, 0.5f, 0.8f)
DEFINE_UI_TOOLTIP(Target Color, "Choose target color")
__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {
    return make_float3(tgtColor.r, tgtColor.g, tgtColor.b);
}
"#;
        let prog = parse_dctl(picker_dctl).expect("ColorPicker DCTL");
        let out = prog.eval_pixel(&DctlPixelCtx::simple(16, 16, 0, 0, [0.0; 4], &[])).into_rgb([0.0; 3]);
        assert_eq!(out, [0.2, 0.5, 0.8]);

        // 2. Helper `__DEVICE__` function + DEFINE_DCTL_ALPHA_MODE_STRAIGHT + float4 alpha output
        let alpha_dctl = r#"
DEFINE_DCTL_ALPHA_MODE_STRAIGHT
__DEVICE__ float mySmooth(float e0, float e1, float v) {
    v = _saturatef((v - e0) / (e1 - e0));
    return v * v * (3.0f - 2.0f * v);
}
__DEVICE__ float4 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B, float p_A) {
    const float dist = _hypotf(p_X - 8.0f, p_Y - 8.0f) / 16.0f;
    p_A = mySmooth(0.5f, 0.2f, dist);
    return make_float4(p_R, p_G, p_B, p_A);
}
"#;
        let aprog = parse_dctl(alpha_dctl).expect("Alpha helper DCTL");
        assert!(aprog.straight_alpha && aprog.returns_alpha);
        let center = aprog.eval_pixel(&DctlPixelCtx::simple(16, 16, 8, 8, [1.0, 0.5, 0.25, 1.0], &[])).into_rgba(1.0);
        let corner = aprog.eval_pixel(&DctlPixelCtx::simple(16, 16, 0, 0, [1.0, 0.5, 0.25, 1.0], &[])).into_rgba(1.0);
        assert!((center[3] - 1.0).abs() < 1e-5);
        assert!(corner[3] < 0.05);

        // 3. Spatial `_tex2D` offset sampling
        let tex_dctl = r#"
__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, __TEXTURE__ p_TexR, __TEXTURE__ p_TexG, __TEXTURE__ p_TexB) {
    float r = _tex2D(p_TexR, p_X + 3, p_Y);
    return make_float3(r, 0.0f, 0.0f);
}
"#;
        let tprog = parse_dctl(tex_dctl).expect("spatial _tex2D");
        assert!(tprog.uses_tex2d);
        let sampler = |sx: isize, _sy: isize| [sx as f32 * 0.1, 0.0, 0.0, 1.0];
        let mut ctx = DctlPixelCtx::simple(16, 16, 2, 0, [0.2, 0.0, 0.0, 1.0], &[]);
        ctx.sample_from = Some(&sampler);
        let tout = tprog.eval_pixel(&ctx).into_rgb([0.0; 3]);
        assert!((tout[0] - 0.5).abs() < 1e-5, "sampled x=5 -> 0.5, got {}", tout[0]);

        // 4. Unknown function call is rejected at parse time (no silent fallback)
        let bad_fn = "__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) { return make_float3(nonexistent_fn(p_R), p_G, p_B); }";
        assert!(parse_dctl(bad_fn).is_err());
    }

    #[test]
    fn builtin_dctls_parse_and_hostile_inputs_never_panic() {
        for (id, _, code) in builtin_dctls() {
            let prog = parse_dctl(code).unwrap_or_else(|e| panic!("{id}: {e}"));
            let px = DctlPixelCtx::simple(64, 64, 8, 8, [0.5, 0.4, 0.3, 1.0], &[("p1", 0.3), ("p2", 0.4), ("p3", 1.1)]);
            let out = prog.eval_pixel(&px).into_rgb([0.0; 3]);
            assert!(out.iter().all(|v| v.is_finite()), "{id}: {out:?}");
        }
        for hostile in
            ["", "DEFINE_UI_PARAMS(", "__DEVICE__ float3 transform(", "((((((((((((((((((((((((((((((((((((((1))))))))))))))))))))))))))))))))))))))"]
        {
            assert!(parse_dctl(hostile).is_err());
        }
    }
}
