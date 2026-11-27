//! Dockable `Plugins` panel (Plugin Hub & Live DCTL Studio) and the `Preferences ▸ Plugins`
//! embedded manager (`Row::Custom("pluginManager")`).
//!
//! Brings DaVinci Resolve-style multi-format plugin discovery (`.dctl`, `.ofx.bundle`, `.vst3`,
//! `.ograf.json`, `.drfx`, `manifest.xml`) and live DCTL shader authoring into FilmCraft.

use egui::{Color32, CornerRadius, Frame, Margin, RichText, ScrollArea, Sense, Stroke, vec2};
use filmcraft_engine::plugins::{is_kind_enabled_by_prefs, is_plugin_disabled};
use filmcraft_project::{ClipId, PluginKind, TrackKind, builtin_dctls};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::FilmcraftApp;
use crate::icons::{self, Icon};
use crate::native_dialogs::FileDialog;
use crate::theme::Tokens;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsUi {
    pub filter: String,
    pub search: String,
    pub stage3d_open: bool,
    pub studio_open: bool,
    pub studio_name: String,
    pub studio_code: String,
    pub studio_status: String,
    pub studio_error: bool,
}

impl Default for PluginsUi {
    fn default() -> Self {
        let sample = builtin_dctls().first().map_or_else(String::new, |(_, _, code)| (*code).to_string());
        Self {
            filter: "all".into(),
            search: String::new(),
            stage3d_open: true,
            studio_open: true,
            studio_name: "Custom Film Grade".into(),
            studio_code: sample,
            studio_status: String::new(),
            studio_error: false,
        }
    }
}

#[derive(Clone)]
struct RowEntry {
    id: String,
    name: String,
    kind: PluginKind,
    is_3d: bool,
    vendor: String,
    source: &'static str,
    enabled: bool,
    category_allowed: bool,
    code: Option<String>,
    apply_effect: Option<(&'static str, Option<String>)>,
}

fn kind_badge(kind: PluginKind) -> (&'static str, Color32) {
    match kind {
        PluginKind::DctlTransform => ("DCTL", Color32::from_rgb(0xe8, 0x79, 0x3a)),
        PluginKind::DctlTransition => ("DCTL-TR", Color32::from_rgb(0xd9, 0x6b, 0x43)),
        PluginKind::NativePremierePrm => ("PRM", Color32::from_rgb(0x63, 0x66, 0xf1)),
        PluginKind::NativeAfterEffectsAex => ("AEX", Color32::from_rgb(0x8b, 0x5c, 0xf6)),
        PluginKind::OpenFxVideo | PluginKind::OpenFxTransition => ("OpenFX", Color32::from_rgb(0x3b, 0x82, 0xf6)),
        PluginKind::AudioVst3 => ("VST3", Color32::from_rgb(0x10, 0xb9, 0x81)),
        PluginKind::OgrafTitle => ("OGraf", Color32::from_rgb(0xa8, 0x55, 0xf7)),
        PluginKind::MacroBundle => ("DRFX", Color32::from_rgb(0xec, 0x48, 0x99)),
        PluginKind::WorkflowScript => ("Workflow", Color32::from_rgb(0x64, 0x74, 0x8b)),
    }
}

fn matches_filter(row: &RowEntry, filter: &str) -> bool {
    match filter {
        "3d" => row.is_3d,
        "ofx" => matches!(row.kind, PluginKind::OpenFxVideo | PluginKind::OpenFxTransition | PluginKind::MacroBundle),
        "adobe" => matches!(row.kind, PluginKind::NativePremierePrm | PluginKind::NativeAfterEffectsAex),
        "dctl" => matches!(row.kind, PluginKind::DctlTransform | PluginKind::DctlTransition),
        "audio" => matches!(row.kind, PluginKind::AudioVst3),
        "ograf" => matches!(row.kind, PluginKind::OgrafTitle | PluginKind::WorkflowScript),
        _ => true,
    }
}

fn selected_clip_kinds(app: &FilmcraftApp) -> (bool, bool) {
    let Some(seq) = app.session.active_sequence() else { return (false, false) };
    let mut has_video = false;
    let mut has_audio = false;
    for cid in &app.session.state.selection {
        if let Some((tid, _)) = seq.find_item(*cid)
            && let Some(tr) = seq.track(tid)
        {
            match tr.kind {
                TrackKind::Video => has_video = true,
                TrackKind::Audio => has_audio = true,
            }
        }
    }
    (has_video, has_audio)
}

fn collect_entries(app: &FilmcraftApp) -> Vec<RowEntry> {
    let prefs = &app.session.prefs.plugins;
    let disabled = &prefs.disabled_plugins;
    let mut rows = Vec::new();

    // Native ResolveFX, 3D & Material, and Fairlight DSP built into FilmCraft
    for (fx_id, label, kind, vendor, is_3d) in [
        ("stage_3d", "3D Stage Compositor", PluginKind::OpenFxVideo, "3D & Material", true),
        ("surface_relight", "Surface Relight & PBR Sampler", PluginKind::OpenFxVideo, "3D & Material", true),
        ("procedural_pbr", "Procedural PBR Material", PluginKind::OpenFxVideo, "3D & Material", true),
        ("color_space_transform", "Color Space Transform (CST)", PluginKind::OpenFxVideo, "ResolveFX Color", false),
        ("film_halation", "Film Halation", PluginKind::OpenFxVideo, "ResolveFX Stylize", false),
        ("color_warper", "Color Warper (Hue-Chroma)", PluginKind::OpenFxVideo, "ResolveFX Color", false),
        ("qualifier_3d", "3D HSL Qualifier", PluginKind::OpenFxVideo, "ResolveFX Matte", false),
        ("dynamic_zoom", "Dynamic Zoom (Ken Burns)", PluginKind::OpenFxVideo, "ResolveFX Transform", false),
        ("dialogue_leveler", "Fairlight Dialogue Leveler", PluginKind::AudioVst3, "Fairlight FX", false),
    ] {
        let full_id = format!("resolvefx:{fx_id}");
        let allowed = is_kind_enabled_by_prefs(kind, prefs);
        rows.push(RowEntry {
            enabled: !is_plugin_disabled(disabled, &full_id),
            category_allowed: allowed,
            id: full_id,
            name: label.into(),
            kind,
            is_3d,
            vendor: vendor.into(),
            source: "Built-in",
            code: None,
            apply_effect: Some((fx_id, None)),
        });
    }

    // Built-in clean-room DCTL shaders
    for &(id, name, code) in builtin_dctls() {
        let full_id = format!("builtin:{id}");
        rows.push(RowEntry {
            enabled: !is_plugin_disabled(disabled, &full_id),
            category_allowed: prefs.enable_dctl,
            id: full_id.clone(),
            name: name.into(),
            kind: PluginKind::DctlTransform,
            is_3d: false,
            vendor: "FilmCraft DCTL".into(),
            source: "Built-in",
            code: Some(code.to_string()),
            apply_effect: Some(("dctl_transform", Some(full_id))),
        });
    }

    // Project-embedded portable DCTLs and C++ (.prm / .aex) binary plug-ins
    for p in &app.session.project.plugins {
        let full_id = format!("plugin:{}", p.id);
        let is_native = matches!(p.kind, PluginKind::NativePremierePrm | PluginKind::NativeAfterEffectsAex);
        rows.push(RowEntry {
            enabled: p.enabled && !is_plugin_disabled(disabled, &full_id),
            category_allowed: is_kind_enabled_by_prefs(p.kind, prefs),
            id: full_id.clone(),
            name: p.name.clone(),
            kind: p.kind,
            is_3d: false,
            vendor: if is_native { "Project C++ Plug-in".into() } else { "Project DCTL".into() },
            source: "Project",
            code: (!is_native).then(|| p.code.to_string()),
            apply_effect: Some((if is_native { "native_adobe_plugin" } else { "dctl_transform" }, Some(full_id))),
        });
    }

    // External discovered plugins on disk
    for d in &app.session.plugins.discovered {
        let allowed = is_kind_enabled_by_prefs(d.kind, prefs);
        if !allowed {
            continue;
        }
        let apply_effect = match d.kind {
            PluginKind::DctlTransform | PluginKind::DctlTransition => Some(("import_dctl_path", Some(d.path.clone()))),
            PluginKind::NativePremierePrm | PluginKind::NativeAfterEffectsAex => Some(("import_native_path", Some(d.path.clone()))),
            PluginKind::MacroBundle => Some(("import_drfx_path", Some(d.path.clone()))),
            PluginKind::OgrafTitle => Some(("import_ograf_path", Some(d.path.clone()))),
            _ => None,
        };
        rows.push(RowEntry {
            enabled: !is_plugin_disabled(disabled, &d.id),
            category_allowed: allowed,
            id: d.id.clone(),
            name: d.name.clone(),
            kind: d.kind,
            is_3d: false,
            vendor: d.vendor.clone(),
            source: "System / Disk",
            code: None,
            apply_effect,
        });
    }

    rows
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: egui::Rect) {
    let t = app.tokens;
    ui.painter().rect_filled(rect, 0.0, t.panel_bg);
    let inner = rect.shrink2(vec2(10.0, 8.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).id_salt("plugins-hub"));
    child.set_max_width(inner.width().max(120.0));

    draw_toolbar(app, &mut child, &t);
    child.add_space(6.0);

    let avail_h = child.available_height().max(80.0);
    let list_frac = match (app.ui.plugins_ui.stage3d_open, app.ui.plugins_ui.studio_open) {
        (true, true) => 0.34,
        (true, false) => 0.62,
        (false, true) => 0.46,
        (false, false) => 1.0,
    };
    let list_h = if list_frac < 1.0 { (avail_h * list_frac).max(90.0) } else { avail_h };
    draw_plugin_list(app, &mut child, &t, inner.width(), list_h);

    if app.ui.plugins_ui.stage3d_open {
        child.add_space(5.0);
        child.separator();
        child.add_space(3.0);
        draw_stage3d_lab(app, &mut child, &t);
    }

    if app.ui.plugins_ui.studio_open {
        child.add_space(5.0);
        child.separator();
        child.add_space(3.0);
        draw_dctl_studio(app, &mut child, &t);
    }
}

/// Embedded Plugin Manager table inside `Edit ▸ Preferences ▸ Plugins…` (`Row::Custom("pluginManager")`).
pub fn show_settings_manager(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let t = app.tokens;
    Frame::new().fill(t.panel_bg).stroke(Stroke::new(1.0, t.separator)).corner_radius(CornerRadius::same(4)).inner_margin(Margin::same(8)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tl!("Installed & Discovered Plugins")).font(Tokens::semibold(12.0)).color(t.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let open_hub = ui.button(tl!("Open DCTL Studio"));
                app.auto.add("settings.plugins.openHub", open_hub.rect, "Open DCTL Studio");
                if open_hub.clicked() {
                    app.ui.settings = None;
                    app.dialog = None;
                    app.show_panel(crate::dock::PanelKind::Plugins);
                }
                let scan = ui.button(tl!("Rescan Folders"));
                app.auto.add("settings.plugins.scan", scan.rect, "Rescan Folders");
                if scan.clicked()
                    && let Ok(res) = app.session.execute("plugins.scan", json!({}))
                {
                    let n = res["discovered"].as_u64().unwrap_or(0);
                    app.ui.status = tlf!("Scanned plugin folders ({n} external bundle(s) found)", n);
                }
            });
        });
        ui.add_space(4.0);
        let max_w = ui.available_width();
        draw_plugin_list(app, ui, &t, max_w, 190.0);
    });
}

fn draw_toolbar(app: &mut FilmcraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let mut ask_import_native = false;
    ui.horizontal_wrapped(|ui| {
        for (key, label) in [
            ("all", tl!("All")),
            ("3d", tl!("3D & Material")),
            ("ofx", "ResolveFX / OFX"),
            ("adobe", tl!("PRM / AEX")),
            ("dctl", "DCTL"),
            ("audio", "Fairlight / VST3"),
            ("ograf", "OGraf / Workflow"),
        ] {
            let sel = app.ui.plugins_ui.filter == key;
            let b = ui.selectable_label(sel, RichText::new(label).size(11.5));
            app.auto.add(&format!("plugins.filter.{key}"), b.rect, label);
            if b.clicked() {
                app.ui.plugins_ui.filter = key.into();
            }
        }
        let studio = ui.selectable_label(app.ui.plugins_ui.studio_open, RichText::new(tl!("DCTL Studio")).size(11.5));
        app.auto.add("plugins.toggleStudio", studio.rect, "DCTL Studio");
        if studio.clicked() {
            app.ui.plugins_ui.studio_open = !app.ui.plugins_ui.studio_open;
        }
        let lab3d = ui.selectable_label(app.ui.plugins_ui.stage3d_open, RichText::new(tl!("3D & Material Lab")).size(11.5));
        app.auto.add("plugins.stage3d.toggle", lab3d.rect, "3D & Material Lab");
        if lab3d.clicked() {
            app.ui.plugins_ui.stage3d_open = !app.ui.plugins_ui.stage3d_open;
        }
        let imp_native = ui.button(tl!("Import .prm/.aex…"));
        app.auto.add("plugins.importNative", imp_native.rect, "Import .prm/.aex");
        if imp_native.on_hover_text(tl!("Import C++ Premiere (.prm) or After Effects (.aex) binary plug-in")).clicked() {
            ask_import_native = true;
        }
        let scan = ui.button(tl!("Scan"));
        app.auto.add("plugins.scan", scan.rect, "Scan");
        if scan.on_hover_text(tl!("Rescan OpenFX, VST3, DCTL, and OGraf folders")).clicked()
            && let Ok(res) = app.session.execute("plugins.scan", json!({}))
        {
            let n = res["discovered"].as_u64().unwrap_or(0);
            app.ui.status = tlf!("Plugin scan complete ({n} external item(s))", n);
        }
    });
    if ask_import_native {
        app.pick_ui(FileDialog::open_file(tl!("Adobe C++ Plug-in"), &["prm", "aex", "plugin"]), |app, paths| {
            let Some(path) = paths.into_iter().next() else { return };
            let (has_video_clip, _) = selected_clip_kinds(app);
            match app.session.execute("plugins.importNativeBinary", json!({ "path": path, "apply": has_video_clip })) {
                Ok(v) => {
                    let name = v["name"].as_str().unwrap_or("Plug-in");
                    let badge = v["badge"].as_str().unwrap_or("PRM");
                    app.ui.status = tlf!("Imported {badge} C++ plug-in {name}", badge = badge, name = name);
                }
                Err(e) => app.ui.status = e.to_string(),
            }
        });
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let icon_r = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover()).0;
        icons::paint(ui.painter(), icon_r, Icon::Search, t.text_dim);
        let r = ui.add(
            egui::TextEdit::singleline(&mut app.ui.plugins_ui.search)
                .desired_width((ui.available_width() - 8.0).max(80.0))
                .hint_text(tl!("Search plugins, DCTL shaders, OpenFX, VST3…")),
        );
        app.auto.add("plugins.search", r.rect, "Search plugins");
    });
}

fn draw_plugin_list(app: &mut FilmcraftApp, ui: &mut egui::Ui, t: &Tokens, max_w: f32, max_h: f32) {
    let entries = collect_entries(app);
    let q = app.ui.plugins_ui.search.trim().to_lowercase();
    let filter = app.ui.plugins_ui.filter.clone();
    let (has_video_clip, has_audio_clip) = selected_clip_kinds(app);
    let has_seq = app.session.active_sequence().is_some();
    let mut toggle_cmd: Option<(String, bool)> = None;
    let mut apply_cmd: Option<(&'static str, Option<String>, String)> = None;
    let mut load_code: Option<(String, String)> = None;

    ScrollArea::vertical().id_salt("plugins-list-scroll").max_height(max_h).auto_shrink([false, false]).show(ui, |ui| {
        let card_w = (max_w - 22.0).max(96.0);
        ui.set_max_width(max_w.max(120.0));
        for row in entries {
            if !matches_filter(&row, &filter) {
                continue;
            }
            if !q.is_empty() && !row.name.to_lowercase().contains(&q) && !row.vendor.to_lowercase().contains(&q) {
                continue;
            }
            let effective_on = row.enabled && row.category_allowed;
            let can_target = match row.kind {
                PluginKind::AudioVst3 => has_audio_clip,
                PluginKind::MacroBundle => true,
                PluginKind::OgrafTitle => has_seq,
                _ => has_video_clip,
            };
            let (badge, badge_col) = kind_badge(row.kind);
            ui.push_id(&row.id, |ui| {
                Frame::new().fill(t.field_bg).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                    ui.set_max_width(card_w);
                    ui.set_width(card_w);
                    // Lay out right-side action buttons first so narrow docked panels never push Apply off-screen.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some((fx_id, arg)) = row.apply_effect.clone() {
                            let btn = ui.add_enabled(effective_on && can_target, egui::Button::new(tl!("Apply")));
                            app.auto.add(&format!("plugins.apply.{}", row.id), btn.rect, "Apply");
                            if btn.clicked() {
                                apply_cmd = Some((fx_id, arg, row.name.clone()));
                            }
                        }
                        if let Some(code) = row.code.clone() {
                            let edit = ui.button(tl!("Edit DCTL"));
                            app.auto.add(&format!("plugins.edit.{}", row.id), edit.rect, "Edit DCTL");
                            if edit.clicked() {
                                load_code = Some((row.name.clone(), code));
                            }
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let mut en = row.enabled;
                            let chk = ui.add_enabled(row.category_allowed, egui::Checkbox::without_text(&mut en));
                            app.auto.add(&format!("plugins.enable.{}", row.id), chk.rect, &row.name);
                            if chk.changed() {
                                toggle_cmd = Some((row.id.clone(), en));
                            }
                            ui.label(RichText::new(badge).font(Tokens::semibold(10.0)).color(badge_col));
                            ui.add(egui::Label::new(RichText::new(&row.name).size(12.0).color(if effective_on { t.text } else { t.text_faint })).truncate());
                            ui.add(egui::Label::new(RichText::new(format!("· {} ({})", row.vendor, row.source)).size(11.0).color(t.text_dim)).truncate());
                        });
                    });
                });
            });
            ui.add_space(2.0);
        }
    });

    if let Some((id, enabled)) = toggle_cmd {
        let _ = app.session.execute("plugins.setEnabled", json!({ "id": id, "enabled": enabled }));
    }
    if let Some((name, code)) = load_code {
        app.ui.plugins_ui.studio_open = true;
        app.ui.plugins_ui.studio_name = name;
        app.ui.plugins_ui.studio_code = code;
        app.ui.plugins_ui.studio_status.clear();
    }
    if let Some((fx_id, arg, label)) = apply_cmd {
        apply_plugin_action(app, fx_id, arg.as_deref(), &label);
    }
}

fn selected_video_clip_ids(app: &FilmcraftApp) -> Vec<ClipId> {
    let Some(seq) = app.session.active_sequence() else { return Vec::new() };
    app.session
        .state
        .selection
        .iter()
        .copied()
        .filter(|c| seq.find_item(*c).and_then(|(tid, _)| seq.track(tid)).is_some_and(|tr| tr.kind == TrackKind::Video))
        .collect()
}

fn apply_plugin_action(app: &mut FilmcraftApp, fx_id: &str, arg: Option<&str>, label: &str) {
    if fx_id == "import_drfx_path" {
        let Some(path) = arg else { return };
        match app.session.execute("plugins.importDrfx", json!({ "path": path })) {
            Ok(v) => {
                let n = v["importedDctls"].as_u64().unwrap_or(0);
                app.ui.status = tlf!("Imported {n} DCTL shader(s) from {name}", n = n, name = label);
            }
            Err(e) => app.ui.status = e.to_string(),
        }
        return;
    }
    if fx_id == "import_ograf_path" {
        let Some(path) = arg else { return };
        match app.session.execute("plugins.importOgraf", json!({ "path": path })) {
            Ok(_) => app.ui.status = tlf!("Inserted OGraf graphic {name} at playhead", name = label),
            Err(e) => app.ui.status = e.to_string(),
        }
        return;
    }
    let res = if fx_id == "import_dctl_path" {
        let Some(path) = arg else { return };
        app.session.execute("plugins.importDctl", json!({ "path": path, "apply": true }))
    } else if fx_id == "import_native_path" {
        let Some(path) = arg else { return };
        app.session.execute("plugins.importNativeBinary", json!({ "path": path, "apply": true }))
    } else if fx_id == "dctl_transform" || fx_id == "native_adobe_plugin" {
        let vid_clips = selected_video_clip_ids(app);
        let clip_nums: Vec<u64> = vid_clips.iter().map(|c| c.0).collect();
        let r = app.session.execute("effects.apply", json!({ "clips": clip_nums, "effect": fx_id }));
        if r.is_ok()
            && let Some(plugin_ref) = arg
        {
            let targets: Vec<(u64, usize)> = app
                .session
                .active_sequence()
                .map(|seq| {
                    vid_clips
                        .iter()
                        .filter_map(|&cid| {
                            let (_, item) = seq.find_item(cid)?;
                            let idx = item.effects.iter().rposition(|e| e.effect == fx_id)?;
                            Some((cid.0, idx))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (cid, idx) in targets {
                let _ = app.session.execute("effects.setParam", json!({ "clip": cid, "effect": idx, "param": "plugin_ref", "value": plugin_ref }));
            }
        }
        r
    } else {
        app.session.execute("effects.apply", json!({ "effect": fx_id }))
    };
    match res {
        Ok(_) => app.ui.status = tlf!("Applied {name} to selected clip(s)", name = label),
        Err(e) => app.ui.status = e.to_string(),
    }
}

fn draw_stage3d_lab(app: &mut FilmcraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let (has_video_clip, _) = selected_clip_kinds(app);
    let has_seq = app.session.active_sequence().is_some();
    let mut preset_click: Option<(&'static str, String)> = None;
    let mut run_sample = false;

    Frame::new().fill(t.field_bg).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(8, 5)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tl!("3D Stage & SDF Clay")).font(Tokens::semibold(11.0)).color(t.text_dim));
            for (pid, lbl) in [
                ("gold_sphere", tl!("Polished Gold Sphere")),
                ("bottle_mockup", tl!("Product Bottle Mockup")),
                ("sdf_clay_sculpture", tl!("SDF Clay Sculpture")),
            ] {
                let btn = ui.add_enabled(has_video_clip, egui::Button::new(RichText::new(lbl).size(11.0)));
                app.auto.add(&format!("plugins.stage3d.{pid}"), btn.rect, lbl);
                if btn.clicked() {
                    preset_click = Some((pid, lbl.to_string()));
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tl!("Material Sampler & Relight")).font(Tokens::semibold(11.0)).color(t.text_dim));
            for (pid, lbl) in
                [("relight_warm_key", tl!("3D Surface Relight")), ("pbr_normal_map", tl!("Extract Normal Map")), ("seamless_albedo", tl!("Make Seamless PBR"))]
            {
                let btn = ui.add_enabled(has_video_clip, egui::Button::new(RichText::new(lbl).size(11.0)));
                app.auto.add(&format!("plugins.stage3d.{pid}"), btn.rect, lbl);
                if btn.clicked() {
                    preset_click = Some((pid, lbl.to_string()));
                }
            }
            let smp = ui.add_enabled(has_seq, egui::Button::new(RichText::new(tl!("Sample PBR")).size(11.0)));
            app.auto.add("plugins.stage3d.sample_pbr", smp.rect, "Sample PBR");
            if smp.clicked() {
                run_sample = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tl!("Procedural PBR")).font(Tokens::semibold(11.0)).color(t.text_dim));
            for (pid, lbl) in
                [("brushed_steel", tl!("Brushed Metal")), ("terrazzo_marble", tl!("Terrazzo Marble")), ("carbon_hex_weave", tl!("Carbon Hex Weave"))]
            {
                let btn = ui.add_enabled(has_video_clip, egui::Button::new(RichText::new(lbl).size(11.0)));
                app.auto.add(&format!("plugins.stage3d.{pid}"), btn.rect, lbl);
                if btn.clicked() {
                    preset_click = Some((pid, lbl.to_string()));
                }
            }
        });
    });

    if let Some((preset, label)) = preset_click {
        match app.session.execute("stage3d.applyPreset", json!({ "preset": preset })) {
            Ok(_) => app.ui.status = tlf!("Applied {name} to selected clip(s)", name = label),
            Err(e) => app.ui.status = e.to_string(),
        }
    }
    if run_sample {
        match app.session.execute("material.sampleClipPbr", json!({ "makeTileable": true, "delight": 0.65 })) {
            Ok(v) => {
                let w = v["width"].as_u64().unwrap_or(0);
                let h = v["height"].as_u64().unwrap_or(0);
                let seam = v["borderSeamRmse"].as_f64().unwrap_or(0.0);
                app.ui.status = tlf!("Sampled 6 PBR channels ({w}×{h}, seam RMSE {seam})", w = w, h = h, seam = format!("{seam:.4}"));
            }
            Err(e) => app.ui.status = e.to_string(),
        }
    }
}

fn draw_dctl_studio(app: &mut FilmcraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let dev_mode = app.session.prefs.plugins.developer_mode;
    let dctl_enabled = app.session.prefs.plugins.enable_dctl;
    let (has_video_clip, _) = selected_clip_kinds(app);

    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Live DCTL Studio")).font(Tokens::semibold(12.0)).color(t.text));
        ui.label(RichText::new("DEFINE_UI_PARAMS · transform(p_Width, p_Height, p_X, p_Y, p_R, p_G, p_B)").size(10.5).color(t.text_dim));
    });
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Name")).size(11.5).color(t.text_dim));
        let nr = ui.add(egui::TextEdit::singleline(&mut app.ui.plugins_ui.studio_name).desired_width(170.0));
        app.auto.add("plugins.studio.name", nr.rect, "DCTL Name");

        craft_ui::ComboBox::from_id_salt("dctl-studio-templates").selected_text(tl!("Templates")).width(135.0).show_ui(ui, |ui| {
            for &(_, name, code) in builtin_dctls() {
                if ui.selectable_label(false, name).clicked() {
                    app.ui.plugins_ui.studio_name = name.into();
                    app.ui.plugins_ui.studio_code = code.to_string();
                    app.ui.plugins_ui.studio_status.clear();
                }
            }
        });

        let val_btn = ui.button(tl!("Validate DCTL"));
        app.auto.add("plugins.studio.validate", val_btn.rect, "Validate DCTL");
        if val_btn.clicked() {
            let code = app.ui.plugins_ui.studio_code.clone();
            match app.session.execute("plugins.validateDctl", json!({ "code": code })) {
                Ok(v) if v["valid"].as_bool() == Some(true) => {
                    let count = v["params"].as_array().map_or(0, Vec::len);
                    let kind = if v["isTransition"].as_bool() == Some(true) { "transition" } else { "transform" };
                    let mut msg = tlf!("Valid DCTL ({kind}, {count} UI parameter(s))", kind, count);
                    if dev_mode
                        && let Some(rgb) = v["sampleRgb"].as_array()
                        && rgb.len() == 3
                    {
                        let r = rgb[0].as_f64().unwrap_or(0.0);
                        let g = rgb[1].as_f64().unwrap_or(0.0);
                        let b = rgb[2].as_f64().unwrap_or(0.0);
                        msg.push_str(&format!(" · sample=[{r:.3}, {g:.3}, {b:.3}]"));
                    }
                    app.ui.plugins_ui.studio_status = msg;
                    app.ui.plugins_ui.studio_error = false;
                }
                Ok(v) => {
                    app.ui.plugins_ui.studio_status = v["error"].as_str().unwrap_or("Invalid DCTL").to_string();
                    app.ui.plugins_ui.studio_error = true;
                }
                Err(e) => {
                    app.ui.plugins_ui.studio_status = e.to_string();
                    app.ui.plugins_ui.studio_error = true;
                }
            }
        }

        let save_label = if has_video_clip { tl!("Save & Apply to Clip") } else { tl!("Save to Project") };
        let save_btn = ui.add_enabled(dctl_enabled, egui::Button::new(save_label));
        app.auto.add("plugins.studio.save", save_btn.rect, "Save DCTL");
        if save_btn.clicked() {
            let name = app.ui.plugins_ui.studio_name.clone();
            let code = app.ui.plugins_ui.studio_code.clone();
            match app.session.execute("plugins.importDctl", json!({ "name": name, "code": code, "apply": has_video_clip })) {
                Ok(v) => {
                    let applied = v["appliedClips"].as_u64().unwrap_or(0);
                    let saved_name = v["name"].as_str().unwrap_or("DCTL");
                    app.ui.plugins_ui.studio_status = tlf!("Saved {name} ({applied} clip(s) updated)", name = saved_name, applied = applied);
                    app.ui.plugins_ui.studio_error = false;
                }
                Err(e) => {
                    app.ui.plugins_ui.studio_status = e.to_string();
                    app.ui.plugins_ui.studio_error = true;
                }
            }
        }
    });

    if !app.ui.plugins_ui.studio_status.is_empty() {
        let col = if app.ui.plugins_ui.studio_error { t.render_red } else { t.render_green };
        let status_lbl = ui.label(RichText::new(&app.ui.plugins_ui.studio_status).size(11.0).color(col));
        app.auto.add("plugins.studio.status", status_lbl.rect, &app.ui.plugins_ui.studio_status);
    }

    let code_h = ui.available_height().max(72.0);
    ScrollArea::vertical().id_salt("plugins-studio-code-scroll").max_height(code_h).show(ui, |ui| {
        let ed = ui.add_sized(
            vec2(ui.available_width(), code_h),
            egui::TextEdit::multiline(&mut app.ui.plugins_ui.studio_code).font(egui::TextStyle::Monospace).code_editor().desired_rows(6),
        );
        app.auto.add("plugins.studio.code", ed.rect, "DCTL Source Code");
    });
}
