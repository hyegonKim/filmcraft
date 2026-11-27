//! DaVinci Resolve 21 AI Workflows:
//! - [`ai.intelliScript`]: Text-script-to-rough-cut sequence assembler using normalized token
//!   overlap and order-preserving Dynamic Programming over project transcripts, clip names & markers.
//! - [`multicam.smartSwitch`]: Per-angle RMS audio energy speaker switcher with hysteresis,
//!   minimum cut hold (`minCutSeconds`), and deterministic camera fallback in one undoable pass.
//! - [`audio.matchDialogue`]: 3-band RBJ biquad spectral energy (`<250 Hz`, `250..2500 Hz`,
//!   `2.5..8 kHz`) and integrated RMS/LUFS matcher applying clip gain, `parametric_eq`, and
//!   `dialogue_leveler`.

use std::f64::consts::{FRAC_1_SQRT_2, PI};

use filmcraft_edit as edit;
use filmcraft_edit::multicam::Cut;
use filmcraft_project::transcript::normalize_word;
use filmcraft_project::{ClipId, EffectInstance, ItemId, ItemKind, Param, ParamValue, ProjectItem, SequenceSettings, TrackKind, Transcript, find_effect};
use filmcraft_render::SourceProvider;
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, clip_p, f64_p, has_seq, str_p};
use crate::{EngineError, Event, Result, Session};

const MAX_SCRIPT_LINES: usize = 512;
const MAX_ANALYZE_SECONDS: f64 = 10.0;

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal: true, named: NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec("ai.intelliScript", "Create Rough Cut from Script…", &["Sequence"], r#"{"script":str,"sequenceName":str?}"#, always, intelli_script),
        spec(
            "multicam.smartSwitch",
            "Smart Multicam Audio Switch",
            &["Clip", "Multi-Camera"],
            r#"{"clip":id?,"minCutSeconds":n?,"thresholdDb":n?}"#,
            has_seq,
            smart_switch,
        ),
        spec("audio.matchDialogue", "Match Dialogue Tone & Loudness", &["Clip", "Audio Options"], r#"{"reference":n,"targets":[n]?}"#, has_seq, match_dialogue),
    ]
}

struct SessionSourceProvider<'a> {
    project: &'a filmcraft_project::Project,
    media: &'a crate::MediaPool,
    services: &'a dyn crate::Services,
}

impl<'a> SessionSourceProvider<'a> {
    fn from_session(s: &'a Session) -> Self {
        Self { project: &s.project, media: &s.media, services: &*s.services }
    }
}

impl SourceProvider for SessionSourceProvider<'_> {
    fn source(&self, item: ItemId) -> Option<filmcraft_media::SharedSource> {
        self.media.source_for(self.project, item, self.services)
    }
}

// ---------------------------------------------------------------------------
// 1. `ai.intelliScript` — Script-to-Timeline Rough Cut via Order-Preserving DP
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ScriptCandidate {
    item: ItemId,
    range: TimeRange,
    tokens: Vec<String>,
    text: String,
    order_key: usize,
}

fn tokenize_text(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || matches!(c, '/' | '_' | '-' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '"'))
        .map(normalize_word)
        .filter(|w| !w.is_empty())
        .collect()
}

fn split_script_sentences(script: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for raw_line in script.lines() {
        let mut cur = String::new();
        for ch in raw_line.chars() {
            cur.push(ch);
            if matches!(ch, '.' | '!' | '?') {
                let trimmed = cur.trim().to_string();
                let toks = tokenize_text(&trimmed);
                if !toks.is_empty() {
                    out.push((trimmed, toks));
                }
                cur.clear();
                if out.len() >= MAX_SCRIPT_LINES {
                    return out;
                }
            }
        }
        let tail = cur.trim().to_string();
        let toks = tokenize_text(&tail);
        if !toks.is_empty() && out.len() < MAX_SCRIPT_LINES {
            out.push((tail, toks));
        }
    }
    out
}

fn collect_transcript_candidates(item: ItemId, dur: Tick, tr: &Transcript, order_counter: &mut usize, out: &mut Vec<ScriptCandidate>) {
    if tr.words.is_empty() {
        return;
    }
    let mut start_idx = 0usize;
    while start_idx < tr.words.len() {
        let mut end_idx = start_idx;
        while end_idx + 1 < tr.words.len() && (end_idx - start_idx) < 13 {
            let w_cur = &tr.words[end_idx];
            let w_next = &tr.words[end_idx + 1];
            let ends_sentence = w_cur.text.ends_with(['.', '!', '?']);
            let speaker_changed = w_cur.speaker != w_next.speaker;
            let long_pause = (w_next.start - w_cur.end).seconds() > 0.85;
            if ends_sentence || speaker_changed || long_pause {
                break;
            }
            end_idx += 1;
        }
        let slice = &tr.words[start_idx..=end_idx];
        let toks: Vec<String> = slice.iter().map(|w| w.normalized()).filter(|w| !w.is_empty()).collect();
        if !toks.is_empty() {
            let s0 = slice.first().map_or(Tick::ZERO, |w| w.start).clamp(Tick::ZERO, dur);
            let s1 = slice.last().map_or(s0, |w| w.end).clamp(s0, dur);
            let span = (s1 - s0).max(Tick(TICKS_PER_SECOND / 4)).min((dur - s0).max(Tick(1)));
            let text = slice.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            *order_counter += 1;
            out.push(ScriptCandidate { item, range: TimeRange::new(s0, span), tokens: toks, text, order_key: *order_counter });
        }
        start_idx = end_idx + 1;
    }
}

fn collect_fallback_candidates(it: &ProjectItem, dur: Tick, order_counter: &mut usize, out: &mut Vec<ScriptCandidate>) {
    if let ItemKind::Media(mc) = &it.kind {
        for mk in &mc.markers {
            let label = format!("{} {}", mk.name, mk.comment);
            let toks = tokenize_text(&label);
            if !toks.is_empty() {
                let s0 = mk.start.clamp(Tick::ZERO, dur);
                let default_len = Tick::from_seconds_f64(3.0);
                let span = mk.duration.max(default_len).min((dur - s0).max(Tick(1)));
                *order_counter += 1;
                out.push(ScriptCandidate {
                    item: it.id,
                    range: TimeRange::new(s0, span),
                    tokens: toks,
                    text: label.trim().to_string(),
                    order_key: *order_counter,
                });
            }
        }
    }
    let mut meta_words = it.name.clone();
    for val in it.metadata.values() {
        meta_words.push(' ');
        meta_words.push_str(val);
    }
    let toks = tokenize_text(&meta_words);
    if !toks.is_empty() {
        let range = match &it.kind {
            ItemKind::Subclip { range, .. } => *range,
            ItemKind::Media(mc) => {
                let s0 = mc.mark_in.unwrap_or(Tick::ZERO).clamp(Tick::ZERO, dur);
                let s1 = mc.mark_out.unwrap_or_else(|| (s0 + Tick::from_seconds_f64(4.0)).min(dur)).clamp(s0, dur);
                TimeRange::new(s0, (s1 - s0).max(Tick(1)))
            }
            _ => TimeRange::new(Tick::ZERO, dur.min(Tick::from_seconds_f64(4.0))),
        };
        *order_counter += 1;
        out.push(ScriptCandidate { item: it.id, range, tokens: toks, text: it.name.clone(), order_key: *order_counter });
    }
}

fn build_script_candidates(s: &Session) -> Vec<ScriptCandidate> {
    let mut out = Vec::new();
    let mut order_counter = 0usize;
    for (&id, it) in &s.project.items {
        if !matches!(it.kind, ItemKind::Media(_) | ItemKind::Subclip { .. }) {
            continue;
        }
        let dur = it.duration();
        if dur <= Tick::ZERO {
            continue;
        }
        let root_id = s.project.resolve_media(id).map_or(id, |(r, _, _)| r);
        if let Some(tr) = s.project.transcripts.get(&root_id).filter(|t| !t.words.is_empty())
            && matches!(it.kind, ItemKind::Media(_))
        {
            collect_transcript_candidates(id, dur, tr, &mut order_counter, &mut out);
            continue;
        }
        collect_fallback_candidates(it, dur, &mut order_counter, &mut out);
    }
    out
}

fn similarity_score(query: &[String], cand: &[String]) -> f64 {
    if query.is_empty() || cand.is_empty() {
        return 0.0;
    }
    let hits = query.iter().filter(|q| cand.iter().any(|c| c == *q)).count();
    if hits == 0 {
        return 0.0;
    }
    let recall = hits as f64 / query.len() as f64;
    let precision = hits as f64 / cand.len() as f64;
    let f1 = 2.0 * recall * precision / (recall + precision).max(1e-9);

    let mut bigrams = 0usize;
    if query.len() >= 2 && cand.len() >= 2 {
        for q2 in query.windows(2) {
            if cand.windows(2).any(|c2| c2[0] == q2[0] && c2[1] == q2[1]) {
                bigrams += 1;
            }
        }
    }
    let bigram_bonus = if query.len() >= 2 { 0.25 * (bigrams as f64 / (query.len() - 1) as f64) } else { 0.0 };
    0.55 * recall + 0.45 * f1 + bigram_bonus
}

/// Align script lines to candidate segments via order-preserving DP (rewarding forward chronological
/// progression while allowing out-of-order takes when lexical similarity is decisively stronger).
fn align_script_dp(lines: &[(String, Vec<String>)], cands: &[ScriptCandidate]) -> Vec<Option<(usize, f64)>> {
    let n = lines.len();
    let m = cands.len();
    if n == 0 || m == 0 {
        return vec![None; n];
    }

    let mut scores = vec![vec![0.0_f64; m]; n];
    for (i, (_, qtoks)) in lines.iter().enumerate() {
        for (j, c) in cands.iter().enumerate() {
            scores[i][j] = similarity_score(qtoks, &c.tokens);
        }
    }

    // State `m` represents skipping/unmatched line `i`.
    let states = m + 1;
    let mut dp = vec![vec![f64::NEG_INFINITY; states]; n];
    let mut prev = vec![vec![states; states]; n];

    for j in 0..m {
        if scores[0][j] >= 0.12 {
            dp[0][j] = scores[0][j];
        }
    }
    dp[0][m] = 0.0;

    for i in 1..n {
        // Option A: leave line `i` unmatched
        for p_state in 0..states {
            if dp[i - 1][p_state] > dp[i][m] {
                dp[i][m] = dp[i - 1][p_state];
                prev[i][m] = p_state;
            }
        }
        // Option B: match line `i` to candidate `j`
        for j in 0..m {
            let base = scores[i][j];
            if base < 0.12 {
                continue;
            }
            for p_state in 0..states {
                let prior = dp[i - 1][p_state];
                if !prior.is_finite() {
                    continue;
                }
                let transition = if p_state == m {
                    0.0
                } else if p_state == j {
                    -0.25 // discourage repeating identical take back-to-back unless only match
                } else if cands[j].order_key > cands[p_state].order_key {
                    0.08 // chronological order bonus
                } else {
                    -0.03
                };
                let cand_total = prior + base + transition;
                if cand_total > dp[i][j] {
                    dp[i][j] = cand_total;
                    prev[i][j] = p_state;
                }
            }
        }
    }

    let mut best_state = m;
    let mut best_val = f64::NEG_INFINITY;
    for st in 0..states {
        if dp[n - 1][st] > best_val {
            best_val = dp[n - 1][st];
            best_state = st;
        }
    }

    let mut out = vec![None; n];
    let mut cur = best_state;
    for i in (0..n).rev() {
        if cur < m {
            out[i] = Some((cur, scores[i][cur]));
        }
        cur = prev[i][cur];
        if cur > m {
            break;
        }
    }
    out
}

fn intelli_script(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.intelliScript";
    let script = str_p(p, "script").map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| bad(CMD, "need non-empty `script`"))?;
    let seq_name = str_p(p, "sequenceName").map(str::trim).filter(|n| !n.is_empty()).unwrap_or("IntelliScript Rough Cut").to_string();

    let lines = split_script_sentences(script);
    if lines.is_empty() {
        return Err(bad(CMD, "`script` contains no matchable words"));
    }
    let cands = build_script_candidates(s);
    if cands.is_empty() {
        return Err(bad(CMD, "project contains no playable media items to match against script"));
    }

    let aligned = align_script_dp(&lines, &cands);
    let matched_pairs: Vec<(usize, &ScriptCandidate, f64)> =
        aligned.iter().enumerate().filter_map(|(line_i, opt)| opt.map(|(cand_i, sc)| (line_i, &cands[cand_i], sc))).collect();
    if matched_pairs.is_empty() {
        return Err(bad(CMD, "no transcript or clip metadata matched the supplied script"));
    }

    let settings = s.active_sequence().map_or_else(SequenceSettings::default, |q| q.settings.clone());
    let rate = settings.frame_rate;
    let mut new_seq_id = ItemId(0);
    let mut placed_rows: Vec<Value> = Vec::new();

    s.edit("IntelliScript Rough Cut", |pr, st| {
        new_seq_id = pr.new_sequence(&seq_name, settings.clone(), 1, 1, None);
        let mut cursor = Tick::ZERO;
        let mut v_items = Vec::new();
        let mut a_items = Vec::new();

        for &(line_idx, cand, score) in &matched_pairs {
            let has_v = pr.item(cand.item).is_some_and(ProjectItem::has_video);
            let has_a = pr.item(cand.item).is_some_and(ProjectItem::has_audio);
            let snapped_dur = rate.snap_nearest(cand.range.duration).max(rate.frame_duration());
            let src_range = TimeRange::new(cand.range.start, snapped_dur);
            let link_id = (has_v && has_a).then(|| pr.alloc_id());

            let mut primary_cid = 0u64;
            if has_v && let Some(mut vi) = pr.make_track_item(cand.item, TrackKind::Video, cursor, src_range, rate) {
                vi.link = link_id;
                primary_cid = vi.id.0;
                v_items.push(vi);
            }
            if has_a && let Some(mut ai) = pr.make_track_item(cand.item, TrackKind::Audio, cursor, src_range, rate) {
                ai.link = link_id;
                if primary_cid == 0 {
                    primary_cid = ai.id.0;
                }
                a_items.push(ai);
            }
            placed_rows.push(json!({
                "lineIndex": line_idx,
                "line": lines[line_idx].0,
                "item": cand.item.0,
                "clip": primary_cid,
                "matchedText": cand.text,
                "score": (score * 1000.0).round() / 1000.0,
                "timelineStart": cursor.0,
                "duration": snapped_dur.0,
            }));
            cursor = Tick(cursor.0.saturating_add(snapped_dur.0));
        }

        if let Some(seq) = pr.sequence_mut(new_seq_id) {
            if let Some(vt) = seq.video_tracks.first_mut() {
                vt.items = v_items;
            }
            if let Some(at) = seq.audio_tracks.first_mut() {
                at.items = a_items;
            }
            seq.check().map_err(EngineError::Other)?;
        }
        if !st.open_sequences.contains(&new_seq_id) {
            st.open_sequences.push(new_seq_id);
        }
        st.active_sequence = Some(new_seq_id);
        st.playheads.insert(new_seq_id, Tick::ZERO);
        Ok(())
    })?;

    s.events.push(Event::OpenSequence(new_seq_id));
    Ok(json!({
        "sequence": new_seq_id.0,
        "name": seq_name,
        "matchedLines": placed_rows.len(),
        "totalLines": lines.len(),
        "clips": placed_rows,
    }))
}

// ---------------------------------------------------------------------------
// 2. `multicam.smartSwitch` — Audio RMS Active-Speaker Multicam Cutter
// ---------------------------------------------------------------------------

fn rms_db_of_buffer(buf: &filmcraft_frame::AudioBuffer) -> f64 {
    let total_samples: usize = buf.channels.iter().map(Vec::len).sum();
    if total_samples == 0 {
        return -120.0;
    }
    let sum_sq: f64 = buf
        .channels
        .iter()
        .flat_map(|ch| ch.iter())
        .map(|&v| {
            let x = f64::from(v);
            if x.is_finite() { x * x } else { 0.0 }
        })
        .sum();
    let mean_sq = sum_sq / total_samples as f64;
    if mean_sq <= 1e-12 { -120.0 } else { 10.0 * mean_sq.log10() }
}

fn resolve_multicam_target(s: &Session, p: &Value, cmd: &str) -> Result<ClipId> {
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    if let Some(cid) = clip_p(p, "clip") {
        let (_, it) = seq.find_item(cid).ok_or_else(|| bad(cmd, format!("no clip {}", cid.0)))?;
        if !edit::multicam::is_multicam(it) {
            return Err(bad(cmd, "clip is not a multi-camera clip"));
        }
        return Ok(cid);
    }
    if !s.state.selection.is_empty() {
        return s
            .state
            .selection
            .iter()
            .copied()
            .find(|&c| seq.find_item(c).is_some_and(|(_, it)| edit::multicam::is_multicam(it)))
            .ok_or_else(|| bad(cmd, "selected clip is not a multi-camera clip"));
    }
    let ph = s.playhead();
    seq.video_tracks
        .iter()
        .rev()
        .filter_map(|tr| tr.item_at(ph))
        .find(|it| edit::multicam::is_multicam(it))
        .map(|it| it.id)
        .ok_or_else(|| bad(cmd, "select a multi-camera clip or park playhead over one"))
}

fn measure_angle_window_db(
    s: &Session,
    mc_seq: &filmcraft_project::Sequence,
    angle: usize,
    src_tick: Tick,
    win_frames: usize,
    sr: u32,
    provider: &SessionSourceProvider<'_>,
) -> f64 {
    let Some(cam) = mc_seq.multicam.as_ref().and_then(|m| m.cameras.get(angle)) else {
        return -120.0;
    };
    let s0 = src_tick.to_units_floor(i64::from(sr));
    let mut best_db = -120.0_f64;

    for &atid in &cam.audio_tracks {
        if let Some(atr) = mc_seq.audio_tracks.iter().find(|t| t.id == atid) {
            let buf = filmcraft_render::audio::track_input(&s.project, atr, s0, win_frames, sr, provider);
            best_db = best_db.max(rms_db_of_buffer(&buf));
        }
    }
    if best_db <= -100.0
        && let Some(vtid) = cam.video_track
        && let Some(vtr) = mc_seq.video_tracks.iter().find(|t| t.id == vtid)
        && let Some(vit) = vtr.item_at(src_tick)
        && let Some(src) = provider.source(vit.item)
        && src.info().has_audio()
    {
        let media_t = vit.source_time_at(src_tick).to_units_floor(i64::from(sr));
        if let Ok(buf) = src.audio_stream(vit.audio_stream, media_t, win_frames, sr) {
            best_db = best_db.max(rms_db_of_buffer(&buf));
        }
    }
    best_db
}

fn smart_switch(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "multicam.smartSwitch";
    let target_cid = resolve_multicam_target(s, p, CMD)?;
    let min_cut_s = f64_p(p, "minCutSeconds").unwrap_or(2.0).clamp(0.2, 60.0);
    let thresh_db = f64_p(p, "thresholdDb").unwrap_or(-42.0).clamp(-90.0, 0.0);

    let (mc_item_id, clip_start, clip_end, initial_angle, target_tracks, rate) = {
        let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
        let (tid, it) = seq.find_item(target_cid).ok_or_else(|| bad(CMD, "clip not found"))?;
        let mut tracks = vec![tid];
        if let Some(link) = it.link {
            for tr in seq.all_tracks() {
                if tr.items.iter().any(|x| x.link == Some(link) && edit::multicam::is_multicam(x)) && !tracks.contains(&tr.id) {
                    tracks.push(tr.id);
                }
            }
        }
        let init_ang = it.multicam.as_ref().map_or(0, |m| m.angle as usize);
        (it.item, it.start, it.end(), init_ang, tracks, seq.settings.frame_rate)
    };

    let mc_seq = s.project.sequence(mc_item_id).cloned().ok_or_else(|| bad(CMD, "multi-camera source sequence missing"))?;
    let shown = mc_seq.multicam.as_ref().map(|m| m.shown_angles()).unwrap_or_default();
    if shown.is_empty() {
        return Err(bad(CMD, "multi-camera source sequence has no enabled video angles"));
    }

    let step_s = 0.25_f64;
    let step_ticks = rate.snap_nearest(Tick::from_seconds_f64(step_s)).max(rate.frame_duration());
    let hold_ticks = rate.snap_nearest(Tick::from_seconds_f64(min_cut_s)).max(rate.frame_duration());
    let sr = mc_seq.settings.sample_rate.clamp(8_000, 192_000);
    let win_frames = ((f64::from(sr) * step_s).round() as usize).clamp(256, 96_000);

    // Measure per-window, per-angle RMS energy
    let provider = SessionSourceProvider::from_session(s);
    let clip_snapshot = s.active_sequence().and_then(|q| q.find_item(target_cid)).map(|(_, i)| i.clone());
    let mut windows: Vec<(Tick, Vec<(usize, f64)>)> = Vec::new();
    let mut any_decoded_audio = false;

    let mut t_cur = clip_start;
    while t_cur < clip_end && windows.len() < 4096 {
        let src_t = clip_snapshot.as_ref().map_or(t_cur - clip_start, |it| it.source_time_at(t_cur));
        let mut energies = Vec::with_capacity(shown.len());
        for &ang in &shown {
            let db = measure_angle_window_db(s, &mc_seq, ang, src_t, win_frames, sr, &provider);
            if db > -80.0 {
                any_decoded_audio = true;
            }
            energies.push((ang, db));
        }
        windows.push((t_cur, energies));
        t_cur = Tick(t_cur.0.saturating_add(step_ticks.0));
    }

    // Build switching schedule respecting hysteresis and minCutSeconds
    let mut cuts: Vec<Cut> = Vec::new();
    let mut current_ang = if shown.contains(&initial_angle) { initial_angle } else { shown[0] };
    let mut last_switch_time = clip_start;
    cuts.push(Cut { time: clip_start, angle: current_ang as u32 });

    if any_decoded_audio {
        for (win_t, energies) in &windows {
            if *win_t - last_switch_time < hold_ticks {
                continue;
            }
            let cur_db = energies.iter().find(|(a, _)| *a == current_ang).map_or(-120.0, |(_, d)| *d);
            if let Some(&(best_ang, best_db)) = energies.iter().max_by(|a, b| a.1.total_cmp(&b.1))
                && best_ang != current_ang
                && best_db >= thresh_db
                && best_db >= cur_db + 1.5
            {
                current_ang = best_ang;
                last_switch_time = *win_t;
                cuts.push(Cut { time: *win_t, angle: current_ang as u32 });
            }
        }
    } else if shown.len() >= 2 {
        // Deterministic camera-metadata / interval alternation fallback when angles have no decoded audio
        let mut idx = shown.iter().position(|&a| a == current_ang).unwrap_or(0);
        let mut next_t = Tick(clip_start.0.saturating_add(hold_ticks.0));
        while next_t < clip_end && cuts.len() < 512 {
            idx = (idx + 1) % shown.len();
            let ang = shown[idx];
            cuts.push(Cut { time: next_t, angle: ang as u32 });
            next_t = Tick(next_t.0.saturating_add(hold_ticks.0));
        }
    }

    let edit_points = s.edit_sequence("Smart Multicam Switch", |q, ctx, _| {
        let pts = edit::multicam::record(q, &target_tracks, mc_item_id, &cuts, clip_end, ctx);
        Ok(pts)
    })?;

    let cuts_json: Vec<Value> = cuts.iter().map(|c| json!({ "time": c.time.0, "seconds": (c.time - clip_start).seconds(), "angle": c.angle })).collect();
    Ok(json!({
        "clip": target_cid.0,
        "cuts": cuts_json,
        "editPoints": edit_points,
        "usedDecodedAudio": any_decoded_audio,
    }))
}

// ---------------------------------------------------------------------------
// 3. `audio.matchDialogue` — 3-Band Biquad Spectral + LUFS Dialogue Matcher
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn lowpass(sr: f64, f0: f64, q: f64) -> Self {
        let w0 = 2.0 * PI * (f0 / sr).clamp(0.001, 0.48);
        let (sin_w, cos_w) = w0.sin_cos();
        let alpha = sin_w / (2.0 * q.max(0.1));
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos_w) * 0.5) / a0,
            b1: (1.0 - cos_w) / a0,
            b2: ((1.0 - cos_w) * 0.5) / a0,
            a1: (-2.0 * cos_w) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn highpass(sr: f64, f0: f64, q: f64) -> Self {
        let w0 = 2.0 * PI * (f0 / sr).clamp(0.001, 0.48);
        let (sin_w, cos_w) = w0.sin_cos();
        let alpha = sin_w / (2.0 * q.max(0.1));
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos_w) * 0.5) / a0,
            b1: (-(1.0 + cos_w)) / a0,
            b2: ((1.0 + cos_w) * 0.5) / a0,
            a1: (-2.0 * cos_w) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

#[derive(Clone, Copy, Debug)]
struct DialogueProfile {
    lufs: f64,
    low_db: f64,
    mid_db: f64,
    presence_db: f64,
}

fn analyze_mono_samples(mono: &[f32], sr: u32, clip_gain_db: f64) -> Option<DialogueProfile> {
    if mono.is_empty() {
        return None;
    }
    let lin_gain = 10.0_f64.powf(clip_gain_db / 20.0);
    let fs = f64::from(sr.max(8_000));
    let mut lp_250 = Biquad::lowpass(fs, 250.0, FRAC_1_SQRT_2);
    let mut hp_250 = Biquad::highpass(fs, 250.0, FRAC_1_SQRT_2);
    let mut lp_2500 = Biquad::lowpass(fs, 2500.0, FRAC_1_SQRT_2);
    let mut hp_2500 = Biquad::highpass(fs, 2500.0, FRAC_1_SQRT_2);
    let mut lp_8000 = Biquad::lowpass(fs, 8000.0_f64.min(fs * 0.45), FRAC_1_SQRT_2);

    let mut sum_full = 0.0_f64;
    let mut sum_low = 0.0_f64;
    let mut sum_mid = 0.0_f64;
    let mut sum_pres = 0.0_f64;

    for &raw in mono {
        let x = if raw.is_finite() { f64::from(raw) * lin_gain } else { 0.0 };
        sum_full += x * x;
        let l = lp_250.tick(x);
        let m = lp_2500.tick(hp_250.tick(x));
        let p = lp_8000.tick(hp_2500.tick(x));
        sum_low += l * l;
        sum_mid += m * m;
        sum_pres += p * p;
    }

    let n = mono.len() as f64;
    let ms_full = sum_full / n;
    if ms_full <= 1e-11 {
        return None;
    }
    let to_db = |ms: f64| 10.0 * ms.max(1e-12).log10();
    Some(DialogueProfile {
        lufs: (-0.691 + to_db(ms_full)).clamp(-70.0, 6.0),
        low_db: to_db(sum_low / n),
        mid_db: to_db(sum_mid / n),
        presence_db: to_db(sum_pres / n),
    })
}

fn measure_clip_dialogue_profile(item: &filmcraft_project::TrackItem, sr: u32, provider: &SessionSourceProvider<'_>) -> DialogueProfile {
    let max_frames = ((f64::from(sr) * MAX_ANALYZE_SECONDS).round() as usize).clamp(1024, 480_000);
    let clip_frames = (item.duration.to_units_floor(i64::from(sr)).max(1) as usize).min(max_frames);
    let s0 = item.source_in.to_units_floor(i64::from(sr)).max(0);

    if let Some(src) = provider.source(item.item)
        && src.info().has_audio()
        && let Ok(buf) = src.audio_stream(item.audio_stream, s0, clip_frames, sr)
        && !buf.channels.is_empty()
    {
        let len = buf.channels[0].len();
        let ch_n = buf.channels.len() as f32;
        let mut mono = vec![0.0_f32; len];
        for ch in &buf.channels {
            for (dst, &v) in mono.iter_mut().zip(ch.iter()) {
                *dst += v / ch_n;
            }
        }
        if let Some(prof) = analyze_mono_samples(&mono, sr, item.gain_db) {
            return prof;
        }
    }

    // Metadata / Essential Sound fallback when source audio is offline or synthetic silence
    let base_lufs = (-23.0 + item.gain_db).clamp(-60.0, 0.0);
    let (mut low_off, mut mid_off, mut high_off) = (0.0, 0.0, 0.0);
    if let Some(peq) = item.effect("parametric_eq") {
        low_off = peq.f64_at("low_gain", Tick::ZERO);
        mid_off = peq.f64_at("mid_gain", Tick::ZERO);
        high_off = peq.f64_at("high_gain", Tick::ZERO);
    }
    DialogueProfile { lufs: base_lufs, low_db: base_lufs - 5.0 + low_off, mid_db: base_lufs - 1.5 + mid_off, presence_db: base_lufs - 6.0 + high_off }
}

fn resolve_audio_clip_id(seq: &filmcraft_project::Sequence, raw: ClipId) -> Option<ClipId> {
    let (tid, it) = seq.find_item(raw)?;
    if seq.track(tid)?.kind == TrackKind::Audio {
        return Some(raw);
    }
    let link = it.link?;
    seq.audio_tracks.iter().flat_map(|t| t.items.iter()).find(|x| x.link == Some(link)).map(|x| x.id)
}

fn upsert_audio_fx(effects: &mut Vec<EffectInstance>, fx_id: &str, updates: &[(&str, ParamValue)]) {
    if let Some(existing) = effects.iter_mut().find(|e| e.effect == fx_id) {
        existing.enabled = true;
        for (k, v) in updates {
            existing.params.insert((*k).to_string(), Param::new(v.clone()));
        }
        return;
    }
    let Some(def) = find_effect(fx_id) else { return };
    let mut inst = def.instance();
    for (k, v) in updates {
        inst.params.insert((*k).to_string(), Param::new(v.clone()));
    }
    let insert_at = effects.iter().position(|ef| ef.def().is_some_and(|d| d.intrinsic)).unwrap_or(effects.len());
    effects.insert(insert_at, inst);
}

fn match_dialogue(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "audio.matchDialogue";
    let raw_ref = clip_p(p, "reference").ok_or_else(|| bad(CMD, "need `reference` clip id"))?;
    let (ref_cid, target_cids, sr) = {
        let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
        let r_cid = resolve_audio_clip_id(seq, raw_ref).ok_or_else(|| bad(CMD, "reference clip has no audio track item"))?;
        let raw_targets: Vec<ClipId> = if let Some(arr) = p.get("targets").and_then(Value::as_array) {
            let mut v = Vec::with_capacity(arr.len());
            for val in arr {
                let id = val.as_u64().ok_or_else(|| bad(CMD, "`targets` entries must be clip ids"))?;
                v.push(ClipId(id));
            }
            v
        } else {
            s.state.selection.clone()
        };
        let mut resolved = Vec::new();
        for tc in raw_targets {
            if let Some(ac) = resolve_audio_clip_id(seq, tc)
                && ac != r_cid
                && !resolved.contains(&ac)
            {
                resolved.push(ac);
            }
        }
        if resolved.is_empty() {
            return Err(bad(CMD, "supply `targets` or select at least one target audio clip distinct from `reference`"));
        }
        (r_cid, resolved, seq.settings.sample_rate.clamp(8_000, 192_000))
    };

    let provider = SessionSourceProvider::from_session(s);
    let (ref_prof, plans) = {
        let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
        let (_, ref_it) = seq.find_item(ref_cid).ok_or_else(|| bad(CMD, "reference clip missing"))?;
        let rp = measure_clip_dialogue_profile(ref_it, sr, &provider);
        let mut computed = Vec::with_capacity(target_cids.len());
        for &tcid in &target_cids {
            let (_, tit) = seq.find_item(tcid).ok_or_else(|| bad(CMD, format!("target clip {} missing", tcid.0)))?;
            let tp = measure_clip_dialogue_profile(tit, sr, &provider);
            let gain_delta = (rp.lufs - tp.lufs).clamp(-18.0, 18.0);
            let low_eq = (((rp.low_db - rp.lufs) - (tp.low_db - tp.lufs)) * 0.8).clamp(-12.0, 12.0);
            let mid_eq = (((rp.mid_db - rp.lufs) - (tp.mid_db - tp.lufs)) * 0.8).clamp(-12.0, 12.0);
            let high_eq = (((rp.presence_db - rp.lufs) - (tp.presence_db - tp.lufs)) * 0.8).clamp(-12.0, 12.0);
            let target_lufs = rp.lufs.clamp(-48.0, -6.0);
            computed.push((tcid, gain_delta, low_eq, mid_eq, high_eq, target_lufs));
        }
        (rp, computed)
    };

    let mut applied = Vec::with_capacity(plans.len());
    s.edit_sequence("Match Dialogue", |q, _, _| {
        for &(tcid, gain_delta, low_eq, mid_eq, high_eq, target_lufs) in &plans {
            let Some((_, it)) = q.find_item_mut(tcid) else { continue };
            it.gain_db = (it.gain_db + gain_delta).clamp(-96.0, 24.0);
            upsert_audio_fx(
                &mut it.effects,
                "parametric_eq",
                &[
                    ("low_on", ParamValue::Bool(true)),
                    ("low_freq", ParamValue::Float(220.0)),
                    ("low_gain", ParamValue::Float(low_eq)),
                    ("mid_on", ParamValue::Bool(true)),
                    ("mid_freq", ParamValue::Float(1000.0)),
                    ("mid_gain", ParamValue::Float(mid_eq)),
                    ("high_on", ParamValue::Bool(true)),
                    ("high_freq", ParamValue::Float(4500.0)),
                    ("high_gain", ParamValue::Float(high_eq)),
                ],
            );
            upsert_audio_fx(&mut it.effects, "dialogue_leveler", &[("target_lufs", ParamValue::Float(target_lufs))]);
            applied.push(json!({
                "clip": tcid.0,
                "gainDb": it.gain_db,
                "lowEqDb": low_eq,
                "midEqDb": mid_eq,
                "presenceEqDb": high_eq,
                "targetLufs": target_lufs,
            }));
        }
        Ok(())
    })?;

    Ok(json!({
        "reference": ref_cid.0,
        "referenceLufs": ref_prof.lufs,
        "matchedTargets": applied,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use filmcraft_project::{Camera, Label, MediaClip, MediaRef, MulticamAudio, MulticamSel, MulticamSource, Speaker, Word};
    use filmcraft_time::FrameRate;

    fn write_sine_wav(path: &Path, sr: u32, seconds: f64, freq_hz: f64, amp: f32) {
        let frames = (f64::from(sr) * seconds).round() as usize;
        let data_bytes = (frames * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_bytes as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1_u16.to_le_bytes()); // Mono
        out.extend_from_slice(&sr.to_le_bytes());
        out.extend_from_slice(&(sr * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_bytes.to_le_bytes());
        for i in 0..frames {
            let t = i as f64 / f64::from(sr);
            let sample = ((2.0 * PI * freq_hz * t).sin() * f64::from(amp) * 32767.0).clamp(-32767.0, 32767.0) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, out).expect("write wav");
    }

    use std::path::Path;

    #[test]
    fn intelliscript_builds_rough_cut_in_script_order_from_transcripts() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");

        let media_ids: Vec<ItemId> = s.project.items.iter().filter_map(|(&id, it)| matches!(it.kind, ItemKind::Media(_)).then_some(id)).take(2).collect();
        assert!(media_ids.len() >= 2);

        let tr_a = Transcript {
            language: "en".into(),
            source: "manual".into(),
            speakers: vec![Speaker { name: "Host".into() }],
            words: vec![
                Word::new("Welcome", Tick::from_seconds_f64(0.0), Tick::from_seconds_f64(0.5)),
                Word::new("to", Tick::from_seconds_f64(0.5), Tick::from_seconds_f64(0.8)),
                Word::new("FilmCraft.", Tick::from_seconds_f64(0.8), Tick::from_seconds_f64(1.6)),
                Word::new("Color", Tick::from_seconds_f64(2.0), Tick::from_seconds_f64(2.5)),
                Word::new("grading", Tick::from_seconds_f64(2.5), Tick::from_seconds_f64(3.0)),
                Word::new("shines.", Tick::from_seconds_f64(3.0), Tick::from_seconds_f64(3.8)),
            ],
        };
        let tr_b = Transcript {
            language: "en".into(),
            source: "manual".into(),
            speakers: vec![Speaker { name: "Guest".into() }],
            words: vec![
                Word::new("Audio", Tick::from_seconds_f64(0.2), Tick::from_seconds_f64(0.7)),
                Word::new("matching", Tick::from_seconds_f64(0.7), Tick::from_seconds_f64(1.3)),
                Word::new("saves", Tick::from_seconds_f64(1.3), Tick::from_seconds_f64(1.8)),
                Word::new("hours.", Tick::from_seconds_f64(1.8), Tick::from_seconds_f64(2.4)),
            ],
        };
        s.edit("Attach Transcripts", |pr, _| {
            pr.transcripts.insert(media_ids[0], Arc::new(tr_a.clone()));
            pr.transcripts.insert(media_ids[1], Arc::new(tr_b.clone()));
            Ok(())
        })
        .unwrap();

        let script = "Welcome to FilmCraft.\nAudio matching saves hours.\nColor grading shines.";
        let res = s.execute("ai.intelliScript", json!({ "script": script, "sequenceName": "AI Script Assembly" })).expect("intelliScript");
        assert_eq!(res["matchedLines"], json!(3));
        let clips = res["clips"].as_array().unwrap();
        assert_eq!(clips[0]["item"], json!(media_ids[0].0));
        assert_eq!(clips[1]["item"], json!(media_ids[1].0));
        assert_eq!(clips[2]["item"], json!(media_ids[0].0));
    }

    #[test]
    fn multicam_smart_switch_and_dialogue_match_operate_on_real_audio_and_undo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cam1_wav = dir.path().join("cam1_bass_loud.wav");
        let cam2_wav = dir.path().join("cam2_presence_quiet.wav");
        // Cam 1: 180 Hz loud low-frequency voice; Cam 2: 3500 Hz quieter presence signal
        write_sine_wav(&cam1_wav, 48_000, 6.0, 180.0, 0.65);
        write_sine_wav(&cam2_wav, 48_000, 6.0, 3500.0, 0.12);

        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo");

        let info1 = s.media.open_file(&cam1_wav.to_string_lossy(), &*s.services).expect("probe cam1").info().clone();
        let info2 = s.media.open_file(&cam2_wav.to_string_lossy(), &*s.services).expect("probe cam2").info().clone();

        let mut ref_audio_clip = ClipId(0);
        let mut tgt_audio_clip = ClipId(0);
        let mut mc_timeline_clip = ClipId(0);

        s.edit("Setup Audio & Multicam Test", |pr, st| {
            let mk_audio_item = |pr: &mut filmcraft_project::Project, name: &str, path: &Path, info: filmcraft_media::MediaInfo| -> ItemId {
                let mc = MediaClip {
                    media: MediaRef::File { path: path.to_string_lossy().to_string() },
                    info,
                    interpret: Default::default(),
                    mark_in: None,
                    mark_out: None,
                    markers: Vec::new(),
                    offline: false,
                    proxy: None,
                    identity: None,
                };
                pr.add_item(name, Label::Caribbean, ItemKind::Media(Box::new(mc)), None)
            };

            let item1 = mk_audio_item(pr, "Cam1 Mic", &cam1_wav, info1.clone());
            let item2 = mk_audio_item(pr, "Cam2 Mic", &cam2_wav, info2.clone());
            let vid_src = pr.items.iter().find_map(|(&id, it)| (matches!(it.kind, ItemKind::Media(_)) && it.has_video()).then_some(id)).unwrap();

            // Build a 2-angle Multicam Source Sequence where Angle 0 speaks in 0..3s and Angle 1 speaks in 3..6s
            let mc_seq_id = pr.new_sequence("MC Source", SequenceSettings::default(), 2, 2, None);
            let rate = FrameRate::FPS_24;
            let v0 = pr.make_track_item(vid_src, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(6.0)), rate).unwrap();
            let v1 = pr.make_track_item(vid_src, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(6.0)), rate).unwrap();
            let a0 = pr.make_track_item(item1, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(3.0)), rate).unwrap();
            let mut a1 = pr
                .make_track_item(item2, TrackKind::Audio, Tick::from_seconds_f64(3.0), TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(3.0)), rate)
                .unwrap();
            a1.gain_db = 12.0;

            let seq_mc = pr.sequence_mut(mc_seq_id).unwrap();
            let vt0 = seq_mc.video_tracks[0].id;
            let vt1 = seq_mc.video_tracks[1].id;
            let at0 = seq_mc.audio_tracks[0].id;
            let at1 = seq_mc.audio_tracks[1].id;
            seq_mc.video_tracks[0].items.push(v0);
            seq_mc.video_tracks[1].items.push(v1);
            seq_mc.audio_tracks[0].items.push(a0);
            seq_mc.audio_tracks[1].items.push(a1);
            seq_mc.multicam = Some(MulticamSource {
                cameras: vec![
                    Camera { name: "Angle A".into(), video_track: Some(vt0), audio_tracks: vec![at0], enabled: true, source: Some(vid_src) },
                    Camera { name: "Angle B".into(), video_track: Some(vt1), audio_tracks: vec![at1], enabled: true, source: Some(vid_src) },
                ],
                audio: MulticamAudio::SwitchAudio,
                sync: "audio".into(),
            });

            // Main sequence with the Multicam clip on V1 and the two raw dialogue clips on A1
            let main_seq_id = pr.new_sequence("Main Edit", SequenceSettings::default(), 1, 1, None);
            let mut mc_item =
                pr.make_track_item(mc_seq_id, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(6.0)), rate).unwrap();
            mc_item.multicam = Some(MulticamSel { enabled: true, angle: 0 });
            mc_timeline_clip = mc_item.id;

            let ref_it = pr.make_track_item(item1, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(3.0)), rate).unwrap();
            let tgt_it = pr
                .make_track_item(item2, TrackKind::Audio, Tick::from_seconds_f64(3.5), TimeRange::new(Tick::ZERO, Tick::from_seconds_f64(2.5)), rate)
                .unwrap();
            ref_audio_clip = ref_it.id;
            tgt_audio_clip = tgt_it.id;

            let main_seq = pr.sequence_mut(main_seq_id).unwrap();
            main_seq.video_tracks[0].items.push(mc_item);
            main_seq.audio_tracks[0].items.push(ref_it);
            main_seq.audio_tracks[0].items.push(tgt_it);
            st.active_sequence = Some(main_seq_id);
            st.open_sequences.push(main_seq_id);
            Ok(())
        })
        .unwrap();

        // 1. Test `multicam.smartSwitch` using real decoded WAV audio
        let sw = s.execute("multicam.smartSwitch", json!({ "clip": mc_timeline_clip.0, "minCutSeconds": 1.5, "thresholdDb": -45.0 })).expect("smartSwitch");
        assert_eq!(sw["usedDecodedAudio"], json!(true));
        assert!(sw["editPoints"].as_u64().unwrap() >= 1, "expected switch from Angle 0 to Angle 1: {sw}");

        // Single-step undo restores unsplit multicam clip
        s.execute("edit.undo", json!({})).expect("undo smartSwitch");
        assert_eq!(s.active_sequence().unwrap().video_tracks[0].items.len(), 1);

        // 2. Test `audio.matchDialogue` matching quiet 3.5kHz clip to loud 180Hz reference clip
        let md = s.execute("audio.matchDialogue", json!({ "reference": ref_audio_clip.0, "targets": [tgt_audio_clip.0] })).expect("matchDialogue");
        let matched = &md["matchedTargets"][0];
        assert!(matched["gainDb"].as_f64().unwrap() > 8.0, "quieter target boosted toward reference: {matched}");
        assert!(matched["lowEqDb"].as_f64().unwrap() > 1.0, "low shelf boosted to match 180Hz reference: {matched}");
        assert!(matched["presenceEqDb"].as_f64().unwrap() < -1.0, "presence shelf attenuated vs 3.5kHz target: {matched}");

        let (_, updated_tgt) = s.active_sequence().unwrap().find_item(tgt_audio_clip).unwrap();
        assert!(updated_tgt.effect("parametric_eq").is_some());
        assert!(updated_tgt.effect("dialogue_leveler").is_some());
    }
}
