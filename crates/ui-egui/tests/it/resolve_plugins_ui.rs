//! Headless UI tests for DaVinci Resolve-inspired features:
//! - Bottom 7-Page Navigation Bar (`Media · Cut · Edit · Fusion · Color · Fairlight · Deliver`)
//! - Cut-page Dual-Timeline Full-Sequence Overview Strip (`timeline.cutOverview`)
//! - Dockable `Plugins` Hub & Live DCTL Studio (`PanelKind::Plugins`)
//! - `Preferences ▸ Plugins` manager (`settings.category.plugins`)

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::dock::{LayoutExt as _, PanelKind};
use filmcraft_ui_egui::state::Mode;
use filmcraft_ui_egui::{FilmcraftApp, menus, page_bar};
use serde_json::json;

fn demo_harness() -> Harness<'static, FilmcraftApp> {
    demo_harness_with_control().0
}

fn demo_harness_with_control() -> (Harness<'static, FilmcraftApp>, Sender<ControlRequest>) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).expect("demo project");
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    step_frames(&mut h, 4);
    (h, tx)
}

fn step_frames(h: &mut Harness<'static, FilmcraftApp>, n: usize) {
    for _ in 0..n {
        let ctx = h.ctx.clone();
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        h.step();
    }
}

fn click_auto(h: &mut Harness<'static, FilmcraftApp>, tx: &Sender<ControlRequest>, id: &str) {
    let (req, reply) = ControlRequest::new("ui.click", json!({ "id": id }));
    tx.send(req).expect("send ui.click");
    for _ in 0..200 {
        step_frames(h, 1);
        if let Ok(v) = reply.try_recv() {
            assert_eq!(v["ok"], json!(true), "ui.click {id} failed: {v}");
            step_frames(h, 2);
            return;
        }
    }
    panic!("no reply to ui.click {id}");
}

fn store_only_zip(file_name: &str, payload: &[u8]) -> Vec<u8> {
    let name_bytes = file_name.as_bytes();
    let size = payload.len() as u32;
    let name_len = name_bytes.len() as u16;
    let mut out = Vec::new();
    // Local file header (method 0 = stored)
    out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
    out.extend_from_slice(&20_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(payload);
    let cd_offset = out.len() as u32;
    // Central directory header
    out.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
    out.extend_from_slice(&20_u16.to_le_bytes());
    out.extend_from_slice(&20_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(name_bytes);
    let cd_size = out.len() as u32 - cd_offset;
    // EOCD
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}

#[test]
fn resolve_seven_page_bar_switches_modes_workspaces_and_cut_overview() {
    let mut h = demo_harness();
    for page in ["media", "cut", "edit", "fusion", "color", "fairlight", "deliver"] {
        assert!(!h.state().auto.query(&format!("status.page.{page}")).is_empty(), "missing status.page.{page}");
    }

    // Switch to Cut page -> Assembly workspace + Cut Overview Dual-Timeline Strip enabled
    page_bar::activate_page(h.state_mut(), "cut");
    for _ in 0..3 {
        h.step();
    }
    assert_eq!(h.state().ui.mode, Mode::Edit);
    assert_eq!(h.state().ui.workspace, "Assembly");
    assert!(h.state().ui.timeline.cut_overview);
    assert!(!h.state().auto.query("timeline.cutOverview").is_empty(), "Cut Overview strip drawn");

    // Switch to Fusion page -> Effects workspace + Plugins panel docked & visible
    page_bar::activate_page(h.state_mut(), "fusion");
    for _ in 0..3 {
        h.step();
    }
    assert_eq!(h.state().ui.workspace, "Effects");
    assert!(h.state().ui.dock.is_visible(PanelKind::Plugins), "Plugins panel opened on Fusion page");
    assert!(!h.state().auto.query("plugins.studio.validate").is_empty(), "Live DCTL Studio controls drawn");
    assert!(!h.state().auto.query("plugins.enable.resolvefx:film_halation").is_empty(), "Canonical ResolveFX plugin ID registered");

    // Switch to Color, Fairlight, Media, Deliver, and back to Edit
    page_bar::activate_page(h.state_mut(), "color");
    h.step();
    assert_eq!(h.state().ui.workspace, "Color");

    page_bar::activate_page(h.state_mut(), "fairlight");
    h.step();
    assert_eq!(h.state().ui.workspace, "Audio");

    page_bar::activate_page(h.state_mut(), "media");
    h.step();
    assert_eq!(h.state().ui.mode, Mode::Import);

    page_bar::activate_page(h.state_mut(), "deliver");
    h.step();
    assert_eq!(h.state().ui.mode, Mode::Export);

    // Toggle View ▸ Show Page Navigation Bar
    let ctx = h.ctx.clone();
    menus::invoke(h.state_mut(), &ctx, "view.showPageBar", json!({"on": false})).expect("hide page bar");
    for _ in 0..2 {
        h.step();
    }
    assert!(h.state().auto.query("status.page.edit").is_empty(), "page bar hidden");
    assert!(h.state().ui_error.is_none(), "{:?}", h.state().ui_error);
}

#[test]
fn preferences_plugins_page_renders_live_plugin_manager() {
    let mut h = demo_harness();
    let ctx = h.ctx.clone();
    menus::invoke(h.state_mut(), &ctx, "app.settings.plugins", json!({})).expect("open plugins settings");
    for _ in 0..3 {
        h.step();
    }
    assert!(!h.state().auto.query("settings.plugins.enableDctl").is_empty());
    assert!(!h.state().auto.query("settings.plugins.scan").is_empty());
    assert!(!h.state().auto.query("settings.plugins.openHub").is_empty());
    assert!(h.state().ui_error.is_none(), "{:?}", h.state().ui_error);
}

#[test]
fn fusion_plugins_panel_exposes_3d_material_filter_and_stage3d_presets() {
    let mut h = demo_harness();
    page_bar::activate_page(h.state_mut(), "fusion");
    for _ in 0..3 {
        h.step();
    }

    assert!(!h.state().auto.query("plugins.filter.3d").is_empty(), "3D & Material filter pill registered");
    for id in [
        "plugins.stage3d.toggle",
        "plugins.stage3d.gold_sphere",
        "plugins.stage3d.bottle_mockup",
        "plugins.stage3d.sdf_clay_sculpture",
        "plugins.stage3d.relight_warm_key",
        "plugins.stage3d.pbr_normal_map",
        "plugins.stage3d.seamless_albedo",
        "plugins.stage3d.brushed_steel",
        "plugins.stage3d.terrazzo_marble",
        "plugins.stage3d.carbon_hex_weave",
        "plugins.stage3d.sample_pbr",
    ] {
        assert!(!h.state().auto.query(id).is_empty(), "missing automation id {id}");
    }

    // Switch filter to "3d" and verify 3D & Material built-ins are shown while ResolveFX-only items filter out
    h.state_mut().ui.plugins_ui.filter = "3d".into();
    for _ in 0..2 {
        h.step();
    }
    assert!(!h.state().auto.query("plugins.enable.resolvefx:stage_3d").is_empty());
    assert!(!h.state().auto.query("plugins.enable.resolvefx:surface_relight").is_empty());
    assert!(!h.state().auto.query("plugins.enable.resolvefx:procedural_pbr").is_empty());
    assert!(h.state().auto.query("plugins.enable.resolvefx:film_halation").is_empty());
    assert!(h.state().ui_error.is_none(), "{:?}", h.state().ui_error);
}

#[test]
fn dctl_dynamic_effect_controls_labels_and_ograf_apply_work() {
    let (mut h, tx) = demo_harness_with_control();
    let vid_clip = h.state().session.active_sequence().unwrap().video_tracks[0].items[0].id;
    h.state_mut().session.state.selection = vec![vid_clip];

    let dctl_src = concat!(
        "DEFINE_UI_PARAMS(exp_ev, Exposure (EV), DCTLUI_SLIDER_FLOAT, 0.5, -4.0, 4.0, 0.05)\n",
        "DEFINE_UI_PARAMS(toe_on, Filmic Toe, DCTLUI_CHECK_BOX, 1)\n",
        "__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) {\n",
        "    float g = _exp2f(exp_ev);\n",
        "    return make_float3(p_R * g, p_G * g, p_B * g);\n",
        "}\n"
    );
    h.state_mut()
        .session
        .execute("plugins.importDctl", json!({ "name": "Dynamic Label Test", "code": dctl_src, "apply": true }))
        .expect("importDctl");
    let (fx_idx, collapse_keys) = {
        let (_, it) = h.state().session.active_sequence().unwrap().find_item(vid_clip).unwrap();
        let idx = it.effects.iter().position(|e| e.effect == "dctl_transform").expect("dctl_transform on clip");
        let keys: Vec<String> = (0..it.effects.len()).filter(|&i| i != idx).map(|i| format!("{}:{i}", vid_clip.0)).collect();
        (idx, keys)
    };
    h.state_mut().ui.collapsed_fx.extend(collapse_keys);
    h.state_mut().show_panel(PanelKind::EffectControls);
    step_frames(&mut h, 4);

    let dyn_exp = format!("effectControls.dctl_transform.{fx_idx}.dynamic.exp_ev");
    let dyn_toe = format!("effectControls.dctl_transform.{fx_idx}.dynamic.toe_on");
    assert!(!h.state().auto.query(&dyn_exp).is_empty(), "missing {dyn_exp}");
    assert!(!h.state().auto.query(&dyn_toe).is_empty(), "missing {dyn_toe}");

    // Verify OGraf Apply button and DRFX MacroBundle Apply button in Plugins panel
    let dir = tempfile::tempdir().expect("tempdir");
    let ograf_path = dir.path().join("Ticker.ograf.json");
    std::fs::write(
        &ograf_path,
        json!({
            "name": "Headline Strap",
            "schema": { "properties": { "title": { "type": "string", "default": "BREAKING" } } }
        })
        .to_string(),
    )
    .unwrap();
    let drfx_path = dir.path().join("BundleLook.drfx");
    let bundle_dctl = b"__DEVICE__ float3 transform(int p_Width, int p_Height, int p_X, int p_Y, float p_R, float p_G, float p_B) { return make_float3(p_B, p_G, p_R); }\n";
    std::fs::write(&drfx_path, store_only_zip("Looks/SwapRB.dctl", bundle_dctl)).unwrap();

    h.state_mut().session.plugins.discovered.push(filmcraft_engine::plugins::DiscoveredPlugin {
        id: "ext:ograf:ticker".into(),
        name: "Headline Strap".into(),
        kind: filmcraft_project::PluginKind::OgrafTitle,
        path: ograf_path.to_string_lossy().to_string(),
        vendor: "EBU OGraf".into(),
    });
    h.state_mut().session.plugins.discovered.push(filmcraft_engine::plugins::DiscoveredPlugin {
        id: "ext:drfx:bundle".into(),
        name: "BundleLook".into(),
        kind: filmcraft_project::PluginKind::MacroBundle,
        path: drfx_path.to_string_lossy().to_string(),
        vendor: "DRFX".into(),
    });
    page_bar::activate_page(h.state_mut(), "fusion");
    h.state_mut().ui.plugins_ui.filter = "ograf".into();
    h.state_mut().ui.plugins_ui.stage3d_open = false;
    h.state_mut().ui.plugins_ui.studio_open = false;
    step_frames(&mut h, 3);
    let ograf_btn = h.state().auto.query("plugins.apply.ext:ograf:ticker");
    assert!(!ograf_btn.is_empty(), "OGraf Apply button rendered");
    let ograf_rect = ograf_btn[0].rect;
    click_auto(&mut h, &tx, "plugins.apply.ext:ograf:ticker");
    let has_strap = h.state().session.active_sequence().unwrap().video_tracks.iter().flat_map(|t| &t.items).any(|i| i.name == "Headline Strap");
    assert!(has_strap, "OGraf Apply inserted graphic clip at playhead (rect: {ograf_rect:?}, status: {})", h.state().ui.status);

    h.state_mut().ui.plugins_ui.filter = "ofx".into();
    h.state_mut().ui.plugins_ui.search = "BundleLook".into();
    step_frames(&mut h, 3);
    assert!(!h.state().auto.query("plugins.apply.ext:drfx:bundle").is_empty(), "DRFX Apply button rendered");
    click_auto(&mut h, &tx, "plugins.apply.ext:drfx:bundle");
    assert!(
        h.state().session.project.plugins.iter().any(|p| p.name == "SwapRB"),
        "DRFX Apply imported SwapRB.dctl into project.plugins (status: {})",
        h.state().ui.status
    );
    assert!(h.state().ui_error.is_none(), "{:?}", h.state().ui_error);
}
