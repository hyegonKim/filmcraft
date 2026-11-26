//! 3D Stage, SDF Clay Modeler, Surface Relight / PBR Sampler, and Procedural PBR descriptors,
//! plus a total, bounded Wavefront `.obj` mesh parser (`ObjMesh`).
//!
//! All untrusted `.obj` text is treated as hostile: allocations and counts are capped at
//! [`MAX_OBJ_VERTICES`] and [`MAX_OBJ_TRIANGLES`], non-finite coordinates are rejected, positions
//! are normalized into the unit bounding sphere `[-1, 1]^3`, and smooth area-weighted vertex
//! normals are synthesized whenever `vn` is absent.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Largest `.obj` text accepted by [`parse_obj`] (8 MiB).
pub const MAX_OBJ_BYTES: usize = 8 * 1024 * 1024;
/// Maximum raw or unified vertex count accepted into an [`ObjMesh`].
pub const MAX_OBJ_VERTICES: usize = 65_536;
/// Maximum triangle count accepted into an [`ObjMesh`].
pub const MAX_OBJ_TRIANGLES: usize = 65_536;
/// Maximum polygon corners read from a single `f` statement.
const MAX_FACE_CORNERS: usize = 64;

/// Choices for `"stage_3d"` parameter `"shape"`.
pub const STAGE_SHAPES: &[&str] =
    &["Sphere", "Rounded Box", "Bottle / Can", "Cylinder", "Torus", "SDF Clay Smooth Union", "SDF Clay Subtract", "SDF Clay Intersect", "Custom OBJ Mesh"];

/// Choices for `"stage_3d"` parameter `"material"`.
pub const STAGE_MATERIALS: &[&str] =
    &["Brushed Metal", "Polished Gold", "Frosted Glass", "Matte Plastic", "Ceramic", "Marble Stone", "Weathered Copper", "Custom Color PBR"];

/// Choices for `"surface_relight"` parameter `"output_mode"`.
pub const RELIGHT_OUTPUT_MODES: &[&str] = &["Relit Composite", "Delighted Albedo", "Tangent Normal Map", "Roughness Map", "Height Map", "Ambient Occlusion"];

/// Choices for `"procedural_pbr"` parameter `"pattern"`.
pub const PROCEDURAL_PBR_PATTERNS: &[&str] =
    &["Brushed Anisotropic Metal", "Carbon Hex Weave", "Terrazzo Marble", "Ceramic Tiles & Grout", "Weathered Rust Steel"];

/// Physical PBR material preset parameters (linear-light base reflectance, metallic, roughness,
/// transmission, and default weathering).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageMaterialSpec {
    pub name: &'static str,
    /// Display-sRGB base colour `[r, g, b]`.
    pub base_srgb: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub transmission: f32,
}

pub const STAGE_MATERIAL_SPECS: [StageMaterialSpec; 8] = [
    StageMaterialSpec { name: "Brushed Metal", base_srgb: [0.78, 0.80, 0.83], metallic: 0.92, roughness: 0.28, transmission: 0.0 },
    StageMaterialSpec { name: "Polished Gold", base_srgb: [1.00, 0.78, 0.34], metallic: 0.98, roughness: 0.12, transmission: 0.0 },
    StageMaterialSpec { name: "Frosted Glass", base_srgb: [0.90, 0.95, 0.99], metallic: 0.00, roughness: 0.22, transmission: 0.78 },
    StageMaterialSpec { name: "Matte Plastic", base_srgb: [0.24, 0.52, 0.86], metallic: 0.00, roughness: 0.62, transmission: 0.0 },
    StageMaterialSpec { name: "Ceramic", base_srgb: [0.94, 0.92, 0.88], metallic: 0.02, roughness: 0.14, transmission: 0.0 },
    StageMaterialSpec { name: "Marble Stone", base_srgb: [0.88, 0.87, 0.84], metallic: 0.00, roughness: 0.24, transmission: 0.0 },
    StageMaterialSpec { name: "Weathered Copper", base_srgb: [0.88, 0.48, 0.30], metallic: 0.88, roughness: 0.34, transmission: 0.0 },
    StageMaterialSpec { name: "Custom Color PBR", base_srgb: [0.85, 0.86, 0.88], metallic: 0.85, roughness: 0.25, transmission: 0.0 },
];

pub fn stage_material_spec(choice: u32) -> StageMaterialSpec {
    STAGE_MATERIAL_SPECS.get(choice as usize).copied().unwrap_or(STAGE_MATERIAL_SPECS[0])
}

/// Curated one-click workflow preset descriptor for `stage3d.applyPreset` and the `3D & Material Lab`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstancePreset {
    pub id: &'static str,
    pub label: &'static str,
    pub effect_id: &'static str,
    pub group: &'static str,
}

pub const SUBSTANCE_PRESETS: &[SubstancePreset] = &[
    SubstancePreset { id: "gold_sphere", label: "Polished Gold Sphere", effect_id: "stage_3d", group: "stage" },
    SubstancePreset { id: "bottle_mockup", label: "Product Bottle Mockup", effect_id: "stage_3d", group: "stage" },
    SubstancePreset { id: "sdf_clay_sculpture", label: "SDF Clay Sculpture", effect_id: "stage_3d", group: "stage" },
    SubstancePreset { id: "relight_warm_key", label: "3D Surface Relight", effect_id: "surface_relight", group: "sampler" },
    SubstancePreset { id: "pbr_normal_map", label: "Extract Normal Map", effect_id: "surface_relight", group: "sampler" },
    SubstancePreset { id: "seamless_albedo", label: "Make Seamless PBR", effect_id: "surface_relight", group: "sampler" },
    SubstancePreset { id: "brushed_steel", label: "Brushed Metal", effect_id: "procedural_pbr", group: "designer" },
    SubstancePreset { id: "terrazzo_marble", label: "Terrazzo Marble", effect_id: "procedural_pbr", group: "designer" },
    SubstancePreset { id: "carbon_hex_weave", label: "Carbon Hex Weave", effect_id: "procedural_pbr", group: "designer" },
];

pub fn find_substance_preset(id: &str) -> Option<&'static SubstancePreset> {
    let key = id.trim();
    SUBSTANCE_PRESETS.iter().find(|p| p.id.eq_ignore_ascii_case(key))
}

/// Bounded, normalized triangle mesh parsed from Wavefront `.obj` text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjMesh {
    /// Vertex positions normalized inside the unit sphere `[-1, 1]^3`.
    pub positions: Vec<[f32; 3]>,
    /// Unit-length vertex normals (`positions.len() == normals.len()`).
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates (`positions.len() == uvs.len()`).
    pub uvs: Vec<[f32; 2]>,
    /// Indexed triangle list referencing `0..positions.len()`.
    pub tris: Vec<[u32; 3]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ObjCornerKey {
    v: u32,
    vt: Option<u32>,
    vn: Option<u32>,
}

struct RawObj {
    pos: Vec<[f32; 3]>,
    uv: Vec<[f32; 2]>,
    nrm: Vec<[f32; 3]>,
}

fn parse_finite_f32(tok: Option<&str>) -> Option<f32> {
    let v: f32 = tok?.parse().ok()?;
    v.is_finite().then_some(v)
}

fn resolve_obj_index(raw: &str, count: usize) -> Option<u32> {
    if raw.is_empty() || count == 0 {
        return None;
    }
    let idx: i64 = raw.parse().ok()?;
    let resolved = if idx > 0 {
        usize::try_from(idx - 1).ok()?
    } else if idx < 0 {
        let back = usize::try_from(idx.checked_neg()?).ok()?;
        count.checked_sub(back)?
    } else {
        return None;
    };
    (resolved < count).then_some(resolved as u32)
}

fn parse_corner(token: &str, raw: &RawObj) -> Option<ObjCornerKey> {
    let mut parts = token.split('/');
    let v_str = parts.next()?;
    let v = resolve_obj_index(v_str, raw.pos.len())?;
    let vt = parts.next().and_then(|s| resolve_obj_index(s, raw.uv.len()));
    let vn = parts.next().and_then(|s| resolve_obj_index(s, raw.nrm.len()));
    Some(ObjCornerKey { v, vt, vn })
}

fn normalize_vec3(v: [f32; 3]) -> Option<[f32; 3]> {
    let len2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if !len2.is_finite() || len2 <= 1e-16 {
        return None;
    }
    let inv = 1.0 / len2.sqrt();
    Some([v[0] * inv, v[1] * inv, v[2] * inv])
}

fn normalize_unit_sphere(positions: &mut [[f32; 3]]) {
    if positions.is_empty() {
        return;
    }
    let mut min_p = [f32::INFINITY; 3];
    let mut max_p = [f32::NEG_INFINITY; 3];
    for p in positions.iter() {
        for k in 0..3 {
            min_p[k] = min_p[k].min(p[k]);
            max_p[k] = max_p[k].max(p[k]);
        }
    }
    let center = [0.5 * (min_p[0] + max_p[0]), 0.5 * (min_p[1] + max_p[1]), 0.5 * (min_p[2] + max_p[2])];
    let mut max_r2 = 0.0_f32;
    for p in positions.iter_mut() {
        p[0] -= center[0];
        p[1] -= center[1];
        p[2] -= center[2];
        let r2 = p[0] * p[0] + p[1] * p[1] + p[2] * p[2];
        if r2.is_finite() {
            max_r2 = max_r2.max(r2);
        }
    }
    let radius = max_r2.sqrt();
    if radius > 1e-6 && radius.is_finite() {
        let inv_r = 1.0 / radius;
        for p in positions.iter_mut() {
            p[0] = (p[0] * inv_r).clamp(-1.0, 1.0);
            p[1] = (p[1] * inv_r).clamp(-1.0, 1.0);
            p[2] = (p[2] * inv_r).clamp(-1.0, 1.0);
        }
    } else {
        for p in positions.iter_mut() {
            *p = [0.0, 0.0, 0.0];
        }
    }
}

fn finalize_normals(positions: &[[f32; 3]], tris: &[[u32; 3]], has_explicit: &[bool], normals: &mut [[f32; 3]]) {
    let mut accum = vec![[0.0_f32; 3]; positions.len()];
    for tri in tris {
        let (Some(&p0), Some(&p1), Some(&p2)) = (positions.get(tri[0] as usize), positions.get(tri[1] as usize), positions.get(tri[2] as usize)) else {
            continue;
        };
        let e1 = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let e2 = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let fnrm = [e1[1] * e2[2] - e1[2] * e2[1], e1[2] * e2[0] - e1[0] * e2[2], e1[0] * e2[1] - e1[1] * e2[0]];
        if !(fnrm[0].is_finite() && fnrm[1].is_finite() && fnrm[2].is_finite()) {
            continue;
        }
        for &vi in tri {
            if let Some(dst) = accum.get_mut(vi as usize) {
                dst[0] += fnrm[0];
                dst[1] += fnrm[1];
                dst[2] += fnrm[2];
            }
        }
    }
    for (i, n) in normals.iter_mut().enumerate() {
        let explicit_ok = has_explicit.get(i).copied().unwrap_or(false) && normalize_vec3(*n).map(|u| *n = u).is_some();
        if !explicit_ok {
            *n = accum.get(i).copied().and_then(normalize_vec3).unwrap_or([0.0, 1.0, 0.0]);
        }
    }
}

struct MeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    has_explicit_normal: Vec<bool>,
    tris: Vec<[u32; 3]>,
    dedup: HashMap<ObjCornerKey, u32>,
}

impl MeshBuilder {
    fn new() -> Self {
        Self { positions: Vec::new(), normals: Vec::new(), uvs: Vec::new(), has_explicit_normal: Vec::new(), tris: Vec::new(), dedup: HashMap::new() }
    }

    fn intern(&mut self, key: ObjCornerKey, raw: &RawObj) -> Option<u32> {
        if let Some(&idx) = self.dedup.get(&key) {
            return Some(idx);
        }
        if self.positions.len() >= MAX_OBJ_VERTICES {
            return None;
        }
        let pos = *raw.pos.get(key.v as usize)?;
        let uv = key.vt.and_then(|i| raw.uv.get(i as usize).copied()).unwrap_or([0.0, 0.0]);
        let (nrm, has_n) = match key.vn.and_then(|i| raw.nrm.get(i as usize).copied()).and_then(normalize_vec3) {
            Some(n) => (n, true),
            None => ([0.0, 1.0, 0.0], false),
        };
        let idx = self.positions.len() as u32;
        self.positions.push(pos);
        self.uvs.push(uv);
        self.normals.push(nrm);
        self.has_explicit_normal.push(has_n);
        self.dedup.insert(key, idx);
        Some(idx)
    }

    fn push_face(&mut self, mut words: std::str::SplitWhitespace<'_>, raw: &RawObj) {
        if self.tris.len() >= MAX_OBJ_TRIANGLES {
            return;
        }
        let mut corners: Vec<u32> = Vec::with_capacity(4);
        for _ in 0..MAX_FACE_CORNERS {
            let Some(tok) = words.next() else { break };
            let Some(key) = parse_corner(tok, raw) else { continue };
            let Some(vi) = self.intern(key, raw) else { break };
            corners.push(vi);
        }
        if corners.len() < 3 {
            return;
        }
        let c0 = corners[0];
        for pair in corners[1..].windows(2) {
            if self.tris.len() >= MAX_OBJ_TRIANGLES {
                break;
            }
            let (c1, c2) = (pair[0], pair[1]);
            if c0 != c1 && c1 != c2 && c2 != c0 {
                self.tris.push([c0, c1, c2]);
            }
        }
    }
}

/// Parse Wavefront `.obj` ASCII text into a bounded, unit-sphere-normalized [`ObjMesh`].
pub fn parse_obj(src: &str) -> Result<ObjMesh, String> {
    if src.len() > MAX_OBJ_BYTES {
        return Err(format!("OBJ mesh exceeds maximum size of {MAX_OBJ_BYTES} bytes"));
    }
    let mut raw = RawObj { pos: Vec::new(), uv: Vec::new(), nrm: Vec::new() };
    let mut builder = MeshBuilder::new();

    for raw_line in src.lines() {
        let line = match raw_line.split_once('#') {
            Some((before, _)) => before.trim(),
            None => raw_line.trim(),
        };
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(tag) = words.next() else { continue };
        match tag {
            "v" if raw.pos.len() < MAX_OBJ_VERTICES => {
                if let (Some(x), Some(y), Some(z)) = (parse_finite_f32(words.next()), parse_finite_f32(words.next()), parse_finite_f32(words.next())) {
                    let w = parse_finite_f32(words.next()).filter(|w| w.abs() > 1e-6).unwrap_or(1.0);
                    let p = [x / w, y / w, z / w];
                    if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
                        raw.pos.push(p);
                    }
                }
            }
            "vt" if raw.uv.len() < MAX_OBJ_VERTICES => {
                if let Some(u) = parse_finite_f32(words.next()) {
                    let v = parse_finite_f32(words.next()).unwrap_or(0.0);
                    raw.uv.push([u, v]);
                }
            }
            "vn" if raw.nrm.len() < MAX_OBJ_VERTICES => {
                if let (Some(nx), Some(ny), Some(nz)) = (parse_finite_f32(words.next()), parse_finite_f32(words.next()), parse_finite_f32(words.next())) {
                    raw.nrm.push([nx, ny, nz]);
                }
            }
            "f" => builder.push_face(words, &raw),
            _ => {}
        }
    }

    if builder.tris.is_empty() || builder.positions.is_empty() {
        return Err("OBJ mesh contains no valid triangles".into());
    }

    normalize_unit_sphere(&mut builder.positions);
    finalize_normals(&builder.positions, &builder.tris, &builder.has_explicit_normal, &mut builder.normals);

    Ok(ObjMesh { positions: builder.positions, normals: builder.normals, uvs: builder.uvs, tris: builder.tris })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectKind, find_effect};

    #[test]
    fn parse_obj_triangulates_quads_normalizes_bounds_and_synthesizes_smooth_normals() {
        let obj = r#"
            # Simple pyramid with a quad base and negative relative indices
            v -10.0  0.0 -10.0
            v  10.0  0.0 -10.0
            v  10.0  0.0  10.0
            v -10.0  0.0  10.0
            v   0.0 20.0   0.0
            vt 0.0 0.0
            vt 1.0 1.0
            f 1/1 2/2 3/1 4/2
            f -4 -3 -1
            f 2 3 5
            f 3 4 5
            f 4 1 5
        "#;
        let mesh = parse_obj(obj).expect("valid pyramid obj");
        assert_eq!(mesh.tris.len(), 6, "1 quad (2 tris) + 4 side tris");
        assert_eq!(mesh.positions.len(), mesh.normals.len());
        assert_eq!(mesh.positions.len(), mesh.uvs.len());

        let mut max_r = 0.0_f32;
        for p in &mesh.positions {
            for &c in p {
                assert!(c.is_finite() && (-1.0..=1.0).contains(&c), "out of unit bounds: {p:?}");
            }
            max_r = max_r.max((p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt());
        }
        assert!((max_r - 1.0).abs() < 1e-4, "normalized to unit bounding sphere, got {max_r}");

        for n in &mesh.normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "expected unit normal, got {n:?}");
        }
    }

    #[test]
    fn parse_obj_rejects_or_survives_hostile_inputs_without_panicking() {
        for bad_input in [
            "",
            "# comment only\no empty",
            "v NaN Infinity -Infinity\nf 1 2 3",
            "v 1 2 3\nf 0 -999999999 999999999",
            "v 0 0 0\nv 0 0 0\nv 0 0 0\nf 1//1 1//1 1//1",
            "f -9223372036854775808 1 2",
            "v 1e38 1e38 1e38 0.0\nf 1 1 1",
        ] {
            let res = std::panic::catch_unwind(|| parse_obj(bad_input));
            assert!(res.is_ok(), "panicked on {bad_input:?}");
        }
        assert!(parse_obj("").is_err());
        assert!(parse_obj("v 0 0 0\nv 1 0 0\nf 1 2").is_err());
    }

    #[test]
    fn three_d_and_material_effect_definitions_are_registered() {
        for (id, name, expected_params) in [
            (
                "stage_3d",
                "3D Stage Compositor",
                &[
                    "shape",
                    "mesh_path",
                    "material",
                    "base_color",
                    "metallic",
                    "roughness",
                    "weathering",
                    "sdf_blend",
                    "pos_x",
                    "pos_y",
                    "scale",
                    "rot_x",
                    "rot_y",
                    "rot_z",
                    "fov_deg",
                    "video_ibl",
                    "ibl_intensity",
                    "light_azimuth",
                    "light_elevation",
                    "shadow_catcher",
                    "ground_y",
                ][..],
            ),
            (
                "surface_relight",
                "Surface Relight & PBR Sampler",
                &[
                    "output_mode",
                    "delight",
                    "normal_strength",
                    "light_x",
                    "light_y",
                    "light_z",
                    "light_color",
                    "intensity",
                    "roughness",
                    "metallic",
                    "ao_strength",
                    "make_tileable",
                ][..],
            ),
            (
                "procedural_pbr",
                "Procedural PBR Material",
                &["pattern", "scale", "roughness", "metallic", "weathering", "light_angle", "blend_opacity", "animate_phase"][..],
            ),
        ] {
            let def = find_effect(id).unwrap_or_else(|| panic!("missing effect {id}"));
            assert_eq!(def.name, name);
            assert_eq!(def.kind, EffectKind::Video);
            assert_eq!(def.category, &["Video Effects", "3D & Material"]);
            assert!(def.accelerated && def.float32);
            for pid in expected_params {
                assert!(def.param(pid).is_some(), "{id} missing param {pid}");
            }
        }
    }
}
