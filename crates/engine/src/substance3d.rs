//! Engine commands for 3D Stage Compositing, SDF Clay Modeling, Surface Relight / PBR Material
//! Sampling, and Procedural PBR Materials:
//! - `stage3d.applyPreset`: apply a curated 3D / Material preset to selected or named video clips
//! - `material.sampleClipPbr`: evaluate 6-channel PBR statistics (`relit`, `albedo`, `normal`,
//!   `roughness`, `height`, `ao`) on the active clip/sequence frame and optionally export PNG maps.

use std::path::Path;

use filmcraft_project::{ClipId, EffectInstance, Param, ParamValue, TrackKind, find_effect, find_substance_preset};
use filmcraft_render::Image;
use filmcraft_render::stage3d::{PbrSampleParams, sample_pbr_channels};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, bad, bool_p, clip_p, f64_p, has_seq, str_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal: true, named: NamedKeys::NONE }
}

fn query(id: &'static str, label: &'static str, params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled, run, journal: false, named: NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec("stage3d.applyPreset", "Apply 3D & Material Preset", &["Clip"], r#"{"preset":str,"clips":[id]?}"#, has_seq, apply_preset),
        query(
            "material.sampleClipPbr",
            "Sample Clip PBR Material Channels",
            r#"{"clip":id?,"makeTileable":bool?,"delight":n?,"normalStrength":n?,"outputDir":str?}"#,
            has_seq,
            sample_clip_pbr,
        ),
    ]
}

fn target_video_clips(s: &Session, p: &Value, cmd: &str) -> Result<Vec<ClipId>> {
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let raw: Vec<ClipId> = if let Some(arr) = p.get("clips").and_then(Value::as_array) {
        if arr.is_empty() {
            return Err(bad(cmd, "`clips` must name at least one clip id"));
        }
        let mut ids = Vec::with_capacity(arr.len());
        for v in arr {
            let id = v.as_u64().ok_or_else(|| bad(cmd, "`clips` entries must be integer clip ids"))?;
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

fn set_param(inst: &mut EffectInstance, key: &str, val: ParamValue) {
    inst.params.insert(key.to_string(), Param::new(val));
}

fn build_preset_instance(preset_id: &str) -> Option<(&'static str, &'static str, EffectInstance)> {
    let meta = find_substance_preset(preset_id)?;
    let def = find_effect(meta.effect_id)?;
    let mut inst = def.instance();

    match meta.id {
        "gold_sphere" => {
            set_param(&mut inst, "shape", ParamValue::Choice(0));
            set_param(&mut inst, "material", ParamValue::Choice(1));
            set_param(&mut inst, "base_color", ParamValue::Color([1.0, 0.78, 0.34, 1.0]));
            set_param(&mut inst, "metallic", ParamValue::Float(0.98));
            set_param(&mut inst, "roughness", ParamValue::Float(0.12));
            set_param(&mut inst, "weathering", ParamValue::Float(0.08));
            set_param(&mut inst, "video_ibl", ParamValue::Bool(true));
            set_param(&mut inst, "shadow_catcher", ParamValue::Float(0.60));
        }
        "bottle_mockup" => {
            set_param(&mut inst, "shape", ParamValue::Choice(2));
            set_param(&mut inst, "material", ParamValue::Choice(4));
            set_param(&mut inst, "base_color", ParamValue::Color([0.94, 0.92, 0.88, 1.0]));
            set_param(&mut inst, "metallic", ParamValue::Float(0.04));
            set_param(&mut inst, "roughness", ParamValue::Float(0.14));
            set_param(&mut inst, "scale", ParamValue::Float(108.0));
            set_param(&mut inst, "rot_x", ParamValue::Float(8.0));
            set_param(&mut inst, "rot_y", ParamValue::Float(-20.0));
            set_param(&mut inst, "video_ibl", ParamValue::Bool(true));
            set_param(&mut inst, "shadow_catcher", ParamValue::Float(0.62));
        }
        "sdf_clay_sculpture" => {
            set_param(&mut inst, "shape", ParamValue::Choice(5));
            set_param(&mut inst, "material", ParamValue::Choice(6));
            set_param(&mut inst, "sdf_blend", ParamValue::Float(0.28));
            set_param(&mut inst, "weathering", ParamValue::Float(0.35));
            set_param(&mut inst, "rot_x", ParamValue::Float(18.0));
            set_param(&mut inst, "rot_y", ParamValue::Float(-30.0));
            set_param(&mut inst, "video_ibl", ParamValue::Bool(true));
            set_param(&mut inst, "shadow_catcher", ParamValue::Float(0.58));
        }
        "relight_warm_key" => {
            set_param(&mut inst, "output_mode", ParamValue::Choice(0));
            set_param(&mut inst, "delight", ParamValue::Float(0.70));
            set_param(&mut inst, "normal_strength", ParamValue::Float(1.8));
            set_param(&mut inst, "light_x", ParamValue::Float(0.42));
            set_param(&mut inst, "light_y", ParamValue::Float(0.38));
            set_param(&mut inst, "light_z", ParamValue::Float(0.58));
            set_param(&mut inst, "light_color", ParamValue::Color([1.0, 0.88, 0.70, 1.0]));
            set_param(&mut inst, "intensity", ParamValue::Float(1.65));
            set_param(&mut inst, "roughness", ParamValue::Float(0.30));
        }
        "pbr_normal_map" => {
            set_param(&mut inst, "output_mode", ParamValue::Choice(2));
            set_param(&mut inst, "delight", ParamValue::Float(0.55));
            set_param(&mut inst, "normal_strength", ParamValue::Float(2.2));
        }
        "seamless_albedo" => {
            set_param(&mut inst, "output_mode", ParamValue::Choice(1));
            set_param(&mut inst, "delight", ParamValue::Float(0.80));
            set_param(&mut inst, "make_tileable", ParamValue::Bool(true));
        }
        "brushed_steel" => {
            set_param(&mut inst, "pattern", ParamValue::Choice(0));
            set_param(&mut inst, "scale", ParamValue::Float(2.5));
            set_param(&mut inst, "metallic", ParamValue::Float(0.92));
            set_param(&mut inst, "roughness", ParamValue::Float(0.24));
            set_param(&mut inst, "weathering", ParamValue::Float(0.18));
        }
        "terrazzo_marble" => {
            set_param(&mut inst, "pattern", ParamValue::Choice(2));
            set_param(&mut inst, "scale", ParamValue::Float(2.2));
            set_param(&mut inst, "metallic", ParamValue::Float(0.05));
            set_param(&mut inst, "roughness", ParamValue::Float(0.18));
            set_param(&mut inst, "weathering", ParamValue::Float(0.12));
        }
        "carbon_hex_weave" => {
            set_param(&mut inst, "pattern", ParamValue::Choice(1));
            set_param(&mut inst, "scale", ParamValue::Float(3.2));
            set_param(&mut inst, "metallic", ParamValue::Float(0.35));
            set_param(&mut inst, "roughness", ParamValue::Float(0.22));
            set_param(&mut inst, "weathering", ParamValue::Float(0.10));
        }
        _ => return None,
    }
    Some((meta.id, meta.effect_id, inst))
}

fn apply_preset(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "stage3d.applyPreset";
    let preset_raw = str_p(p, "preset").ok_or_else(|| bad(CMD, "need `preset`"))?;
    let (canonical_id, fx_id, template) = build_preset_instance(preset_raw).ok_or_else(|| {
        bad(
            CMD,
            format!(
                "unknown preset `{preset_raw}` (expected gold_sphere, bottle_mockup, sdf_clay_sculpture, relight_warm_key, pbr_normal_map, seamless_albedo, brushed_steel, terrazzo_marble, or carbon_hex_weave)"
            ),
        )
    })?;
    let clips = target_video_clips(s, p, CMD)?;
    let first = *clips.first().ok_or_else(|| bad(CMD, "select at least one video clip"))?;
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let mut inserted_index = 0_usize;

    s.edit("Apply 3D & Material Preset", |pr, state| {
        let Some(seq) = pr.sequence_mut(seq_id) else { return Ok(()) };
        for cid in &clips {
            if let Some((_, item)) = seq.find_item_mut(*cid) {
                let insert_at = item.effects.iter().position(|ef| ef.def().is_some_and(|d| d.intrinsic)).unwrap_or(item.effects.len());
                item.effects.insert(insert_at, template.clone());
                if *cid == first {
                    inserted_index = insert_at;
                }
            }
        }
        if let Some(seq_ref) = pr.sequence(seq_id)
            && let Some((_, item)) = seq_ref.find_item(first)
        {
            state.selected_effect = crate::EffectSel::of(first, &item.effects, inserted_index);
        }
        Ok(())
    })?;

    Ok(json!({
        "preset": canonical_id,
        "effect": fx_id,
        "appliedClips": clips.len(),
        "clip": first.0,
        "index": inserted_index,
    }))
}

fn channel_stats_rgb(img: &Image) -> Value {
    let n = (img.w * img.h).max(1) as f64;
    let mut sum = [0.0_f64; 3];
    let mut min_v = [1.0_f32; 3];
    let mut max_v = [0.0_f32; 3];
    for y in 0..img.h {
        for x in 0..img.w {
            let c = Image::unpremul(img.get(x, y));
            for k in 0..3 {
                let v = if c[k].is_finite() { c[k] } else { 0.0 };
                sum[k] += f64::from(v);
                min_v[k] = min_v[k].min(v);
                max_v[k] = max_v[k].max(v);
            }
        }
    }
    let rnd = |v: f64| (v * 10_000.0).round() / 10_000.0;
    json!({
        "mean": [rnd(sum[0] / n), rnd(sum[1] / n), rnd(sum[2] / n)],
        "min": [rnd(f64::from(min_v[0])), rnd(f64::from(min_v[1])), rnd(f64::from(min_v[2]))],
        "max": [rnd(f64::from(max_v[0])), rnd(f64::from(max_v[1])), rnd(f64::from(max_v[2]))],
    })
}

fn linear_data_rgba8(img: &Image) -> Vec<u8> {
    let mut out = vec![0_u8; img.w * img.h * 4];
    for (dst, src) in out.as_chunks_mut::<4>().0.iter_mut().zip(img.px.as_chunks::<4>().0) {
        let c = Image::unpremul(*src);
        dst[0] = (c[0].clamp(0.0, 1.0) * 255.0).round() as u8;
        dst[1] = (c[1].clamp(0.0, 1.0) * 255.0).round() as u8;
        dst[2] = (c[2].clamp(0.0, 1.0) * 255.0).round() as u8;
        dst[3] = (src[3].clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    out
}

fn export_pbr_pngs(s: &Session, out_dir: &str, maps: &filmcraft_render::stage3d::SampledPbrMaps) -> Result<Vec<String>> {
    let base = Path::new(out_dir);
    let w = maps.width as u32;
    let h = maps.height as u32;
    let entries: [(&str, Vec<u8>); 6] = [
        ("albedo.png", maps.albedo.over_black_rgba8()),
        ("normal.png", linear_data_rgba8(&maps.normal)),
        ("roughness.png", linear_data_rgba8(&maps.roughness)),
        ("height.png", linear_data_rgba8(&maps.height_map)),
        ("ao.png", linear_data_rgba8(&maps.ao)),
        ("relit.png", maps.relit.over_black_rgba8()),
    ];
    let mut written = Vec::with_capacity(entries.len());
    for (file_name, rgba) in entries {
        let bytes = filmcraft_export::encode_png(rgba, w, h).map_err(|e| EngineError::Other(e.to_string()))?;
        let full_path = base.join(file_name).to_string_lossy().to_string();
        s.services.write_file(&full_path, &bytes).map_err(|e| EngineError::Other(format!("{full_path}: {e}")))?;
        written.push(full_path);
    }
    Ok(written)
}

fn sample_clip_pbr(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "material.sampleClipPbr";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let seq = s.project.sequence(seq_id).ok_or(EngineError::NoSequence)?;

    let delight = f64_p(p, "delight").unwrap_or(0.65);
    if !delight.is_finite() || !(0.0..=1.0).contains(&delight) {
        return Err(bad(CMD, "`delight` must be between 0.0 and 1.0"));
    }
    let normal_strength = f64_p(p, "normalStrength").unwrap_or(1.5);
    if !normal_strength.is_finite() || !(0.1..=5.0).contains(&normal_strength) {
        return Err(bad(CMD, "`normalStrength` must be between 0.1 and 5.0"));
    }
    let make_tileable = bool_p(p, "makeTileable").unwrap_or(false);
    let out_dir = match p.get("outputDir") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let dir = v.as_str().ok_or_else(|| bad(CMD, "`outputDir` must be a string path"))?.trim();
            if dir.is_empty() {
                return Err(bad(CMD, "`outputDir` cannot be empty"));
            }
            Some(dir.to_string())
        }
    };

    // Resolve optional target clip (or first selected video clip) to clamp sample time into its span
    let chosen_clip = if let Some(cid) = clip_p(p, "clip") {
        let (tid, _) = seq.find_item(cid).ok_or_else(|| bad(CMD, format!("no clip {} in the active sequence", cid.0)))?;
        if seq.track(tid).is_none_or(|t| t.kind != TrackKind::Video) {
            return Err(bad(CMD, "clip must be on a video track"));
        }
        Some(cid)
    } else {
        s.state.selection.iter().copied().find(|&cid| seq.find_item(cid).and_then(|(tid, _)| seq.track(tid)).is_some_and(|t| t.kind == TrackKind::Video))
    };

    let mut sample_time = s.playhead();
    if let Some(cid) = chosen_clip
        && let Some((_, it)) = seq.find_item(cid)
        && it.duration > Tick::ZERO
    {
        sample_time = sample_time.clamp(it.start, it.end() - Tick(1));
    }

    let max_dim = seq.settings.width.max(seq.settings.height).max(1);
    let scale = (384.0 / max_dim as f32).clamp(1.0 / 16.0, 1.0);
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let opts = filmcraft_render::RenderOptions { scale, effects: false, ..Default::default() };
    let frame_img = filmcraft_render::render_sequence(&s.project, seq_id, sample_time, opts, &provider);

    let sample_params = PbrSampleParams { delight: delight as f32, normal_strength: normal_strength as f32, make_tileable, ..Default::default() };
    let maps = sample_pbr_channels(&frame_img, &sample_params);
    let exported = match out_dir.as_deref() {
        Some(dir) => export_pbr_pngs(s, dir, &maps)?,
        None => Vec::new(),
    };

    Ok(json!({
        "width": maps.width,
        "height": maps.height,
        "makeTileable": make_tileable,
        "delight": delight,
        "normalStrength": normal_strength,
        "borderSeamRmse": (f64::from(maps.border_seam_rmse) * 10_000.0).round() / 10_000.0,
        "channels": {
            "relit": channel_stats_rgb(&maps.relit),
            "albedo": channel_stats_rgb(&maps.albedo),
            "normal": channel_stats_rgb(&maps.normal),
            "roughness": channel_stats_rgb(&maps.roughness),
            "height": channel_stats_rgb(&maps.height_map),
            "ao": channel_stats_rgb(&maps.ao),
        },
        "exportedFiles": exported,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_preset_and_sample_clip_pbr_round_trip_and_reject_hostile_params() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");
        let seq = s.active_sequence().expect("active sequence");
        let vid_clip = seq.video_tracks.iter().flat_map(|t| t.items.iter()).next().expect("video clip").id;
        s.state.selection = vec![vid_clip];

        for (preset, expected_fx) in [
            ("gold_sphere", "stage_3d"),
            ("bottle_mockup", "stage_3d"),
            ("sdf_clay_sculpture", "stage_3d"),
            ("relight_warm_key", "surface_relight"),
            ("pbr_normal_map", "surface_relight"),
            ("seamless_albedo", "surface_relight"),
            ("brushed_steel", "procedural_pbr"),
            ("terrazzo_marble", "procedural_pbr"),
            ("carbon_hex_weave", "procedural_pbr"),
        ] {
            let res = s.execute("stage3d.applyPreset", json!({ "preset": preset })).expect(preset);
            assert_eq!(res["effect"], json!(expected_fx));
            assert_eq!(res["appliedClips"], json!(1));
        }

        // Reject unknown preset & out-of-range delight
        assert!(s.execute("stage3d.applyPreset", json!({ "preset": "nonexistent_preset" })).is_err());
        assert!(s.execute("material.sampleClipPbr", json!({ "delight": 99.0 })).is_err());
        assert!(s.execute("material.sampleClipPbr", json!({ "outputDir": "   " })).is_err());

        // Evaluate PBR statistics and export 6 PNG channels into a temp directory
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_dir = tmp.path().to_string_lossy().to_string();
        let sampled = s.execute("material.sampleClipPbr", json!({ "makeTileable": true, "delight": 0.75, "outputDir": out_dir })).expect("sampleClipPbr");
        assert!(sampled["width"].as_u64().unwrap() > 0);
        assert_eq!(sampled["makeTileable"], json!(true));
        let files = sampled["exportedFiles"].as_array().expect("exportedFiles");
        assert_eq!(files.len(), 6);
        for f in files {
            let path = f.as_str().expect("path string");
            let bytes = std::fs::read(path).expect("PNG exists");
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{path} is valid PNG");
        }
    }
}
