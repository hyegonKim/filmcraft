//! Cross-platform Adobe Premiere Pro `.prm` & After Effects `.aex` C++ binary video effect host
//! for FilmCraft timeline evaluation (`"native_adobe_plugin"`).
//!
//! Executes 64-bit PE32+ (`x86_64` / `ARM64`) and Mach-O / Universal (`arm64` / `x86_64`) C++
//! plug-in binaries inside [`crate::native_vm::AePrmHost`] with zero JIT (`W^X` compliant on iOS,
//! Android, macOS, Linux, Windows, and `wasm32-unknown-unknown`) and caches initialized
//! `PF_Cmd_GLOBAL_SETUP` VM instances across timeline frames.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use filmcraft_color::{linear_to_srgb, srgb_to_linear};
use filmcraft_project::{DctlParamKind, DctlUiParam, EffectInstance, PluginKind, ProjectPlugin, decode_base64};
use serde::{Deserialize, Serialize};

use crate::effects::{FxCtx, f, text};
use crate::image::Image;
use crate::native_vm::{AeParamKind, AeParamSpec, AePrmHost, GuestArch, GuestOs, Limits, PluginFormat, RgbaFrame};

const MAX_CACHED_HOSTS: usize = 16;
const MAX_NATIVE_BINARY_BYTES: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostCacheKey {
    hash: u64,
    len: usize,
    format: PluginFormat,
}

type SharedHost = Arc<Mutex<AePrmHost>>;

fn host_cache() -> &'static Mutex<VecDeque<(HostCacheKey, SharedHost)>> {
    static CACHE: OnceLock<Mutex<VecDeque<(HostCacheKey, SharedHost)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_CACHED_HOSTS)))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn cached_host_for(bytes: &[u8], file_hint: Option<&str>) -> Result<SharedHost, String> {
    let fmt = file_hint.and_then(PluginFormat::from_extension).unwrap_or(PluginFormat::AfterEffectsAex);
    let key = HostCacheKey { hash: fnv1a64(bytes), len: bytes.len(), format: fmt };
    {
        let mut guard = host_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = guard.iter().position(|(k, _)| *k == key)
            && let Some(entry) = guard.remove(pos)
        {
            let host = Arc::clone(&entry.1);
            guard.push_back(entry);
            return Ok(host);
        }
    }
    let loaded = AePrmHost::from_bytes(bytes.to_vec(), file_hint, Limits::default()).map_err(|e| e.to_string())?;
    let shared = Arc::new(Mutex::new(loaded));
    let mut guard = host_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = guard.iter().position(|(k, _)| *k == key)
        && let Some(entry) = guard.remove(pos)
    {
        let host = Arc::clone(&entry.1);
        guard.push_back(entry);
        return Ok(host);
    }
    if guard.len() >= MAX_CACHED_HOSTS {
        let _ = guard.pop_front();
    }
    guard.push_back((key, Arc::clone(&shared)));
    Ok(shared)
}

/// Static + `PF_Cmd_PARAMS_SETUP` inspection summary for a `.prm` or `.aex` C++ binary plug-in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAdobePluginSummary {
    pub name: String,
    pub category: String,
    pub match_name: String,
    pub kind: PluginKind,
    pub arch: GuestArch,
    pub guest_os: GuestOs,
    pub entrypoint_symbol: String,
    pub ui_params: Vec<DctlUiParam>,
    pub ae_params: Vec<AeParamSpec>,
}

/// Validates and inspects a compiled `.prm` or `.aex` C++ binary plug-in on any target platform.
pub fn inspect_native_adobe_plugin(bytes: &[u8], file_hint: Option<&str>) -> Result<NativeAdobePluginSummary, String> {
    let host = AePrmHost::from_bytes(bytes.to_vec(), file_hint, Limits::default()).map_err(|e| e.to_string())?;
    let desc = host.descriptor();
    let fallback_name = file_hint
        .and_then(|h| Path::new(h).file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Adobe C++ Plug-in".to_string());
    let name = if !desc.pipl.name.trim().is_empty() { desc.pipl.name.trim().to_string() } else { fallback_name };
    let category = if !desc.pipl.category.trim().is_empty() { desc.pipl.category.trim().to_string() } else { "Adobe Plug-ins".to_string() };
    let kind = match desc.plugin_format {
        PluginFormat::PremierePrm => PluginKind::NativePremierePrm,
        _ => PluginKind::NativeAfterEffectsAex,
    };
    let ae_params = host.params().to_vec();
    let ui_params = ae_params.iter().enumerate().map(|(i, sp)| ae_spec_to_ui_param(i, sp)).collect();
    Ok(NativeAdobePluginSummary {
        name,
        category,
        match_name: desc.pipl.match_name.clone(),
        kind,
        arch: desc.arch,
        guest_os: desc.guest_os,
        entrypoint_symbol: desc.entrypoint_symbol.clone(),
        ui_params,
        ae_params,
    })
}

fn ae_spec_to_ui_param(idx: usize, sp: &AeParamSpec) -> DctlUiParam {
    let clean_ident: String =
        sp.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect::<String>().trim_matches('_').to_string();
    let name = if clean_ident.is_empty() { format!("p{}", idx + 1) } else { clean_ident };
    let label = if sp.name.trim().is_empty() { format!("Param {}", idx + 1) } else { sp.name.clone() };
    let kind = match &sp.kind {
        AeParamKind::Slider { default, min, max } => {
            let lo = if min.is_finite() { *min as f32 } else { 0.0 };
            let hi = if max.is_finite() && (*max as f32) > lo { *max as f32 } else { lo + 100.0 };
            let def = if default.is_finite() { (*default as f32).clamp(lo, hi) } else { lo };
            DctlParamKind::Float { default: def, min: lo, max: hi, step: 0.01 }
        }
        AeParamKind::Angle { default } => {
            let def = if default.is_finite() { *default as f32 } else { 0.0 };
            DctlParamKind::Float { default: def, min: -360.0, max: 360.0, step: 0.5 }
        }
        AeParamKind::Checkbox { default } => DctlParamKind::Bool { default: *default },
        AeParamKind::Popup { default, count } => {
            let n = (*count).clamp(1, 64) as usize;
            let ids: Vec<String> = (1..=n).map(|i| format!("OPT_{i}")).collect();
            let labels: Vec<String> = (1..=n).map(|i| format!("Option {i}")).collect();
            DctlParamKind::Combo { default: (*default as usize).min(n - 1), ids, labels }
        }
        AeParamKind::Color { default } => DctlParamKind::Color { default: [default[0] as f32, default[1] as f32, default[2] as f32] },
        AeParamKind::Point { default } => DctlParamKind::Float { default: default[0] as f32, min: -4096.0, max: 4096.0, step: 1.0 },
    };
    DctlUiParam { name, label, kind }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_bounded_disk_binary(path_str: &str) -> Option<Vec<u8>> {
    let path = Path::new(path_str);
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_NATIVE_BINARY_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

#[cfg(target_arch = "wasm32")]
fn read_bounded_disk_binary(_path_str: &str) -> Option<Vec<u8>> {
    None
}

fn resolve_plugin_bytes(plug: &ProjectPlugin) -> Option<(Vec<u8>, String)> {
    let default_hint = match plug.kind {
        PluginKind::NativePremierePrm => "plugin.prm",
        _ => "plugin.aex",
    };
    let hint = plug.source_path.clone().or_else(|| plug.binary_path.as_deref().map(str::to_string)).unwrap_or_else(|| default_hint.to_string());
    if let Some(b64) = plug.binary_base64.as_deref()
        && !b64.trim().is_empty()
        && let Ok(bytes) = decode_base64(b64)
    {
        return Some((bytes, hint));
    }
    if let Some(bpath) = plug.binary_path.as_deref().or(plug.source_path.as_deref())
        && let Some(bytes) = read_bounded_disk_binary(bpath)
    {
        return Some((bytes, hint));
    }
    None
}

fn resolve_effect_binary(e: &EffectInstance, cx: &FxCtx) -> Option<(Vec<u8>, String)> {
    let pref = text(e, "plugin_ref").trim();
    if pref.is_empty() {
        return None;
    }
    if let Some(id) = pref.strip_prefix("plugin:") {
        let plug = cx.project?.plugins.iter().find(|p| p.id == id)?;
        if !plug.enabled {
            return None;
        }
        return resolve_plugin_bytes(plug);
    }
    if let Some(b64) = pref.strip_prefix("base64:") {
        let bytes = decode_base64(b64).ok()?;
        return Some((bytes, "inline.aex".into()));
    }
    let bytes = read_bounded_disk_binary(pref)?;
    Some((bytes, pref.to_string()))
}

fn flatten_clip_params(e: &EffectInstance, cx: &FxCtx, specs: &[AeParamSpec]) -> Vec<f64> {
    const SLOTS: [&str; 8] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8"];
    let mut out = Vec::new();
    let mut slot_idx = 0usize;
    for spec in specs {
        match &spec.kind {
            AeParamKind::Slider { default, .. } | AeParamKind::Angle { default } => {
                let v = SLOTS.get(slot_idx).and_then(|&pid| e.param(pid).map(|_| e.f64_at(pid, cx.t))).unwrap_or(*default);
                slot_idx += 1;
                out.push(v);
            }
            AeParamKind::Checkbox { default } => {
                let v = SLOTS.get(slot_idx).and_then(|&pid| e.param(pid).map(|_| e.f64_at(pid, cx.t))).unwrap_or(if *default { 1.0 } else { 0.0 });
                slot_idx += 1;
                out.push(v);
            }
            AeParamKind::Popup { default, .. } => {
                let v = SLOTS.get(slot_idx).and_then(|&pid| e.param(pid).map(|_| e.f64_at(pid, cx.t))).unwrap_or(f64::from(*default));
                slot_idx += 1;
                out.push(v);
            }
            AeParamKind::Color { default } => {
                out.extend_from_slice(default);
            }
            AeParamKind::Point { default } => {
                let x = SLOTS.get(slot_idx).and_then(|&pid| e.param(pid).map(|_| e.f64_at(pid, cx.t))).unwrap_or(default[0]);
                let y = SLOTS.get(slot_idx + 1).and_then(|&pid| e.param(pid).map(|_| e.f64_at(pid, cx.t))).unwrap_or(default[1]);
                slot_idx += 2;
                out.push(x);
                out.push(y);
            }
        }
    }
    out
}

/// Applies `"native_adobe_plugin"` in-place on linear-light premultiplied `img`.
pub fn apply(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let mix = (f(e, "mix", cx) / 100.0).clamp(0.0, 1.0);
    if mix <= 0.0 || img.w == 0 || img.h == 0 {
        return;
    }
    let Some((bytes, hint)) = resolve_effect_binary(e, cx) else { return };
    let Ok(shared_host) = cached_host_for(&bytes, Some(&hint)) else { return };

    let mut in_pixels = Vec::with_capacity(img.w * img.h);
    for px in img.px.as_chunks::<4>().0 {
        let a = if px[3].is_finite() { px[3].clamp(0.0, 1.0) } else { 0.0 };
        if a > 1e-6 {
            in_pixels.push([
                linear_to_srgb((px[0] / a).max(0.0)).clamp(0.0, 1.0),
                linear_to_srgb((px[1] / a).max(0.0)).clamp(0.0, 1.0),
                linear_to_srgb((px[2] / a).max(0.0)).clamp(0.0, 1.0),
                a,
            ]);
        } else {
            in_pixels.push([0.0; 4]);
        }
    }
    let Ok(rgba_in) = RgbaFrame::new(img.w as u32, img.h as u32, in_pixels) else { return };

    let out_frame = {
        let mut guard = shared_host.lock().unwrap_or_else(|err| err.into_inner());
        let flat_params = flatten_clip_params(e, cx, guard.params());
        let Ok(rendered) = guard.render_reusing_setup(&rgba_in, &flat_params, cx.seconds, cx.frame_rate) else {
            return;
        };
        rendered
    };

    for (dst, out_px) in img.px.as_chunks_mut::<4>().0.iter_mut().zip(out_frame.pixels.iter()) {
        let orig = *dst;
        let out_a = if out_px[3].is_finite() { out_px[3].clamp(0.0, 1.0) } else { 0.0 };
        let lin_r = srgb_to_linear(if out_px[0].is_finite() { out_px[0].clamp(0.0, 1.0) } else { 0.0 }) * out_a;
        let lin_g = srgb_to_linear(if out_px[1].is_finite() { out_px[1].clamp(0.0, 1.0) } else { 0.0 }) * out_a;
        let lin_b = srgb_to_linear(if out_px[2].is_finite() { out_px[2].clamp(0.0, 1.0) } else { 0.0 }) * out_a;
        dst[0] = orig[0] * (1.0 - mix) + lin_r * mix;
        dst[1] = orig[1] * (1.0 - mix) + lin_g * mix;
        dst[2] = orig[2] * (1.0 - mix) + lin_b * mix;
        dst[3] = orig[3] * (1.0 - mix) + out_a * mix;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use filmcraft_color::WorkingSpace;
    use filmcraft_project::{Param, ParamValue, PluginKind, Project, ProjectPlugin, encode_base64, find_effect};
    use filmcraft_time::Tick;

    use super::{cached_host_for, inspect_native_adobe_plugin};
    use crate::effects::{FxCtx, apply};
    use crate::image::Image;

    fn build_test_pe64(export_name: &str, code: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 0x3000];
        img[0..2].copy_from_slice(b"MZ");
        img[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80usize;
        img[pe..pe + 4].copy_from_slice(b"PE\0\0");
        img[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes());
        img[pe + 6..pe + 8].copy_from_slice(&2u16.to_le_bytes());
        img[pe + 20..pe + 22].copy_from_slice(&0xF0u16.to_le_bytes());
        img[pe + 22..pe + 24].copy_from_slice(&0x2022u16.to_le_bytes());

        let opt = pe + 24;
        img[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes());
        img[opt + 24..opt + 32].copy_from_slice(&0x0000_0001_8000_0000u64.to_le_bytes());
        img[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        img[opt + 36..opt + 40].copy_from_slice(&0x0400u32.to_le_bytes());
        img[opt + 56..opt + 60].copy_from_slice(&0x4000u32.to_le_bytes());
        img[opt + 60..opt + 64].copy_from_slice(&0x0400u32.to_le_bytes());
        img[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let dd = opt + 112;
        img[dd..dd + 4].copy_from_slice(&0x2000u32.to_le_bytes());
        img[dd + 4..dd + 8].copy_from_slice(&0x200u32.to_le_bytes());

        let sh = opt + 0xF0;
        img[sh..sh + 5].copy_from_slice(b".text");
        img[sh + 8..sh + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sh + 12..sh + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sh + 16..sh + 20].copy_from_slice(&0x0800u32.to_le_bytes());
        img[sh + 20..sh + 24].copy_from_slice(&0x0400u32.to_le_bytes());

        let sh1 = sh + 40;
        img[sh1..sh1 + 6].copy_from_slice(b".edata");
        img[sh1 + 8..sh1 + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sh1 + 12..sh1 + 16].copy_from_slice(&0x2000u32.to_le_bytes());
        img[sh1 + 16..sh1 + 20].copy_from_slice(&0x0400u32.to_le_bytes());
        img[sh1 + 20..sh1 + 24].copy_from_slice(&0x0C00u32.to_le_bytes());

        img[0x0400..0x0400 + code.len()].copy_from_slice(code);

        let eo = 0x0C00usize;
        img[eo + 16..eo + 20].copy_from_slice(&1u32.to_le_bytes());
        img[eo + 20..eo + 24].copy_from_slice(&1u32.to_le_bytes());
        img[eo + 24..eo + 28].copy_from_slice(&1u32.to_le_bytes());
        img[eo + 28..eo + 32].copy_from_slice(&0x2040u32.to_le_bytes());
        img[eo + 32..eo + 36].copy_from_slice(&0x2044u32.to_le_bytes());
        img[eo + 36..eo + 40].copy_from_slice(&0x2048u32.to_le_bytes());
        img[eo + 0x40..eo + 0x44].copy_from_slice(&0x1000u32.to_le_bytes());
        img[eo + 0x44..eo + 0x48].copy_from_slice(&0x2050u32.to_le_bytes());
        img[eo + 0x48..eo + 0x4A].copy_from_slice(&0u16.to_le_bytes());
        let nb = export_name.as_bytes();
        img[eo + 0x50..eo + 0x50 + nb.len()].copy_from_slice(nb);
        img
    }

    fn red_boost_aex_bytes() -> Vec<u8> {
        let code: &[u8] = &[
            0x83, 0xF9, 0x02, // cmp ecx, 2 (PARAMS_SETUP)
            0x75, 0x53, // jne check_render
            0x48, 0x81, 0xEC, 0xD8, 0x00, 0x00, 0x00, // sub rsp, 0xD8
            0x57, // push rdi
            0x49, 0x89, 0xD2, // mov r10, rdx
            0x48, 0x8D, 0x7C, 0x24, 0x28, // lea rdi, [rsp+0x28]
            0x31, 0xC0, // xor eax, eax
            0xB9, 0xB0, 0x00, 0x00, 0x00, // mov ecx, 0xB0
            0xF3, 0xAA, // rep stosb
            0x5F, // pop rdi
            0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00, // uu_id = 1
            0xC7, 0x44, 0x24, 0x2C, 0x02, 0x00, 0x00, 0x00, // FIX_SLIDER
            0xC7, 0x44, 0x24, 0x58, 0x00, 0x80, 0x00, 0x00, // default 0.5
            0x49, 0x8B, 0x42, 0x10, // mov rax, [r10 + 0x10] (add_param)
            0x49, 0x8B, 0x8A, 0xB8, 0x00, 0x00, 0x00, // mov rcx, [r10 + 0xB8]
            0xBA, 0xFF, 0xFF, 0xFF, 0xFF, // mov edx, -1
            0x4C, 0x8D, 0x44, 0x24, 0x20, // lea r8, [rsp+0x20]
            0xFF, 0xD0, // call rax
            0x48, 0x81, 0xC4, 0xD8, 0x00, 0x00, 0x00, // add rsp, 0xD8
            0x31, 0xC0, // xor eax, eax
            0xC3, // ret
            // check_render:
            0x83, 0xF9, 0x0B, // cmp ecx, 11 (PF_Cmd_RENDER)
            0x75, 0x29, // jne ok_ret
            0x49, 0x8B, 0x01, // mov rax, [r9] (params[0])
            0x48, 0x8B, 0x40, 0x50, // mov rax, [rax + 0x50] (src data)
            0x49, 0x8B, 0x49, 0x08, // mov rcx, [r9 + 8] (params[1])
            0x8B, 0x49, 0x38, // mov ecx, [rcx + 0x38]
            0xC1, 0xF9, 0x08, // sar ecx, 8
            0x4C, 0x8B, 0x54, 0x24, 0x28, // mov r10, [rsp + 0x28]
            0x4D, 0x8B, 0x52, 0x18, // mov r10, [r10 + 0x18]
            0x8B, 0x10, // mov edx, [rax]
            0x41, 0x89, 0x12, // mov [r10], edx
            0x0F, 0xB6, 0x50, 0x01, // movzx edx, byte ptr [rax + 1]
            0x01, 0xCA, // add edx, ecx
            0x41, 0x88, 0x52, 0x01, // mov [r10 + 1], dl
            // ok_ret:
            0x31, 0xC0, // xor eax, eax
            0xC3, // ret
        ];
        build_test_pe64("EntryPointFunc", code)
    }

    #[test]
    fn native_prm_and_aex_execute_on_linear_images_with_cached_global_setup_and_mix() {
        let bin = red_boost_aex_bytes();
        let summary = inspect_native_adobe_plugin(&bin, Some("RedBoost.prm")).expect("inspect .prm");
        assert_eq!(summary.kind, PluginKind::NativePremierePrm);
        assert_eq!(summary.ui_params.len(), 1);

        let mut proj = Project::default();
        proj.plugins.push(ProjectPlugin {
            id: "prm1".into(),
            name: "RedBoost".into(),
            kind: PluginKind::NativePremierePrm,
            enabled: true,
            source_path: Some("RedBoost.prm".into()),
            binary_base64: Some(Arc::from(encode_base64(&bin))),
            binary_path: None,
            params: summary.ui_params.clone(),
            code: Arc::from(""),
        });

        let def = find_effect("native_adobe_plugin").expect("native_adobe_plugin registered");
        let mut inst = def.instance();
        inst.params.insert("plugin_ref".into(), Param::new(ParamValue::Text("plugin:prm1".into())));
        inst.params.insert("p1".into(), Param::new(ParamValue::Float(0.5)));

        let cx = FxCtx {
            t: Tick::ZERO,
            px_scale: 1.0,
            mask_scale: 1.0,
            seconds: 0.5,
            timecode: "00:00:00:12",
            clip_name: "Clip",
            project: Some(&proj),
            env: None,
            frame_rate: 24.0,
            working: WorkingSpace::Rec709,
        };

        // Render multiple frames and verify bounded cache reuses GLOBAL_SETUP (count == 1)
        let mut img_full = Image::filled(1, 1, [0.05, 0.1, 0.2, 1.0]);
        apply(&mut img_full, &inst, &cx);
        let mut img_second = Image::filled(1, 1, [0.05, 0.1, 0.2, 1.0]);
        apply(&mut img_second, &inst, &cx);
        assert!(img_full.px[0] > 0.45, "linear red boosted: {:?}", img_full.px);
        assert_eq!(img_full.px, img_second.px);

        let shared = cached_host_for(&bin, Some("RedBoost.prm")).expect("cached host");
        let setup_runs = shared.lock().unwrap_or_else(|e| e.into_inner()).global_setup_count();
        assert_eq!(setup_runs, 1, "cached AePrmHost ran GLOBAL_SETUP only once across frames");

        // Verify 50% mix blends between original and plugin output
        inst.params.insert("mix".into(), Param::new(ParamValue::Float(50.0)));
        let mut img_half = Image::filled(1, 1, [0.05, 0.1, 0.2, 1.0]);
        apply(&mut img_half, &inst, &cx);
        assert!(img_half.px[0] > 0.20 && img_half.px[0] < img_full.px[0], "50% mix: {:?}", img_half.px);
    }

    #[test]
    fn inspects_real_adobe_threshold_rgb_aex_if_installed() {
        for cand in [
            r"C:\Program Files\Adobe\Adobe After Effects 2026\Support Files\Plug-ins\Effects\CycoreFXHD\ThresholdRGB.aex",
            r"C:\Program Files\Adobe\Adobe After Effects 2027\Support Files\Plug-ins\Effects\CycoreFXHD\ThresholdRGB.aex",
        ] {
            let p = std::path::Path::new(cand);
            if p.is_file() {
                let bytes = std::fs::read(p).expect("read ThresholdRGB.aex");
                let sum = inspect_native_adobe_plugin(&bytes, Some("ThresholdRGB.aex")).expect("inspect ThresholdRGB.aex");
                assert_eq!(sum.kind, PluginKind::NativeAfterEffectsAex);
                assert!(!sum.name.is_empty());
                break;
            }
        }
    }
}
