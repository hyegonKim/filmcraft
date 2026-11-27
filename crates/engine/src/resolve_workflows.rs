//! DaVinci Resolve Cut Page & Media Page workflows:
//! - [`cut.sourceTape`] & [`cut.sourceTapeSeek`]: Virtual concatenated review tape across bin media
//! - [`cut.fastReview`]: Adaptive clip-length playback speed (`1.0x .. 8.0x`)
//! - [`media.cloneVerify`]: Bounded file offload with pure-Rust FIPS 180-4 SHA-256 verification
//! - [`project.smartBin`]: Metadata & technical rule evaluation with non-destructive Smart Bin creation

use std::path::Path;

use filmcraft_project::{BinId, ClipId, FindOp, FindQuery, FindRow, ItemId, ItemKind, Label, MediaRef, ProjectItem, SearchBin};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, bool_p, clip_p, has_seq, str_p, u64_p};
use crate::{Event, Result, Session};

const MAX_CLONE_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CLONE_BATCH_FILES: usize = 128;

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
        query("cut.sourceTape", "Inspect Source Tape", r#"{"bin":n?}"#, always, source_tape),
        spec("cut.sourceTapeSeek", "Seek Source Tape", &[], r#"{"position":n,"bin":n?}"#, always, source_tape_seek),
        query("cut.fastReview", "Compute Fast Review Speed", r#"{"clip":id?}"#, has_seq, fast_review),
        spec("media.cloneVerify", "Clone & Checksum Media", &["File"], r#"{"sources":[str],"destinationDir":str}"#, always, clone_verify),
        spec(
            "project.smartBin",
            "Evaluate or Create Smart Bin",
            &["File", "New"],
            r#"{"name":str?,"query":str?,"mediaKind":str?,"minWidth":n?,"label":str?,"createBin":bool?}"#,
            always,
            smart_bin,
        ),
    ]
}

// ---------------------------------------------------------------------------
// 1. Cut Page: Source Tape (`cut.sourceTape` & `cut.sourceTapeSeek`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct TapeSeg {
    item: ItemId,
    name: String,
    source_start: Tick,
    duration: Tick,
    tape_start: Tick,
    tape_end: Tick,
}

fn playable_tape_range(item: &ProjectItem) -> Option<(Tick, Tick)> {
    match &item.kind {
        ItemKind::Media(m) => {
            let dur = m.duration();
            if dur <= Tick::ZERO {
                return None;
            }
            let s0 = m.mark_in.unwrap_or(Tick::ZERO).clamp(Tick::ZERO, dur);
            let s1 = m.mark_out.unwrap_or(dur).clamp(s0, dur);
            let span = if s1 > s0 { s1 - s0 } else { dur };
            Some((if s1 > s0 { s0 } else { Tick::ZERO }, span))
        }
        ItemKind::Subclip { range, .. } if range.duration > Tick::ZERO => Some((range.start, range.duration)),
        _ => None,
    }
}

fn build_source_tape(s: &Session, p: &Value) -> Vec<TapeSeg> {
    let mut candidate_ids: Vec<ItemId> = Vec::new();
    let explicit_bin = u64_p(p, "bin").map(BinId);
    let selected_bin = if explicit_bin.is_none() && s.state.project_selection.len() == 1 {
        let sel = BinId(s.state.project_selection[0].0);
        s.project.root.find_bin(sel).map(|_| sel)
    } else {
        None
    };

    if let Some(bid) = explicit_bin.or(selected_bin) {
        if let Some(bin) = s.project.root.find_bin(bid) {
            bin.all_items(&mut candidate_ids);
        }
    } else {
        s.project.root.all_items(&mut candidate_ids);
        for &id in s.project.items.keys() {
            if !candidate_ids.contains(&id) {
                candidate_ids.push(id);
            }
        }
    }

    let mut cursor = Tick::ZERO;
    let mut segments = Vec::new();
    for id in candidate_ids {
        let Some(it) = s.project.item(id) else { continue };
        let Some((src_start, dur)) = playable_tape_range(it) else { continue };
        let tape_end = Tick(cursor.0.saturating_add(dur.0));
        segments.push(TapeSeg { item: id, name: it.name.clone(), source_start: src_start, duration: dur, tape_start: cursor, tape_end });
        cursor = tape_end;
    }
    segments
}

fn source_tape(s: &mut Session, p: &Value) -> Result<Value> {
    let segs = build_source_tape(s, p);
    let total = segs.last().map_or(Tick::ZERO, |last| last.tape_end);
    let rows: Vec<Value> = segs
        .iter()
        .map(|seg| {
            json!({
                "item": seg.item.0,
                "name": seg.name,
                "tapeStart": seg.tape_start.0,
                "tapeEnd": seg.tape_end.0,
                "duration": seg.duration.0,
                "durationSeconds": seg.duration.seconds(),
            })
        })
        .collect();
    Ok(json!({
        "totalTicks": total.0,
        "totalSeconds": total.seconds(),
        "segmentCount": rows.len(),
        "segments": rows,
    }))
}

fn parse_tape_position(p: &Value, cmd: &str) -> Result<Tick> {
    if let Some(secs) = p.get("seconds").and_then(Value::as_f64) {
        if !secs.is_finite() {
            return Err(bad(cmd, "`seconds` must be finite"));
        }
        return Ok(Tick::from_seconds_f64(secs.max(0.0)));
    }
    let Some(pos_val) = p.get("position") else {
        return Err(bad(cmd, "need `position` (ticks or seconds)"));
    };
    if let Some(ticks) = pos_val.as_i64() {
        return Ok(Tick(ticks.max(0)));
    }
    if let Some(ticks_u) = pos_val.as_u64() {
        return Ok(Tick(i64::try_from(ticks_u).unwrap_or(i64::MAX)));
    }
    if let Some(secs_f) = pos_val.as_f64() {
        if !secs_f.is_finite() {
            return Err(bad(cmd, "`position` must be finite"));
        }
        return Ok(Tick::from_seconds_f64(secs_f.max(0.0)));
    }
    Err(bad(cmd, "`position` must be a number"))
}

fn source_tape_seek(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "cut.sourceTapeSeek";
    let pos = parse_tape_position(p, CMD)?;
    let segs = build_source_tape(s, p);
    let Some(last) = segs.last() else {
        return Err(bad(CMD, "no playable media items for Source Tape"));
    };
    let clamped = pos.clamp(Tick::ZERO, last.tape_end);
    let chosen = segs.iter().find(|seg| clamped >= seg.tape_start && clamped < seg.tape_end).unwrap_or(last);
    let offset = (clamped - chosen.tape_start).clamp(Tick::ZERO, chosen.duration);
    let source_in = Tick(chosen.source_start.0.saturating_add(offset.0));

    s.state.source_item = Some(chosen.item);
    s.state.source_playhead = source_in;
    crate::source_monitor::track_history(s);
    s.events.push(Event::OpenSource(chosen.item));

    Ok(json!({
        "item": chosen.item.0,
        "name": chosen.name,
        "sourceIn": source_in.0,
        "sourceSeconds": source_in.seconds(),
        "tapePosition": clamped.0,
    }))
}

// ---------------------------------------------------------------------------
// 2. Cut Page: Fast Review (`cut.fastReview`)
// ---------------------------------------------------------------------------

/// Compute adaptive playback speed (`1.0..=8.0`) from clip duration in seconds.
pub fn adaptive_fast_review_speed(duration_seconds: f64) -> f64 {
    if !duration_seconds.is_finite() || duration_seconds <= 3.0 {
        return 1.0;
    }
    if duration_seconds >= 60.0 {
        return 8.0;
    }
    let norm = ((duration_seconds - 3.0) / 57.0).clamp(0.0, 1.0);
    let raw = 1.0 + 7.0 * norm.powf(0.75);
    ((raw * 100.0).round() / 100.0).clamp(1.0, 8.0)
}

fn fast_review(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "cut.fastReview";
    let seq = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let target_id: Option<ClipId> = if let Some(cid) = clip_p(p, "clip") {
        Some(cid)
    } else if let Some(&sel) = s.state.selection.first() {
        Some(sel)
    } else {
        let ph = s.playhead();
        seq.video_tracks
            .iter()
            .rev()
            .find_map(|tr| tr.item_at(ph).map(|it| it.id))
            .or_else(|| seq.audio_tracks.iter().find_map(|tr| tr.item_at(ph).map(|it| it.id)))
    };

    let cid = target_id.ok_or_else(|| bad(CMD, "no clip selected or under playhead"))?;
    let (_, item) = seq.find_item(cid).ok_or_else(|| bad(CMD, format!("no clip {}", cid.0)))?;
    let dur_s = item.duration.seconds().max(0.0);
    let speed = adaptive_fast_review_speed(dur_s);
    Ok(json!({
        "clip": item.id.0,
        "name": item.name,
        "durationSeconds": dur_s,
        "speed": speed,
    }))
}

// ---------------------------------------------------------------------------
// 3. Media Page: Clone Tool with Pure-Rust FIPS 180-4 SHA-256 (`media.cloneVerify`)
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74,
    0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d,
    0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e,
    0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (i, slot) in w.iter_mut().take(16).enumerate() {
        let b = i * 4;
        *slot = u32::from_be_bytes([block[b], block[b + 1], block[b + 2], block[b + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Pure-Rust FIPS 180-4 SHA-256 hex digest.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let (chunks, rem) = data.as_chunks::<64>();
    for blk in chunks {
        sha256_compress(&mut state, blk);
    }

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let total_tail = if rem.len() < 56 { 64 } else { 128 };
    tail[total_tail - 8..total_tail].copy_from_slice(&bit_len.to_be_bytes());

    let mut first_blk = [0u8; 64];
    first_blk.copy_from_slice(&tail[..64]);
    sha256_compress(&mut state, &first_blk);
    if total_tail == 128 {
        let mut second_blk = [0u8; 64];
        second_blk.copy_from_slice(&tail[64..128]);
        sha256_compress(&mut state, &second_blk);
    }

    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for word in state {
        for byte in word.to_be_bytes() {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

fn clone_verify(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "media.cloneVerify";
    let srcs = p.get("sources").and_then(Value::as_array).ok_or_else(|| bad(CMD, "need `sources` array"))?;
    if srcs.is_empty() || srcs.len() > MAX_CLONE_BATCH_FILES {
        return Err(bad(CMD, format!("`sources` must contain 1..={MAX_CLONE_BATCH_FILES} file paths")));
    }
    let dest_dir = str_p(p, "destinationDir").map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| bad(CMD, "need non-empty `destinationDir`"))?;

    let mut manifest = Vec::with_capacity(srcs.len());
    let mut all_verified = true;

    for entry in srcs {
        let src_path = entry.as_str().map(str::trim).filter(|st| !st.is_empty()).ok_or_else(|| bad(CMD, "every source must be a non-empty path"))?;
        let declared_sz = s.services.file_size(src_path).map_err(|e| bad(CMD, format!("cannot stat `{src_path}`: {e}")))?;
        if declared_sz > MAX_CLONE_FILE_BYTES {
            return Err(bad(CMD, format!("`{src_path}` ({declared_sz} bytes) exceeds {MAX_CLONE_FILE_BYTES}-byte limit")));
        }
        let src_bytes = s.services.read_file(src_path).map_err(|e| bad(CMD, format!("cannot read `{src_path}`: {e}")))?;
        if src_bytes.len() as u64 > MAX_CLONE_FILE_BYTES {
            return Err(bad(CMD, format!("`{src_path}` exceeds {MAX_CLONE_FILE_BYTES}-byte limit")));
        }

        let file_name = Path::new(src_path)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .ok_or_else(|| bad(CMD, format!("invalid source filename in `{src_path}`")))?;
        let dst_buf = Path::new(dest_dir).join(file_name);
        let dst_str = dst_buf.to_string_lossy().to_string();
        if dst_str == src_path {
            return Err(bad(CMD, format!("destination `{dst_str}` matches source path")));
        }

        let src_sha = sha256_hex(&src_bytes);
        s.services.write_file(&dst_str, &src_bytes).map_err(|e| bad(CMD, format!("cannot write `{dst_str}`: {e}")))?;
        let verify_bytes = s.services.read_file(&dst_str).map_err(|e| bad(CMD, format!("cannot re-read `{dst_str}` for verification: {e}")))?;
        let dst_sha = sha256_hex(&verify_bytes);
        let matched = src_bytes.len() == verify_bytes.len() && src_sha == dst_sha;
        if !matched {
            all_verified = false;
        }
        manifest.push(json!({
            "source": src_path,
            "destination": dst_str,
            "bytes": src_bytes.len(),
            "sha256": src_sha,
            "destinationSha256": dst_sha,
            "matched": matched,
        }));
    }

    Ok(json!({
        "verified": all_verified,
        "files": manifest,
    }))
}

// ---------------------------------------------------------------------------
// 4. Media Page: Smart Bins (`project.smartBin`)
// ---------------------------------------------------------------------------

fn item_video_width(project: &filmcraft_project::Project, item: &ProjectItem) -> Option<u32> {
    match &item.kind {
        ItemKind::Media(m) => m.info.video.as_ref().map(|v| v.width),
        ItemKind::Sequence(q) => Some(q.settings.width),
        ItemKind::Subclip { parent, .. } => project.item(*parent).and_then(|p| item_video_width(project, p)),
        ItemKind::AdjustmentLayer { width, .. } | ItemKind::Graphic { width, .. } => Some(*width),
    }
}

fn matches_smart_rules(
    project: &filmcraft_project::Project,
    item: &ProjectItem,
    query_lc: Option<&str>,
    kind_lc: Option<&str>,
    min_width: Option<u32>,
    label_lc: Option<&str>,
) -> bool {
    if matches!(item.kind, ItemKind::Graphic { .. }) && !project.source_graphics.contains_key(&item.id) {
        return false;
    }
    if let Some(q) = query_lc {
        let name_hit = item.name.to_lowercase().contains(q);
        let meta_hit = item.metadata.values().any(|v| v.to_lowercase().contains(q));
        let path_hit = match &item.kind {
            ItemKind::Media(m) => match &m.media {
                MediaRef::File { path } => path.to_lowercase().contains(q),
                MediaRef::Generator(_) => false,
            },
            _ => false,
        };
        if !name_hit && !meta_hit && !path_hit {
            return false;
        }
    }
    if let Some(k) = kind_lc {
        let tl = item.type_label().to_ascii_lowercase();
        let ok = match k {
            "video" | "movie" => tl == "movie" || (item.has_video() && matches!(item.kind, ItemKind::Media(_))),
            "audio" | "audioonly" => tl == "audio" || (!item.has_video() && item.has_audio()),
            "still" | "image" | "stillimage" => tl == "still image",
            "sequence" => tl.contains("sequence"),
            "subclip" => tl == "subclip",
            other => tl.contains(other),
        };
        if !ok {
            return false;
        }
    }
    if let Some(mw) = min_width
        && item_video_width(project, item).is_none_or(|w| w < mw)
    {
        return false;
    }
    if let Some(lbl) = label_lc {
        let item_lbl = format!("{:?}", item.label).to_ascii_lowercase();
        if item_lbl != lbl {
            return false;
        }
    }
    true
}

fn smart_bin(s: &mut Session, p: &Value) -> Result<Value> {
    let bin_name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).unwrap_or("Smart Bin").to_string();
    let query_lc = str_p(p, "query").map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
    let kind_lc = str_p(p, "mediaKind").map(str::trim).filter(|k| !k.is_empty()).map(|k| k.to_ascii_lowercase().replace([' ', '_', '-'], ""));
    let min_width = u64_p(p, "minWidth").map(|w| w.min(u64::from(u32::MAX)) as u32);
    let label_lc = str_p(p, "label").map(str::trim).filter(|l| !l.is_empty()).map(|l| l.to_ascii_lowercase());
    let create_bin = bool_p(p, "createBin").unwrap_or(false);

    let matched: Vec<ItemId> = s
        .project
        .items
        .values()
        .filter(|it| matches_smart_rules(&s.project, it, query_lc.as_deref(), kind_lc.as_deref(), min_width, label_lc.as_deref()))
        .map(|it| it.id)
        .collect();

    let mut created_bin: Option<BinId> = None;
    let mut created_refs: Vec<u64> = Vec::new();

    if create_bin {
        let q_text = query_lc.clone();
        let bname = bin_name.clone();
        let matched_snapshot = matched.clone();
        s.edit("Create Smart Bin", |pr, _| {
            let bid = pr.add_bin(&bname, None);
            created_bin = Some(bid);
            for mid in &matched_snapshot {
                let Some((root_media, mc, sub_range)) = pr.resolve_media(*mid) else { continue };
                let dur = mc.duration();
                if dur <= Tick::ZERO {
                    continue;
                }
                let range = sub_range.unwrap_or_else(|| TimeRange::new(Tick::ZERO, dur));
                let orig_name = pr.item(*mid).map_or_else(|| "Smart Ref".to_string(), |i| i.name.clone());
                let orig_label = pr.item(*mid).map_or(Label::Cerulean, |i| i.label);
                let ref_id = pr.add_item(&orig_name, orig_label, ItemKind::Subclip { parent: root_media, range, restrict_trims: false }, Some(bid));
                created_refs.push(ref_id.0);
            }
            if let Some(qt) = &q_text {
                let sbid = BinId(pr.alloc_id());
                pr.search_bins.push(SearchBin {
                    id: sbid,
                    name: bname.clone(),
                    query: FindQuery {
                        rows: vec![FindRow { column: "All".into(), op: FindOp::Contains, text: qt.clone() }],
                        match_all: true,
                        case_sensitive: false,
                    },
                });
            }
            Ok(())
        })?;
    }

    let matched_ids: Vec<u64> = matched.iter().map(|i| i.0).collect();
    Ok(json!({
        "name": bin_name,
        "matched": matched_ids,
        "count": matched_ids.len(),
        "bin": created_bin.map(|b| b.0),
        "referenceItems": created_refs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips_180_4_sha256_matches_nist_vectors_and_clone_verifies_files() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");

        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("camera_card");
        let dst_dir = dir.path().join("raid_backup");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let clip_a = src_dir.join("A001_C001.mov");
        let clip_b = src_dir.join("A001_C002.wav");
        std::fs::write(&clip_a, b"camera-card-payload-alpha-0123456789").unwrap();
        std::fs::write(&clip_b, vec![0x5a_u8; 4096]).unwrap();

        let mut s = Session::default();
        let res = s
            .execute(
                "media.cloneVerify",
                json!({
                    "sources": [clip_a.to_string_lossy(), clip_b.to_string_lossy()],
                    "destinationDir": dst_dir.to_string_lossy(),
                }),
            )
            .expect("cloneVerify");
        assert_eq!(res["verified"], json!(true));
        let files = res["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["matched"], json!(true));
        assert_eq!(files[1]["bytes"], json!(4096));
    }

    #[test]
    fn source_tape_fast_review_and_smart_bin_workflows_operate_cleanly() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");

        // 1. Source Tape query & seek
        let tape = s.execute("cut.sourceTape", json!({})).expect("sourceTape");
        let total = tape["totalTicks"].as_i64().unwrap();
        let segs = tape["segments"].as_array().unwrap();
        assert!(!segs.is_empty() && total > 0);

        let mid_pos = total / 2;
        let seek = s.execute("cut.sourceTapeSeek", json!({ "position": mid_pos })).expect("sourceTapeSeek");
        let opened_item = ItemId(seek["item"].as_u64().unwrap());
        assert_eq!(s.state.source_item, Some(opened_item));

        // 2. Fast Review speed scales monotonically from 1.0x up to 8.0x
        assert_eq!(adaptive_fast_review_speed(1.5), 1.0);
        assert!(adaptive_fast_review_speed(15.0) > 2.0 && adaptive_fast_review_speed(15.0) < 8.0);
        assert_eq!(adaptive_fast_review_speed(90.0), 8.0);

        let first_clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
        let fr = s.execute("cut.fastReview", json!({ "clip": first_clip.0 })).expect("fastReview");
        let spd = fr["speed"].as_f64().unwrap();
        assert!((1.0..=8.0).contains(&spd));

        // 3. Smart Bin metadata/resolution filter + non-destructive Bin creation
        let sb =
            s.execute("project.smartBin", json!({ "name": "HD Video Selects", "mediaKind": "movie", "minWidth": 640, "createBin": true })).expect("smartBin");
        assert!(sb["count"].as_u64().unwrap() >= 1);
        let bin_id = BinId(sb["bin"].as_u64().expect("created bin id"));
        assert!(s.project.root.find_bin(bin_id).is_some());
    }
}
