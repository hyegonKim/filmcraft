//! Plugin manager, directory scanner (`.dctl`, `.ofx.bundle`, `.vst3`, `.ograf.json`,
//! `manifest.xml`), DCTL live validator/importer, and DaVinci Resolve-style Gallery PowerGrades &
//! serial color-node commands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use filmcraft_geom::Vec2;
use filmcraft_project::graphic::{new_shape_layer, new_text_layer};
use filmcraft_project::{
    ClipId, DctlParamKind, DctlPixelCtx, EffectInstance, MAX_PLUGIN_TEXT_BYTES, Param, ParamValue, PluginKind, PowerGradeStill, ProjectPlugin, TrackKind,
    builtin_dctls, find_effect, parse_dctl,
};
use filmcraft_time::{Tick, TimeDisplay, format_time};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, bool_p, clip_p, has_seq, str_p};
use crate::settings::PluginPrefs;
use crate::{EngineError, Result, Session};

pub const PLUGINS_DIR: &str = "Plugins";
const MAX_DISCOVERED_PLUGINS: usize = 512;

/// Metadata for a discovered external plugin bundle or shader file on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPlugin {
    pub id: String,
    pub name: String,
    pub kind: PluginKind,
    pub path: String,
    pub vendor: String,
}

/// Whether `kind` is enabled under the global category switches in `Preferences ▸ Plugins`.
pub fn is_kind_enabled_by_prefs(kind: PluginKind, prefs: &PluginPrefs) -> bool {
    match kind {
        PluginKind::OpenFxVideo | PluginKind::OpenFxTransition | PluginKind::MacroBundle => prefs.enable_openfx,
        PluginKind::DctlTransform | PluginKind::DctlTransition => prefs.enable_dctl,
        PluginKind::AudioVst3 => prefs.enable_audio_plugins,
        PluginKind::OgrafTitle | PluginKind::WorkflowScript => true,
    }
}

/// Canonical check for `disabled_plugins` matching both prefixed (`resolvefx:foo`, `plugin:dctl1`)
/// and bare (`foo`, `dctl1`) identifiers.
pub fn is_plugin_disabled(disabled: &[String], id: &str) -> bool {
    let bare = strip_plugin_prefix(id);
    disabled.iter().any(|d| d == id || strip_plugin_prefix(d) == bare)
}

fn strip_plugin_prefix(id: &str) -> &str {
    id.strip_prefix("resolvefx:").or_else(|| id.strip_prefix("plugin:")).unwrap_or(id)
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Session-owned plugin discovery cache.
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    pub dir: Option<PathBuf>,
    pub discovered: Vec<DiscoveredPlugin>,
}

impl PluginRegistry {
    pub fn set_dir(&mut self, data_dir: &Path, scan_system: bool, custom_dir: &str) {
        self.dir = Some(data_dir.to_path_buf());
        self.scan(scan_system, custom_dir);
    }

    pub fn scan(&mut self, scan_system: bool, custom_dir: &str) -> usize {
        let mut out = Vec::new();
        let mut roots: Vec<(PathBuf, &'static str)> = Vec::new();
        if let Some(d) = &self.dir {
            roots.push((d.join(PLUGINS_DIR), "User Plugins"));
        }
        let trimmed = custom_dir.trim();
        if !trimmed.is_empty() {
            roots.push((PathBuf::from(trimmed), "Custom Path"));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if scan_system {
            for (p, vendor) in system_plugin_roots() {
                if p.is_dir() {
                    roots.push((p, vendor));
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = scan_system;

        for (root, vendor) in roots {
            scan_dir(&root, vendor, 0, &mut out);
            if out.len() >= MAX_DISCOVERED_PLUGINS {
                break;
            }
        }
        out.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        out.dedup_by(|a, b| a.path == b.path);
        self.discovered = out;
        self.discovered.len()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn system_plugin_roots() -> Vec<(PathBuf, &'static str)> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    {
        v.push((PathBuf::from(r"C:\Program Files\Common Files\OFX\Plugins"), "System OpenFX"));
        v.push((PathBuf::from(r"C:\Program Files\Common Files\VST3"), "System VST3"));
        v.push((PathBuf::from(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\LUT"), "DaVinci LUT/DCTL"));
        v.push((PathBuf::from(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\Developer\DaVinciCTL"), "DaVinci DCTL SDK"));
        v.push((PathBuf::from(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\Developer\OGraf HTML Templates\Prebuilt"), "EBU OGraf"));
        v.push((PathBuf::from(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\Workflow Integration Plugins"), "Workflow Integration"));
    }
    #[cfg(target_os = "macos")]
    {
        v.push((PathBuf::from("/Library/OFX/Plugins"), "System OpenFX"));
        v.push((PathBuf::from("/Library/Audio/Plug-Ins/VST3"), "System VST3"));
        v.push((PathBuf::from("/Library/Application Support/Blackmagic Design/DaVinci Resolve/LUT"), "DaVinci LUT/DCTL"));
        v.push((PathBuf::from("/Library/Application Support/Blackmagic Design/DaVinci Resolve/Workflow Integration Plugins"), "Workflow Integration"));
    }
    #[cfg(target_os = "linux")]
    {
        v.push((PathBuf::from("/usr/OFX/Plugins"), "System OpenFX"));
        v.push((PathBuf::from("/usr/lib/vst3"), "System VST3"));
        v.push((PathBuf::from("/opt/resolve/LUT"), "DaVinci LUT/DCTL"));
    }
    v
}

fn scan_dir(dir: &Path, vendor: &str, depth: usize, out: &mut Vec<DiscoveredPlugin>) {
    if depth > 3 || out.len() >= MAX_DISCOVERED_PLUGINS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_DISCOVERED_PLUGINS {
            break;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        let lower = file_name.to_ascii_lowercase();
        if ft.is_dir() {
            if lower.ends_with(".ofx.bundle") {
                let stem = file_name.trim_end_matches(".ofx.bundle").trim_end_matches(".OFX.BUNDLE").to_string();
                push_discovered(out, PluginKind::OpenFxVideo, &stem, &path, vendor);
            } else if lower.ends_with(".vst3") {
                let stem = file_name[..file_name.len().saturating_sub(5)].to_string();
                push_discovered(out, PluginKind::AudioVst3, &stem, &path, vendor);
            } else {
                scan_dir(&path, vendor, depth + 1, out);
            }
        } else if ft.is_file() {
            if lower.ends_with(".dctl") {
                let stem = path.file_stem().map_or_else(|| file_name.clone(), |s| s.to_string_lossy().to_string());
                let kind = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| parse_dctl(&s).ok())
                    .map_or(PluginKind::DctlTransform, |p| if p.is_transition { PluginKind::DctlTransition } else { PluginKind::DctlTransform });
                push_discovered(out, kind, &stem, &path, vendor);
            } else if lower.ends_with(".vst3") {
                let stem = path.file_stem().map_or_else(|| file_name.clone(), |s| s.to_string_lossy().to_string());
                push_discovered(out, PluginKind::AudioVst3, &stem, &path, vendor);
            } else if lower.ends_with(".ograf") || lower.ends_with(".ograf.json") {
                let stem = file_name.trim_end_matches(".ograf.json").trim_end_matches(".ograf").to_string();
                push_discovered(out, PluginKind::OgrafTitle, &stem, &path, vendor);
            } else if lower.ends_with(".drfx") || lower.ends_with(".setting") {
                let stem = path.file_stem().map_or_else(|| file_name.clone(), |s| s.to_string_lossy().to_string());
                push_discovered(out, PluginKind::MacroBundle, &stem, &path, vendor);
            } else if lower == "manifest.xml"
                && let Some(parent_name) = path.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().to_string())
            {
                push_discovered(out, PluginKind::WorkflowScript, &parent_name, &path, vendor);
            }
        }
    }
}

fn push_discovered(out: &mut Vec<DiscoveredPlugin>, kind: PluginKind, name: &str, path: &Path, vendor: &str) {
    let display = path.to_string_lossy().to_string();
    let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    let hash = fnv1a32(display.as_bytes());
    out.push(DiscoveredPlugin {
        id: format!("ext:{}:{slug}:{hash:08x}", kind.badge().to_ascii_lowercase()),
        name: name.to_string(),
        kind,
        path: display,
        vendor: vendor.to_string(),
    });
}

// ---------------------------------------------------------------------------
// Engine commands (`plugins.*` and Color-page `color.*` PowerGrades / Nodes)
// ---------------------------------------------------------------------------

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal: true, named: NamedKeys::NONE }
}

fn query(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal: false, named: NamedKeys::NONE }
}

const COLOR_NODE_EFFECTS: &[&str] = &["lumetri", "color_space_transform", "film_halation", "color_warper", "qualifier_3d", "dctl_transform", "surface_relight"];

pub fn is_color_node_effect(id: &str) -> bool {
    COLOR_NODE_EFFECTS.contains(&id)
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        query("plugins.list", "List Plugins", "{}", list_plugins),
        query("plugins.scan", "Scan Plugin Folders", "{}", scan_plugins),
        query("plugins.validateDctl", "Validate DCTL Shader", r#"{"code":str}"#, validate_dctl),
        spec("plugins.importDctl", "Import DCTL Plugin…", &[], r#"{"path":str?,"code":str?,"name":str?,"apply":bool?,"clips":[id]?}"#, always, import_dctl),
        spec("plugins.importDrfx", "Import DRFX Bundle…", &[], r#"{"path":str}"#, always, import_drfx),
        spec("plugins.importOgraf", "Import OGraf Broadcast Graphic…", &[], r#"{"path":str?,"json":str?,"name":str?,"seconds":n?}"#, has_seq, import_ograf),
        spec("plugins.remove", "Remove Project Plugin", &[], r#"{"id":str}"#, always, remove_plugin),
        spec("plugins.setEnabled", "Enable or Disable Plugin", &[], r#"{"id":str,"enabled":bool}"#, always, set_plugin_enabled),
        spec("color.grabStill", "Grab PowerGrade Still", &["Clip"], r#"{"clip":id?,"name":str?,"notes":str?}"#, has_seq, grab_still),
        query("color.listStills", "List PowerGrade Stills", "{}", list_stills),
        spec("color.applyStill", "Apply PowerGrade Still", &[], r#"{"id":n,"clips":[id]?,"append":bool?}"#, has_seq, apply_still),
        spec("color.deleteStill", "Delete PowerGrade Still", &[], r#"{"id":n}"#, always, delete_still),
        spec("color.addSerialNode", "Add Serial Color Node", &["Clip"], r#"{"clip":id?,"effect":str?}"#, has_seq, add_serial_node),
    ]
}

fn builtin_plugin_rows(prefs: &PluginPrefs) -> Vec<Value> {
    let disabled = &prefs.disabled_plugins;
    let mut rows = Vec::new();
    for (fx_id, name, badge, category) in [
        ("color_space_transform", "Color Space Transform (CST)", "ResolveFX Color", PluginKind::OpenFxVideo),
        ("film_halation", "Film Halation", "ResolveFX Stylize", PluginKind::OpenFxVideo),
        ("color_warper", "Color Warper (Spider-Web)", "ResolveFX Color", PluginKind::OpenFxVideo),
        ("qualifier_3d", "3D HSL Qualifier (Inside/Outside)", "ResolveFX Color", PluginKind::OpenFxVideo),
        ("dctl_transform", "DCTL Color Transform Host", "ResolveFX DCTL", PluginKind::DctlTransform),
        ("dynamic_zoom", "Dynamic Zoom (Green/Red Frame)", "Resolve Cut/Edit", PluginKind::OpenFxVideo),
        ("dialogue_leveler", "Fairlight Dialogue Leveler", "Fairlight FX", PluginKind::AudioVst3),
        ("stage_3d", "3D Stage Compositor (SDF & OBJ)", "3D & Material", PluginKind::OpenFxVideo),
        ("surface_relight", "Surface Relight & PBR Sampler", "3D & Material", PluginKind::OpenFxVideo),
        ("procedural_pbr", "Procedural PBR Material", "3D & Material", PluginKind::OpenFxVideo),
    ] {
        let full_id = format!("resolvefx:{fx_id}");
        rows.push(json!({
            "id": full_id,
            "effect": fx_id,
            "name": name,
            "vendor": badge,
            "kind": category,
            "kindLabel": category.label(),
            "badge": category.badge(),
            "builtin": true,
            "enabled": is_kind_enabled_by_prefs(category, prefs) && !is_plugin_disabled(disabled, &full_id),
        }));
    }
    for (id, name, _) in builtin_dctls() {
        let full_id = format!("builtin:{id}");
        rows.push(json!({
            "id": full_id,
            "name": name,
            "vendor": "FilmCraft DCTL",
            "kind": PluginKind::DctlTransform,
            "kindLabel": PluginKind::DctlTransform.label(),
            "badge": "DCTL",
            "builtin": true,
            "enabled": prefs.enable_dctl && !is_plugin_disabled(disabled, &full_id),
        }));
    }
    rows
}

fn list_plugins(s: &mut Session, _: &Value) -> Result<Value> {
    let prefs = &s.prefs.plugins;
    let disabled = &prefs.disabled_plugins;
    let builtin = builtin_plugin_rows(prefs);
    let project: Vec<Value> = s
        .project
        .plugins
        .iter()
        .map(|p| {
            let full_id = format!("plugin:{}", p.id);
            json!({
                "id": p.id,
                "ref": full_id,
                "name": p.name,
                "kind": p.kind,
                "kindLabel": p.kind.label(),
                "badge": p.kind.badge(),
                "source": p.source_path,
                "paramCount": p.params.len(),
                "enabled": p.enabled && is_kind_enabled_by_prefs(p.kind, prefs) && !is_plugin_disabled(disabled, &full_id),
            })
        })
        .collect();
    let discovered: Vec<Value> = s
        .plugins
        .discovered
        .iter()
        .filter(|d| is_kind_enabled_by_prefs(d.kind, prefs))
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "kind": d.kind,
                "kindLabel": d.kind.label(),
                "badge": d.kind.badge(),
                "path": d.path,
                "vendor": d.vendor,
                "enabled": !is_plugin_disabled(disabled, &d.id),
            })
        })
        .collect();
    Ok(json!({
        "builtin": builtin,
        "project": project,
        "discovered": discovered,
        "disabled": disabled,
        "developerMode": prefs.developer_mode,
    }))
}

fn scan_plugins(s: &mut Session, _: &Value) -> Result<Value> {
    let scan_sys = s.prefs.plugins.scan_system_folders;
    let custom = s.prefs.plugins.custom_search_path.clone();
    let _ = s.plugins.scan(scan_sys, &custom);
    let prefs = &s.prefs.plugins;
    let visible = s.plugins.discovered.iter().filter(|d| is_kind_enabled_by_prefs(d.kind, prefs)).count();
    Ok(json!({ "discovered": visible }))
}

fn validate_dctl(_: &mut Session, p: &Value) -> Result<Value> {
    let code = str_p(p, "code").ok_or_else(|| bad("plugins.validateDctl", "need `code`"))?;
    match parse_dctl(code) {
        Ok(prog) => {
            let sample = prog.eval_pixel(&DctlPixelCtx::simple(1920, 1080, 960, 540, [0.5, 0.4, 0.3, 1.0], &[])).into_rgb([0.5, 0.4, 0.3]);
            Ok(json!({
                "valid": true,
                "isTransition": prog.is_transition,
                "returnsAlpha": prog.returns_alpha,
                "straightAlpha": prog.straight_alpha,
                "usesTex2d": prog.uses_tex2d,
                "params": prog.ui_params,
                "sampleRgb": sample,
            }))
        }
        Err(err) => Ok(json!({
            "valid": false,
            "error": err,
        })),
    }
}

fn target_video_clips(s: &Session, p: &Value, cmd: &str) -> Result<Vec<ClipId>> {
    let seq = s.active_sequence().ok_or_else(|| bad(cmd, "no sequence is open"))?;
    let raw: Vec<ClipId> = if let Some(arr) = p.get("clips").and_then(Value::as_array) {
        let mut ids = Vec::with_capacity(arr.len());
        for v in arr {
            let id = v.as_u64().ok_or_else(|| bad(cmd, "`clips` entries must be clip ids"))?;
            ids.push(ClipId(id));
        }
        ids
    } else if let Some(c) = clip_p(p, "clip") {
        vec![c]
    } else {
        s.state.selection.clone()
    };
    let vids: Vec<ClipId> =
        raw.into_iter().filter(|c| seq.find_item(*c).and_then(|(tid, _)| seq.track(tid)).is_some_and(|t| t.kind == TrackKind::Video)).collect();
    if vids.is_empty() {
        return Err(bad(cmd, "select at least one video clip"));
    }
    Ok(vids)
}

fn dctl_param_default_scalar(kind: &DctlParamKind) -> Option<f64> {
    match kind {
        DctlParamKind::Float { default, .. } => Some(f64::from(*default)),
        DctlParamKind::Int { default, .. } => Some(f64::from(*default)),
        DctlParamKind::Bool { default } => Some(if *default { 1.0 } else { 0.0 }),
        DctlParamKind::Combo { default, .. } => Some(*default as f64),
        DctlParamKind::Color { .. } => None,
    }
}

fn import_dctl(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "plugins.importDctl";
    if !s.prefs.plugins.enable_dctl {
        return Err(EngineError::Disabled(CMD.into(), "DCTL plugins are disabled in Preferences ▸ Plugins".into()));
    }
    let path_opt = str_p(p, "path").map(str::to_string);
    let code = match (str_p(p, "code"), path_opt.as_deref()) {
        (Some(inline), _) => inline.to_string(),
        (None, Some(path)) => s
            .services
            .read_file(path)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| bad(CMD, format!("could not read UTF-8 DCTL file `{path}`")))?,
        (None, None) => return Err(bad(CMD, "supply `path` or `code`")),
    };
    if code.len() > MAX_PLUGIN_TEXT_BYTES {
        return Err(bad(CMD, format!("DCTL exceeds {MAX_PLUGIN_TEXT_BYTES} bytes")));
    }
    let prog = parse_dctl(&code).map_err(|e| bad(CMD, e))?;
    let default_name =
        path_opt.as_deref().and_then(|pt| Path::new(pt).file_stem()).map_or_else(|| "Custom DCTL".to_string(), |st| st.to_string_lossy().to_string());
    let name = str_p(p, "name").map_or(default_name, str::to_string);
    let kind = if prog.is_transition { PluginKind::DctlTransition } else { PluginKind::DctlTransform };
    let apply_now = bool_p(p, "apply").unwrap_or(false) || p.get("clips").is_some();
    let targets = if apply_now && s.active_sequence().is_some() { target_video_clips(s, p, CMD).unwrap_or_default() } else { Vec::new() };
    let active_seq = s.state.active_sequence;

    let mut id = String::new();
    s.edit("Import DCTL Plugin", |pr, _| {
        if let Some(existing) = pr.plugins.iter().find(|pl| *pl.code == *code) {
            id = existing.id.clone();
        } else {
            id = format!("dctl{}", pr.alloc_id());
            pr.plugins.push(ProjectPlugin {
                id: id.clone(),
                name: name.clone(),
                kind,
                source_path: path_opt.clone(),
                enabled: true,
                params: prog.ui_params.clone(),
                code: Arc::from(code.as_str()),
            });
        }
        if !targets.is_empty()
            && let Some(seq_id) = active_seq
            && let Some(seq) = pr.sequence_mut(seq_id)
            && let Some(def) = find_effect("dctl_transform")
        {
            for cid in &targets {
                if let Some((_, item)) = seq.find_item_mut(*cid) {
                    let mut inst = def.instance();
                    inst.params.insert("plugin_ref".into(), Param::new(ParamValue::Text(format!("plugin:{id}"))));
                    inst.params.insert("mode".into(), Param::new(ParamValue::Choice(0)));
                    for (slot_i, up) in prog.ui_params.iter().take(4).enumerate() {
                        let pid = ["p1", "p2", "p3", "p4"][slot_i];
                        if let Some(val) = dctl_param_default_scalar(&up.kind) {
                            inst.params.insert(pid.into(), Param::new(ParamValue::Float(val)));
                        }
                    }
                    let insert_at = item.effects.iter().position(|ef| ef.def().is_some_and(|d| d.intrinsic)).unwrap_or(item.effects.len());
                    item.effects.insert(insert_at, inst);
                }
            }
        }
        Ok(())
    })?;
    Ok(json!({
        "id": id,
        "ref": format!("plugin:{id}"),
        "name": name,
        "kind": kind,
        "appliedClips": targets.len(),
    }))
}

fn import_drfx(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "plugins.importDrfx";
    if !s.prefs.plugins.enable_dctl && !s.prefs.plugins.enable_openfx {
        return Err(EngineError::Disabled(CMD.into(), "OpenFX / DCTL bundles are disabled in Preferences ▸ Plugins".into()));
    }
    let path = str_p(p, "path").ok_or_else(|| bad(CMD, "need `path` to `.drfx` bundle"))?;
    let bytes = s.services.read_file(path).map_err(|e| bad(CMD, format!("could not read `{path}`: {e}")))?;
    let archive_entries = crate::drfx_zip::read_zip_entries(&bytes).map_err(|e| bad(CMD, e))?;

    let mut entries: Vec<String> = Vec::with_capacity(archive_entries.len());
    let mut parsed_dctls: Vec<(String, PluginKind, Vec<filmcraft_project::DctlUiParam>, String)> = Vec::new();
    for entry in archive_entries {
        entries.push(entry.name.clone());
        let file_name = entry.name.rsplit('/').next().unwrap_or(&entry.name);
        if entry.name.starts_with("__MACOSX/") || file_name.starts_with("._") {
            continue;
        }
        if file_name.to_ascii_lowercase().ends_with(".dctl")
            && entry.data.len() <= MAX_PLUGIN_TEXT_BYTES
            && s.prefs.plugins.enable_dctl
            && let Ok(code) = String::from_utf8(entry.data)
            && let Ok(prog) = parse_dctl(&code)
        {
            let stem = Path::new(file_name).file_stem().map_or_else(|| "DRFX DCTL".to_string(), |st| st.to_string_lossy().to_string());
            let kind = if prog.is_transition { PluginKind::DctlTransition } else { PluginKind::DctlTransform };
            parsed_dctls.push((stem, kind, prog.ui_params, code));
        }
    }

    let mut imported_ids: Vec<String> = Vec::new();
    if !parsed_dctls.is_empty() {
        let src_path = path.to_string();
        s.edit("Import DRFX Bundle", |pr, _| {
            for (stem, kind, params, code) in &parsed_dctls {
                if let Some(existing) = pr.plugins.iter().find(|pl| *pl.code == *code) {
                    imported_ids.push(existing.id.clone());
                } else {
                    let id = format!("dctl{}", pr.alloc_id());
                    pr.plugins.push(ProjectPlugin {
                        id: id.clone(),
                        name: stem.clone(),
                        kind: *kind,
                        source_path: Some(src_path.clone()),
                        enabled: true,
                        params: params.clone(),
                        code: Arc::from(code.as_str()),
                    });
                    imported_ids.push(id);
                }
            }
            Ok(())
        })?;
    }

    Ok(json!({
        "importedDctls": imported_ids.len(),
        "pluginIds": imported_ids,
        "entries": entries,
    }))
}

fn extract_ograf_fields(manifest: &Value) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let Some(props) = manifest.get("schema").and_then(|s| s.get("properties")).and_then(Value::as_object) else {
        return fields;
    };
    for (key, spec) in props {
        let label = spec.get("title").and_then(Value::as_str).unwrap_or(key).trim().to_string();
        let val = match spec.get("default") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            _ => match spec.get("examples").and_then(Value::as_array).and_then(|a| a.first()) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            },
        };
        fields.push((label, val));
    }
    fields
}

fn import_ograf(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "plugins.importOgraf";
    let path_opt = str_p(p, "path").map(str::to_string);
    let raw = match (str_p(p, "json"), path_opt.as_deref()) {
        (Some(inline), _) => inline.to_string(),
        (None, Some(path)) => {
            let bytes = s.services.read_file(path).map_err(|e| bad(CMD, format!("could not read `{path}`: {e}")))?;
            if bytes.len() > MAX_PLUGIN_TEXT_BYTES {
                return Err(bad(CMD, format!("OGraf manifest exceeds {MAX_PLUGIN_TEXT_BYTES} bytes")));
            }
            String::from_utf8(bytes).map_err(|_| bad(CMD, "OGraf manifest must be valid UTF-8"))?
        }
        (None, None) => return Err(bad(CMD, "supply `path` or `json`")),
    };
    if raw.len() > MAX_PLUGIN_TEXT_BYTES {
        return Err(bad(CMD, format!("OGraf manifest exceeds {MAX_PLUGIN_TEXT_BYTES} bytes")));
    }
    let manifest: Value = serde_json::from_str(&raw).map_err(|e| bad(CMD, format!("invalid OGraf JSON: {e}")))?;
    let fallback_name = path_opt
        .as_deref()
        .and_then(|pt| Path::new(pt).file_name())
        .map(|f| f.to_string_lossy().trim_end_matches(".ograf.json").trim_end_matches(".ograf").to_string())
        .unwrap_or_else(|| "OGraf Graphic".to_string());
    let name = str_p(p, "name").or_else(|| manifest.get("name").and_then(Value::as_str)).map_or(fallback_name, str::to_string);
    let fields = extract_ograf_fields(&manifest);

    let non_empty: Vec<String> = fields
        .iter()
        .filter_map(|(lbl, val)| if val.trim().is_empty() { None } else { Some(val.trim().to_string()).or_else(|| Some(lbl.clone())) })
        .collect();
    let headline = non_empty.first().cloned().unwrap_or_else(|| name.clone());
    let subtitle = non_empty.get(1).cloned().or_else(|| manifest.get("description").and_then(Value::as_str).map(str::to_string));

    let (fw, fh) = s.active_sequence().map_or((1920.0, 1080.0), |q| (f64::from(q.settings.width.max(16)), f64::from(q.settings.height.max(16))));
    let mut card = new_shape_layer(0, Vec2::new(fw * 0.5, fh * 0.82), Vec2::new(fw * 0.64, fh * 0.14), Vec::new());
    card.params.insert("name".into(), Param::new(ParamValue::Text("OGraf Strap".into())));
    card.params.insert("fill_color".into(), Param::new(ParamValue::Color([0.08, 0.10, 0.14, 0.88])));
    card.params.insert("corner_radius".into(), Param::new(ParamValue::Float(8.0)));

    let mut title_layer = new_text_layer(&headline, Vec2::new(fw * 0.5, fh * if subtitle.is_some() { 0.795 } else { 0.82 }), (fh * 0.045).clamp(20.0, 96.0));
    title_layer.params.insert("name".into(), Param::new(ParamValue::Text("OGraf Headline".into())));
    title_layer.params.insert("align".into(), Param::new(ParamValue::Choice(1)));

    let mut layers = vec![card, title_layer];
    if let Some(sub) = subtitle.filter(|st| !st.trim().is_empty()) {
        let mut sub_layer = new_text_layer(&sub, Vec2::new(fw * 0.5, fh * 0.855), (fh * 0.028).clamp(14.0, 60.0));
        sub_layer.params.insert("name".into(), Param::new(ParamValue::Text("OGraf Subtitle".into())));
        sub_layer.params.insert("align".into(), Param::new(ParamValue::Choice(1)));
        sub_layer.params.insert("fill_color".into(), Param::new(ParamValue::Color([0.82, 0.86, 0.92, 1.0])));
        layers.push(sub_layer);
    }

    let seconds = p.get("seconds").and_then(Value::as_f64).unwrap_or(5.0).clamp(0.1, 3600.0);
    let mut placement = p.clone();
    if let Some(obj) = placement.as_object_mut() {
        obj.remove("seconds");
    }
    let cid = crate::graphics::place_video_clip(s, CMD, &name, &placement, "Import OGraf Graphic", layers, move |pr, (w, h, rate)| {
        let src = crate::graphics::graphic_source(pr, w, h, rate);
        (src, rate.snap_nearest(Tick::from_seconds_f64(seconds)).max(rate.frame_duration()))
    })?;

    let field_json: Vec<Value> = fields.into_iter().map(|(k, v)| json!({ "name": k, "default": v })).collect();
    Ok(json!({
        "clip": cid.0,
        "name": name,
        "fields": field_json,
    }))
}

fn remove_plugin(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "plugins.remove";
    let id = str_p(p, "id").ok_or_else(|| bad(CMD, "need `id`"))?.trim_start_matches("plugin:").to_string();
    if !s.project.plugins.iter().any(|pl| pl.id == id) {
        return Err(bad(CMD, format!("no project plugin `{id}`")));
    }
    s.edit("Remove Project Plugin", |pr, _| {
        pr.plugins.retain(|pl| pl.id != id);
        Ok(())
    })?;
    Ok(Value::Null)
}

fn set_plugin_enabled(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "plugins.setEnabled";
    let id = str_p(p, "id").ok_or_else(|| bad(CMD, "need `id`"))?.to_string();
    let enabled = bool_p(p, "enabled").ok_or_else(|| bad(CMD, "need `enabled`"))?;
    let bare = strip_plugin_prefix(&id).to_string();
    s.prefs.plugins.disabled_plugins.retain(|x| x != &id && strip_plugin_prefix(x) != bare);
    if !enabled {
        s.prefs.plugins.disabled_plugins.push(id.clone());
    }
    s.save_prefs();
    if s.project.plugins.iter().any(|pl| pl.id == bare) {
        s.edit("Toggle Project Plugin", |pr, _| {
            for pl in &mut pr.plugins {
                if pl.id == bare {
                    pl.enabled = enabled;
                }
            }
            Ok(())
        })?;
    }
    Ok(json!({ "id": id, "enabled": enabled }))
}

fn grab_still(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "color.grabStill";
    let clips = target_video_clips(s, p, CMD)?;
    let first = *clips.first().ok_or_else(|| bad(CMD, "no video clip selected"))?;
    let seq = s.active_sequence().ok_or_else(|| bad(CMD, "no active sequence"))?;
    let (_, item) = seq.find_item(first).ok_or_else(|| bad(CMD, "clip not found"))?;
    let nodes: Vec<EffectInstance> = item.effects.iter().filter(|e| is_color_node_effect(&e.effect)).cloned().collect();
    if nodes.is_empty() {
        return Err(bad(CMD, "the selected clip has no color or ResolveFX nodes to grab"));
    }
    let ph = s.playhead();
    let tc = format_time(ph, seq.settings.frame_rate, seq.settings.drop_frame, TimeDisplay::Timecode, seq.settings.sample_rate as i64);
    let default_name = format!("Still {} · {}", s.project.power_grades.len() + 1, item.name);
    let name = str_p(p, "name").map_or(default_name, str::to_string);
    let notes = str_p(p, "notes").unwrap_or("").to_string();
    let mut still_id = 0u64;
    s.edit("Grab PowerGrade Still", |pr, _| {
        still_id = pr.alloc_id();
        pr.power_grades.push(PowerGradeStill { id: still_id, name: name.clone(), timecode: tc.clone(), notes: notes.clone(), effects: nodes.clone() });
        Ok(())
    })?;
    Ok(json!({ "id": still_id, "name": name, "timecode": tc, "nodeCount": nodes.len() }))
}

fn list_stills(s: &mut Session, _: &Value) -> Result<Value> {
    let list: Vec<Value> = s
        .project
        .power_grades
        .iter()
        .map(|pg| {
            let fx_names: Vec<&str> = pg.effects.iter().map(|e| e.effect.as_str()).collect();
            json!({
                "id": pg.id,
                "name": pg.name,
                "timecode": pg.timecode,
                "notes": pg.notes,
                "nodeCount": pg.effects.len(),
                "nodes": fx_names,
            })
        })
        .collect();
    Ok(json!({ "stills": list }))
}

fn apply_still(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "color.applyStill";
    let id = p.get("id").and_then(Value::as_u64).ok_or_else(|| bad(CMD, "need still `id`"))?;
    let still = s.project.power_grades.iter().find(|pg| pg.id == id).cloned().ok_or_else(|| bad(CMD, format!("no PowerGrade still {id}")))?;
    let append = bool_p(p, "append").unwrap_or(false);
    let clips = target_video_clips(s, p, CMD)?;
    let seq_id = s.state.active_sequence.ok_or_else(|| bad(CMD, "no active sequence"))?;
    s.edit("Apply PowerGrade Still", |pr, _| {
        let Some(seq) = pr.sequence_mut(seq_id) else { return Ok(()) };
        for cid in &clips {
            if let Some((_, item)) = seq.find_item_mut(*cid) {
                if !append {
                    item.effects.retain(|e| !is_color_node_effect(&e.effect));
                }
                let insert_at = item.effects.iter().position(|ef| ef.def().is_some_and(|d| d.intrinsic)).unwrap_or(item.effects.len());
                for (offset, fx) in still.effects.iter().enumerate() {
                    item.effects.insert(insert_at + offset, fx.clone());
                }
            }
        }
        Ok(())
    })?;
    Ok(json!({ "appliedClips": clips.len(), "nodes": still.effects.len() }))
}

fn delete_still(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "color.deleteStill";
    let id = p.get("id").and_then(Value::as_u64).ok_or_else(|| bad(CMD, "need still `id`"))?;
    if !s.project.power_grades.iter().any(|pg| pg.id == id) {
        return Err(bad(CMD, format!("no PowerGrade still {id}")));
    }
    s.edit("Delete PowerGrade Still", |pr, _| {
        pr.power_grades.retain(|pg| pg.id != id);
        Ok(())
    })?;
    Ok(Value::Null)
}

fn add_serial_node(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "color.addSerialNode";
    let fx_id = str_p(p, "effect").unwrap_or("lumetri");
    if !is_color_node_effect(fx_id) {
        return Err(bad(CMD, format!("`{fx_id}` is not a serial color/ResolveFX node effect")));
    }
    let def = find_effect(fx_id).ok_or_else(|| bad(CMD, format!("unknown effect `{fx_id}`")))?;
    let clips = target_video_clips(s, p, CMD)?;
    let first = *clips.first().ok_or_else(|| bad(CMD, "no video clip selected"))?;
    let seq_id = s.state.active_sequence.ok_or_else(|| bad(CMD, "no active sequence"))?;
    let mut new_index = 0usize;
    s.edit("Add Serial Color Node", |pr, state| {
        let Some(seq) = pr.sequence_mut(seq_id) else { return Ok(()) };
        for cid in &clips {
            if let Some((_, item)) = seq.find_item_mut(*cid) {
                let insert_at = item.effects.iter().position(|ef| ef.def().is_some_and(|d| d.intrinsic)).unwrap_or(item.effects.len());
                item.effects.insert(insert_at, def.instance());
                if *cid == first {
                    new_index = insert_at;
                }
            }
        }
        if let Some(seq_ref) = pr.sequence(seq_id)
            && let Some((_, item)) = seq_ref.find_item(first)
        {
            state.selected_effect = crate::EffectSel::of(first, &item.effects, new_index);
        }
        Ok(())
    })?;
    Ok(json!({ "clip": first.0, "effect": fx_id, "index": new_index }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dctl_validation_import_enable_sync_and_powergrade_round_trip() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");

        // Select the first video clip in the active sequence
        let seq = s.active_sequence().expect("active sequence");
        let vid_clip = seq.video_tracks.iter().flat_map(|t| t.items.iter()).next().expect("video clip").id;
        s.state.selection = vec![vid_clip];

        // 1. Validate a custom DCTL shader with DEFINE_UI_PARAMS and __DEVICE__ helper
        let code = r#"
DEFINE_UI_PARAMS(p_warmth, Warmth, DCTLUI_SLIDER_FLOAT, 0.25, 0.0, 1.0, 0.01)
__DEVICE__ float boost(float c, float amt) {
    return c + amt;
}
__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {
    return make_float3(boost(p_R, p_warmth * 0.1f), p_G, p_B - p_warmth * 0.05f);
}
"#;
        let val = s.execute("plugins.validateDctl", json!({ "code": code })).expect("validateDctl");
        assert_eq!(val["valid"], json!(true));
        assert_eq!(val["params"].as_array().unwrap().len(), 1);

        // 2. Import and apply DCTL to the selected video clip
        let imp = s.execute("plugins.importDctl", json!({ "name": "Warm Film", "code": code, "apply": true })).expect("importDctl");
        assert_eq!(imp["appliedClips"], json!(1));
        assert_eq!(s.project.plugins.len(), 1);
        assert!(s.project.plugins[0].enabled);

        // 2b. Toggling `plugin:<id>` updates both preferences AND ProjectPlugin::enabled
        let plug_ref = imp["ref"].as_str().unwrap().to_string();
        s.execute("plugins.setEnabled", json!({ "id": plug_ref, "enabled": false })).expect("disable project plugin");
        assert!(!s.project.plugins[0].enabled);
        let plist_disabled = s.execute("plugins.list", json!({})).expect("plugins.list");
        assert_eq!(plist_disabled["project"][0]["enabled"], json!(false));

        s.execute("plugins.setEnabled", json!({ "id": plug_ref, "enabled": true })).expect("re-enable project plugin");
        assert!(s.project.plugins[0].enabled);

        // 2c. Disabling enableDctl in preferences rejects new DCTL imports
        s.prefs.plugins.enable_dctl = false;
        assert!(s.execute("plugins.importDctl", json!({ "code": code })).is_err());
        s.prefs.plugins.enable_dctl = true;

        // 3. Add a serial CST node and grab a PowerGrade still
        let added = s.execute("color.addSerialNode", json!({ "effect": "color_space_transform" })).expect("addSerialNode");
        assert_eq!(added["effect"], json!("color_space_transform"));

        let still = s.execute("color.grabStill", json!({ "name": "Hero Look" })).expect("grabStill");
        let still_id = still["id"].as_u64().expect("still id");
        assert!(still["nodeCount"].as_u64().unwrap() >= 2);

        let listed = s.execute("color.listStills", json!({})).expect("listStills");
        assert_eq!(listed["stills"].as_array().unwrap().len(), 1);

        // 4. Apply the PowerGrade still and verify plugins.list
        let applied = s.execute("color.applyStill", json!({ "id": still_id })).expect("applyStill");
        assert_eq!(applied["appliedClips"], json!(1));

        let plist = s.execute("plugins.list", json!({})).expect("plugins.list");
        assert!(plist["builtin"].as_array().unwrap().len() >= 3);
        assert_eq!(plist["project"].as_array().unwrap().len(), 1);

        // 5. Delete still and project plugin
        s.execute("color.deleteStill", json!({ "id": still_id })).expect("deleteStill");
        assert!(s.project.power_grades.is_empty());
        let plug_id = imp["id"].as_str().unwrap().to_string();
        s.execute("plugins.remove", json!({ "id": plug_id })).expect("plugins.remove");
        assert!(s.project.plugins.is_empty());
    }

    fn build_test_zip(entries: &[(&str, u16, &[u8], usize)]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut central = Vec::new();
        for &(name, method, payload, uncomp_len) in entries {
            let local_off = buf.len() as u32;
            let nb = name.as_bytes();
            buf.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
            buf.extend_from_slice(&20_u16.to_le_bytes());
            buf.extend_from_slice(&0_u16.to_le_bytes());
            buf.extend_from_slice(&method.to_le_bytes());
            buf.extend_from_slice(&0_u32.to_le_bytes());
            buf.extend_from_slice(&0_u32.to_le_bytes());
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(uncomp_len as u32).to_le_bytes());
            buf.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0_u16.to_le_bytes());
            buf.extend_from_slice(nb);
            buf.extend_from_slice(payload);

            central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            central.extend_from_slice(&(uncomp_len as u32).to_le_bytes());
            central.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(nb);
        }
        let cd_off = buf.len() as u32;
        let cd_len = central.len() as u32;
        buf.extend_from_slice(&central);
        buf.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        buf.extend_from_slice(&0_u16.to_le_bytes());
        buf.extend_from_slice(&0_u16.to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        buf.extend_from_slice(&cd_len.to_le_bytes());
        buf.extend_from_slice(&cd_off.to_le_bytes());
        buf.extend_from_slice(&0_u16.to_le_bytes());
        buf
    }

    fn deflate_stored_block(raw: &[u8]) -> Vec<u8> {
        let len = raw.len() as u16;
        let nlen = !len;
        let mut out = Vec::with_capacity(5 + raw.len());
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(raw);
        out
    }

    #[test]
    fn drfx_bundle_and_ograf_broadcast_graphic_importers_work() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");

        let dctl_a = b"DEFINE_UI_PARAMS(exp, Exposure (EV), DCTLUI_SLIDER_FLOAT, 0.5, -4.0, 4.0, 0.05)\n__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) { return make_float3(p_R * _exp2f(exp), p_G * _exp2f(exp), p_B * _exp2f(exp)); }\n";
        let dctl_b_raw = b"__DEVICE__ float4 transition(int p_Width, int p_Height, int p_X, int p_Y, __TEXTURE__ p_FromTexR, __TEXTURE__ p_FromTexG, __TEXTURE__ p_FromTexB, __TEXTURE__ p_FromTexA, __TEXTURE__ p_ToTexR, __TEXTURE__ p_ToTexG, __TEXTURE__ p_ToTexB, __TEXTURE__ p_ToTexA, float p_Transition) { return make_float4(_mix(p_FromR, p_ToR, p_Transition), _mix(p_FromG, p_ToG, p_Transition), _mix(p_FromB, p_ToB, p_Transition), 1.0f); }\n";
        let dctl_b_deflated = deflate_stored_block(dctl_b_raw);

        let zip_bytes = build_test_zip(&[
            ("Shaders/ExposureGrade.dctl", 0, dctl_a, dctl_a.len()),
            ("Transitions/FlashCut.dctl", 8, &dctl_b_deflated, dctl_b_raw.len()),
            ("Macros/TitleMacro.setting", 0, b"{ Tools = ordered() {} }", 24),
        ]);

        let dir = tempfile::tempdir().expect("tempdir");
        let drfx_path = dir.path().join("LookPack.drfx");
        std::fs::write(&drfx_path, &zip_bytes).expect("write drfx");

        let drfx_res = s
            .execute("plugins.importDrfx", json!({ "path": drfx_path.to_string_lossy() }))
            .expect("importDrfx");
        assert_eq!(drfx_res["importedDctls"], json!(2));
        assert_eq!(drfx_res["entries"].as_array().unwrap().len(), 3);
        assert_eq!(s.project.plugins.len(), 2);
        assert!(s.project.plugins.iter().any(|p| p.kind == PluginKind::DctlTransform && p.name == "ExposureGrade"));
        assert!(s.project.plugins.iter().any(|p| p.kind == PluginKind::DctlTransition && p.name == "FlashCut"));

        let ograf_manifest = json!({
            "name": "Breaking News Lower Third",
            "version": "1.0.0",
            "schema": {
                "type": "object",
                "properties": {
                    "headline": { "type": "string", "title": "Headline", "default": "LIVE FROM SEOUL" },
                    "subline": { "type": "string", "title": "Subhead", "default": "DaVinci Resolve 21 Broadcast Workflow" }
                }
            }
        });
        let ograf_path = dir.path().join("LowerThird.ograf.json");
        std::fs::write(&ograf_path, ograf_manifest.to_string()).expect("write ograf");

        let ograf_res = s
            .execute("plugins.importOgraf", json!({ "path": ograf_path.to_string_lossy(), "seconds": 4.0 }))
            .expect("importOgraf");
        let cid = ClipId(ograf_res["clip"].as_u64().expect("clip id"));
        assert_eq!(ograf_res["name"], json!("Breaking News Lower Third"));
        assert_eq!(ograf_res["fields"].as_array().unwrap().len(), 2);
        let seq = s.active_sequence().expect("seq");
        let (_, item) = seq.find_item(cid).expect("inserted graphic clip");
        assert_eq!(item.name, "Breaking News Lower Third");
        assert!(item.effects.iter().any(|e| e.effect == filmcraft_project::graphic::TEXT_LAYER));
    }
}
