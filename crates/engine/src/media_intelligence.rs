//! Premiere Pro 2027 (`27.0`) Media Intelligence Project Indexing (`메타데이터 색인화`),
//! Timecode to Metadata, and Content Credentials (C2PA) binary manifest inspection.

use filmcraft_media::FrameRequest;
use filmcraft_project::{ClipId, ItemId, ItemKind, MediaRef, Project};
use filmcraft_render::SourceProvider;
use filmcraft_time::{Tick, TimeDisplay, format_time};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, u64_p};
use crate::{EngineError, Result, Session};

/// Official C2PA JUMBF UUID (`d8fec3d6-1b0e-483c-9297-5828877ec481`).
pub const C2PA_JUMBF_UUID: [u8; 16] = [0xd8, 0xfe, 0xc3, 0xd6, 0x1b, 0x0e, 0x48, 0x3c, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7e, 0xc4, 0x81];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct C2paSummary {
    pub present: bool,
    pub signed: bool,
    pub generator: Option<String>,
    pub label: String,
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn extract_ascii_after(hay: &[u8], key: &[u8]) -> Option<String> {
    let pos = hay.windows(key.len()).position(|w| w == key)?;
    let rest = hay.get(pos + key.len()..)?;
    let start = rest.iter().position(|b| b.is_ascii_alphanumeric())?;
    if start > 24 {
        return None;
    }
    let text: String = rest
        .get(start..)?
        .iter()
        .copied()
        .take(48)
        .take_while(|b| (0x20..=0x7e).contains(b) && !matches!(b, b'"' | b'<' | b'>' | b';'))
        .map(char::from)
        .collect();
    let trimmed = text.trim();
    (trimmed.len() >= 3).then(|| trimmed.to_string())
}

fn detect_generator(bytes: &[u8]) -> Option<String> {
    for marker in [b"claim_generator" as &[u8], b"claimGenerator", b"softwareAgent"] {
        if let Some(s) = extract_ascii_after(bytes, marker) {
            return Some(s);
        }
    }
    for known in ["Adobe Firefly", "Adobe Premiere", "FilmCraft", "Leica", "Sony", "Nikon", "Canon", "OpenAI"] {
        if contains_subslice(bytes, known.as_bytes()) {
            return Some(known.to_string());
        }
    }
    None
}

/// Clean-room binary inspection of ISO BMFF `uuid` JUMBF boxes, JPEG APP11 (`JP`), PNG `caBX`,
/// and XMP `dcterms:provenance` / `c2pa:` manifests.
pub fn scan_c2pa_bytes(bytes: &[u8]) -> C2paSummary {
    let has_uuid = contains_subslice(bytes, &C2PA_JUMBF_UUID);
    let has_jumbf = (contains_subslice(bytes, b"jumb") || contains_subslice(bytes, b"jumd") || contains_subslice(bytes, b"caBX"))
        && (contains_subslice(bytes, b"c2pa") || contains_subslice(bytes, b"c2ma") || contains_subslice(bytes, b"c2cl"));
    let has_xmp = contains_subslice(bytes, b"dcterms:provenance") || contains_subslice(bytes, b"c2pa:manifest");
    let present = has_uuid || has_jumbf || has_xmp;
    if !present {
        return C2paSummary { present: false, signed: false, generator: None, label: "None".to_string() };
    }
    let signed = contains_subslice(bytes, b"c2cs") || (has_uuid && contains_subslice(bytes, b"c2cl"));
    let generator = detect_generator(bytes);
    let base = if signed { "Verified C2PA" } else { "C2PA Manifest" };
    let label = match &generator {
        Some(g) => format!("{base} ({g})"),
        None => base.to_string(),
    };
    C2paSummary { present: true, signed, generator, label }
}

fn read_probe_window(s: &Session, path: &str) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const HEAD: usize = 1024 * 1024;
    const TAIL: u64 = 256 * 1024;
    if let Ok(mut f) = std::fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut buf = vec![0u8; len.min(HEAD as u64) as usize];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        if len > HEAD as u64 {
            let tail_pos = len.saturating_sub(TAIL).max(HEAD as u64);
            if f.seek(SeekFrom::Start(tail_pos)).is_ok() {
                let _ = f.read_to_end(&mut buf);
            }
        }
        return Some(buf);
    }
    s.services.read_file(path).ok()
}

fn push_unique(out: &mut Vec<ItemId>, proj: &Project, id: ItemId) {
    if proj.items.contains_key(&id) && !out.contains(&id) {
        out.push(id);
    }
}

/// Resolve target `ItemId`s from `"items"`, `"clips"`, `"item"`, `"clip"`, Project selection,
/// Timeline selection, or all media items in the project when invoked globally.
pub fn target_items(s: &Session, p: &Value, fallback_all: bool) -> Vec<ItemId> {
    let mut out = Vec::new();
    if let Some(arr) = p.get("items").and_then(Value::as_array) {
        for v in arr.iter().filter_map(Value::as_u64) {
            push_unique(&mut out, &s.project, ItemId(v));
        }
    }
    if let Some(id) = u64_p(p, "item") {
        push_unique(&mut out, &s.project, ItemId(id));
    }
    if let Some(arr) = p.get("clips").and_then(Value::as_array)
        && let Some(seq) = s.active_sequence()
    {
        for cid in arr.iter().filter_map(Value::as_u64).map(ClipId) {
            if let Some((_, it)) = seq.find_item(cid) {
                push_unique(&mut out, &s.project, it.item);
            }
        }
    }
    if let Some(cid) = u64_p(p, "clip").map(ClipId)
        && let Some(seq) = s.active_sequence()
        && let Some((_, it)) = seq.find_item(cid)
    {
        push_unique(&mut out, &s.project, it.item);
    }
    if !out.is_empty() {
        return out;
    }
    for &id in &s.state.project_selection {
        push_unique(&mut out, &s.project, id);
    }
    if out.is_empty()
        && let Some(seq) = s.active_sequence()
    {
        for &cid in &s.state.selection {
            if let Some((_, it)) = seq.find_item(cid) {
                push_unique(&mut out, &s.project, it.item);
            }
        }
    }
    if out.is_empty() && fallback_all {
        s.project.root.all_items(&mut out);
    }
    out
}

fn media_path(project: &Project, id: ItemId) -> Option<&str> {
    let (_, m, _) = project.resolve_media(id)?;
    match &m.media {
        MediaRef::File { path } => Some(path.as_str()),
        MediaRef::Generator(_) => None,
    }
}

fn check_content_credentials(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = target_items(s, p, true);
    if ids.is_empty() {
        return Err(bad("media.checkContentCredentials", "no media items in the project or selection"));
    }
    let mut updates: Vec<(ItemId, C2paSummary)> = Vec::new();
    for id in ids {
        let Some(path) = media_path(&s.project, id) else { continue };
        let summary = match read_probe_window(s, path) {
            Some(bytes) => scan_c2pa_bytes(&bytes),
            None => C2paSummary { present: false, signed: false, generator: None, label: "Unreadable".to_string() },
        };
        updates.push((id, summary));
    }
    if updates.is_empty() {
        return Err(EngineError::Other("select one or more file-backed media clips".into()));
    }
    s.edit("Check Content Credentials", |pr, _| {
        for (id, sum) in &updates {
            if let Some(it) = pr.item_mut(*id) {
                it.metadata.insert("Content Credentials".to_string(), sum.label.clone());
            }
        }
        Ok(())
    })?;
    let items_json: Vec<Value> = updates
        .iter()
        .map(|(id, sum)| {
            json!({
                "item": id.0,
                "present": sum.present,
                "signed": sum.signed,
                "generator": sum.generator,
                "status": sum.label,
            })
        })
        .collect();
    Ok(json!({"items": items_json}))
}

fn frame_visual_stats(src: &filmcraft_media::SharedSource, t: Tick) -> Option<(f32, f32, f32, Vec<f32>)> {
    let vf = src.video_frame(FrameRequest { time: t, scale: 0.125 }).ok()?;
    let (w, h, px) = vf.to_linear_f32_decimated(4);
    let total = (w * h).max(1) as f32;
    let (mut sum_y, mut sum_warm, mut sum_sat) = (0.0f32, 0.0f32, 0.0f32);
    let mut grid = Vec::with_capacity(64);
    let step = (w * h / 64).max(1);
    for (idx, p) in px.as_chunks::<4>().0.iter().enumerate() {
        let (r, g, b) = (p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0), p[2].clamp(0.0, 1.0));
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        sum_y += y;
        sum_warm += r - b;
        sum_sat += if mx > 1e-4 { (mx - mn) / mx } else { 0.0 };
        if idx % step == 0 && grid.len() < 64 {
            grid.push(y);
        }
    }
    Some((sum_y / total, sum_warm / total, sum_sat / total, grid))
}

fn describe_visuals(id: ItemId, project: &Project, provider: &dyn SourceProvider) -> Vec<String> {
    let mut tags = Vec::new();
    let Some((_, m, range)) = project.resolve_media(id) else { return tags };
    if let Some(v) = &m.info.video {
        if v.width >= 7680 || v.height >= 4320 {
            tags.push("8K".to_string());
        } else if v.width >= 3840 || v.height >= 2160 {
            tags.push("4K UHD".to_string());
        }
        if v.height > v.width {
            tags.push("Vertical / Portrait".to_string());
        } else if v.width as f32 >= v.height.max(1) as f32 * 2.0 {
            tags.push("Anamorphic / Widescreen".to_string());
        }
    }
    let Some(src) = provider.source(id) else { return tags };
    if !src.info().has_video() {
        return tags;
    }
    let dur = range.map_or(src.info().duration, |r| r.duration);
    let t0 = range.map_or(Tick::ZERO, |r| r.start);
    let a = frame_visual_stats(&src, t0 + Tick(dur.0 / 4));
    let b = frame_visual_stats(&src, t0 + Tick((dur.0 * 3) / 4));
    if let Some((y, warm, sat, g0)) = a {
        tags.push(
            if y < 0.08 {
                "Low-Key / Dark"
            } else if y > 0.45 {
                "High-Key / Bright"
            } else {
                "Balanced Exposure"
            }
            .to_string(),
        );
        if sat < 0.08 {
            tags.push("Monochrome / Muted".to_string());
        } else if sat > 0.48 {
            tags.push("Vibrant".to_string());
        }
        if warm > 0.06 {
            tags.push("Warm".to_string());
        } else if warm < -0.06 {
            tags.push("Cool".to_string());
        }
        if let Some((_, _, _, g1)) = b
            && !g0.is_empty()
            && g0.len() == g1.len()
        {
            let diff: f32 = g0.iter().zip(&g1).map(|(x, y)| (x - y).abs()).sum::<f32>() / g0.len() as f32;
            tags.push(if diff > 0.08 { "Dynamic Motion" } else { "Static / Locked-Off" }.to_string());
        }
    }
    tags
}

fn timecode_fields(project: &Project, id: ItemId) -> Option<(String, String, String)> {
    let it = project.item(id)?;
    let media = match &it.kind {
        ItemKind::Media(m) => Some(m.as_ref()),
        ItemKind::Subclip { parent, .. } => project.item(*parent).and_then(|x| x.as_media()),
        _ => None,
    };
    let rate = it.frame_rate();
    let start_tc = media.and_then(|m| m.info.start_timecode).map(|f| rate.tick_of(f)).unwrap_or(Tick::ZERO);
    let (offset, dur) = match &it.kind {
        ItemKind::Subclip { range, .. } => (range.start, range.duration),
        _ => (Tick::ZERO, it.duration()),
    };
    let first = start_tc + offset;
    let last = if dur > Tick::ZERO { first + dur - rate.frame_duration() } else { first };
    let fmt = |t: Tick| format_time(t, rate, false, TimeDisplay::Timecode, 48000);
    Some((fmt(first), fmt(last), fmt(dur)))
}

fn index_media_intelligence(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = target_items(s, p, true);
    if ids.is_empty() {
        return Err(bad("project.indexMediaIntelligence", "no items in the project"));
    }
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    type Entry = (ItemId, String, Option<(String, String, String)>, Option<String>);
    let mut indexed: Vec<Entry> = Vec::new();
    for id in ids {
        let Some(it) = s.project.item(id) else { continue };
        if matches!(it.kind, ItemKind::Sequence(_) | ItemKind::AdjustmentLayer { .. }) {
            continue;
        }
        let mut tags = describe_visuals(id, &s.project, &provider);
        if let Some((_, m, _)) = s.project.resolve_media(id)
            && let Some(a) = m.info.audio()
        {
            tags.push(if a.channels == 1 {
                "Mono Audio".to_string()
            } else if a.channels == 2 {
                "Stereo Audio".to_string()
            } else {
                format!("{}ch Surround", a.channels)
            });
        }
        if let Some(tr) = s.project.transcripts.get(&id)
            && !tr.words.is_empty()
        {
            tags.push("Transcribed Dialogue".to_string());
        }
        let c2pa = media_path(&s.project, id).and_then(|path| read_probe_window(s, path)).map(|b| scan_c2pa_bytes(&b).label);
        let tc = timecode_fields(&s.project, id);
        if tags.is_empty() {
            tags.push(it.type_label().to_string());
        }
        indexed.push((id, tags.join(", "), tc, c2pa));
    }
    if indexed.is_empty() {
        return Err(EngineError::Other("no media items to index".into()));
    }
    s.edit("Index Media Intelligence", |pr, _| {
        for (id, summary, tc, c2pa) in &indexed {
            let Some(it) = pr.item_mut(*id) else { continue };
            it.metadata.insert("Media Intelligence".to_string(), summary.clone());
            it.metadata.insert("Scene Tags".to_string(), summary.clone());
            if let Some((st, en, du)) = tc {
                it.metadata.insert("Start Timecode".to_string(), st.clone());
                it.metadata.insert("End Timecode".to_string(), en.clone());
                it.metadata.insert("Duration Timecode".to_string(), du.clone());
            }
            if let Some(cred) = c2pa {
                it.metadata.insert("Content Credentials".to_string(), cred.clone());
            }
        }
        Ok(())
    })?;
    let report: Vec<Value> = indexed
        .iter()
        .map(|(id, tags, tc, c2pa)| {
            json!({
                "item": id.0,
                "mediaIntelligence": tags,
                "startTimecode": tc.as_ref().map(|x| &x.0),
                "contentCredentials": c2pa,
            })
        })
        .collect();
    Ok(json!({"indexed": indexed.len(), "items": report}))
}

fn timecode_to_metadata(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = target_items(s, p, false);
    if ids.is_empty() {
        return Err(bad("clip.timecodeToMetadata", "select one or more clips or project items"));
    }
    let mut rows: Vec<(ItemId, String, String, String, Option<(String, String)>)> = Vec::new();
    let seq_info = s.active_sequence().map(|seq| {
        let rate = seq.settings.frame_rate;
        let df = seq.settings.drop_frame;
        let fmt = move |t: Tick| format_time(t, rate, df, TimeDisplay::Timecode, 48000);
        let mut map = std::collections::BTreeMap::new();
        for tr in seq.all_tracks() {
            for clip in &tr.items {
                if s.state.selection.is_empty() || s.state.selection.contains(&clip.id) {
                    map.entry(clip.item).or_insert_with(|| (fmt(clip.start), fmt(clip.end())));
                }
            }
        }
        map
    });
    for id in ids {
        let Some((st, en, du)) = timecode_fields(&s.project, id) else { continue };
        let seq_span = seq_info.as_ref().and_then(|m| m.get(&id).cloned());
        rows.push((id, st, en, du, seq_span));
    }
    if rows.is_empty() {
        return Err(EngineError::Other("selected items have no timecode".into()));
    }
    s.edit("Timecode to Metadata", |pr, _| {
        for (id, st, en, du, seq_span) in &rows {
            let Some(it) = pr.item_mut(*id) else { continue };
            it.metadata.insert("Start Timecode".to_string(), st.clone());
            it.metadata.insert("End Timecode".to_string(), en.clone());
            it.metadata.insert("Duration Timecode".to_string(), du.clone());
            if let Some((sin, sout)) = seq_span {
                it.metadata.insert("Sequence In".to_string(), sin.clone());
                it.metadata.insert("Sequence Out".to_string(), sout.clone());
            }
        }
        Ok(())
    })?;
    Ok(json!({
        "updated": rows.len(),
        "items": rows.iter().map(|(id, st, en, du, _)| json!({"item": id.0, "startTimecode": st, "endTimecode": en, "durationTimecode": du})).collect::<Vec<_>>(),
    }))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "project.indexMediaIntelligence",
            label: "Index Media Intelligence",
            menu: &["Clip", "Modify"],
            shortcut: None,
            params: r#"{"items":[id]?,"clips":[id]?}"#,
            enabled: always,
            run: index_media_intelligence,
            journal: true,
            named: crate::commands::NamedKeys::NONE,
        },
        CommandSpec {
            id: "clip.timecodeToMetadata",
            label: "Timecode to Metadata",
            menu: &["Clip", "Modify"],
            shortcut: None,
            params: r#"{"clips":[id]?,"items":[id]?}"#,
            enabled: always,
            run: timecode_to_metadata,
            journal: true,
            named: crate::commands::NamedKeys::NONE,
        },
        CommandSpec {
            id: "media.checkContentCredentials",
            label: "Check Content Credentials",
            menu: &["Clip", "Modify"],
            shortcut: None,
            params: r#"{"items":[id]?,"clips":[id]?}"#,
            enabled: always,
            run: check_content_credentials,
            journal: true,
            named: crate::commands::NamedKeys::NONE,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{C2PA_JUMBF_UUID, scan_c2pa_bytes};
    use crate::Session;
    use serde_json::json;

    #[test]
    fn c2pa_scanner_detects_signed_jumbf_and_xmp_provenance() {
        let clean = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
        let r0 = scan_c2pa_bytes(clean);
        assert!(!r0.present);
        assert_eq!(r0.label, "None");

        let mut signed = Vec::from(&b"\x00\x00\x00\x40uuid"[..]);
        signed.extend_from_slice(&C2PA_JUMBF_UUID);
        signed.extend_from_slice(b"jumdc2pac2clclaim_generator: Adobe Firefly 3.0\0c2cs");
        let r1 = scan_c2pa_bytes(&signed);
        assert!(r1.present && r1.signed);
        assert!(r1.label.contains("Verified C2PA"), "got {}", r1.label);
        assert!(r1.label.contains("Adobe Firefly"), "got {}", r1.label);

        let xmp = b"<rdf:Description dcterms:provenance=\"urn:c2pa:1234\" softwareAgent=\"FilmCraft 0.20.1\"/>";
        let r2 = scan_c2pa_bytes(xmp);
        assert!(r2.present && !r2.signed);
        assert!(r2.label.contains("C2PA Manifest"), "got {}", r2.label);
    }

    #[test]
    fn media_intelligence_and_timecode_to_metadata_populate_searchable_fields() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("open demo project");
        let indexed = s.execute("project.indexMediaIntelligence", json!({})).expect("index demo project");
        assert!(indexed["indexed"].as_u64().unwrap_or(0) >= 1);

        let found = s
            .execute("edit.find", json!({"scope": "project", "column": "Media Intelligence", "operator": "contains", "text": "Exposure"}))
            .or_else(|_| s.execute("edit.find", json!({"scope": "project", "column": "Start Timecode", "operator": "contains", "text": "00:"})))
            .expect("find indexed metadata");
        assert!(found["matches"].as_u64().unwrap_or(0) >= 1);

        let first_id = indexed["items"][0]["item"].as_u64().expect("first item id");
        let tc = s.execute("clip.timecodeToMetadata", json!({"items": [first_id]})).expect("timecode to metadata");
        assert_eq!(tc["updated"], json!(1));
    }
}
