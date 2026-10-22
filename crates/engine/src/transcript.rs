//! Text-based editing commands (`transcript.*`): the Text panel ▸ Transcript tab.
//!
//! Transcripts belong to media items (`Project::transcripts`, media time); the sequence transcript
//! is derived from them ([`filmcraft_edit::transcript::sequence_words`]). Words of the sequence
//! transcript are addressed by index (`from`, `to`, inclusive), as `transcript.inspect` lists them.
//!
//! Speech recognition goes through a [`Transcriber`]: [`Session::transcriber`] when a host or a
//! test installed one, else the Whisper model named by `model` from `<data dir>/models` (needs the
//! engine feature `whisper`; without it `transcript.generate` fails with a clear error, and agents
//! can still bring their own transcript with `transcript.set`).

use std::sync::Arc;

use serde_json::{Value, json};

use filmcraft_edit as edit;
use filmcraft_edit::transcript::{self as tx, CaptionRules, SeqWord};
use filmcraft_project::{CaptionFormat, CaptionTrack, ItemId, TrackId, Transcript};
use filmcraft_speech::{Options, SpeechError};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::commands::{CommandSpec, always, bad, bool_p, f64_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal }
}

/// Where downloaded speech models live (`<data dir>/models`).
pub fn models_dir() -> Option<std::path::PathBuf> {
    crate::autosave::default_data_dir().map(|d| d.join("models"))
}

/// Whether this build can transcribe with Whisper (feature `whisper`).
pub fn speech_available() -> bool {
    filmcraft_speech::available()
}

/// The words of the active sequence's transcript.
pub fn sequence_words(s: &Session) -> Vec<SeqWord> {
    match s.active_sequence() {
        Some(q) => tx::sequence_words(q, &s.project.transcripts),
        None => Vec::new(),
    }
}

fn has_transcript(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    if s.active_sequence().is_some_and(|q| tx::has_sequence_words(q, &s.project.transcripts)) {
        Ok(())
    } else {
        Err("the sequence has no transcript (Transcribe first)".into())
    }
}

fn has_transcripts(s: &Session) -> std::result::Result<(), String> {
    if s.project.transcripts.is_empty() { Err("there are no transcripts".into()) } else { Ok(()) }
}

/// Why speech-to-text can't run in this build (no installed transcriber, built without `whisper`).
pub(crate) const NO_SPEECH: &str =
    "speech-to-text is not available in this build (built without the `whisper` feature); import a transcript with transcript.set instead";

/// `transcript.generate` can run: a host installed a transcriber or the build has speech-to-text
/// (#97: it reported enabled and then always failed).
pub(crate) fn can_transcribe(s: &Session) -> std::result::Result<(), String> {
    if s.transcriber.is_some() || speech_available() { Ok(()) } else { Err(NO_SPEECH.into()) }
}

/// `transcript.downloadModel` can run: built with `speech-download` (#98).
fn can_download(_: &Session) -> std::result::Result<(), String> {
    if cfg!(feature = "speech-download") {
        Ok(())
    } else {
        Err("model downloads are not available in this build (built without the `speech-download` feature)".into())
    }
}

/// The media item behind a project item (subclips resolve to their parent).
fn media_item(s: &Session, item: ItemId) -> Option<ItemId> {
    s.project.resolve_media(item).map(|(root, _, _)| root)
}

fn ids_p(p: &Value, k: &str) -> Option<Vec<ItemId>> {
    p.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(ItemId).collect())
}

/// Items to transcribe: `items` / `item`, else the Project panel selection, else the media of the
/// active sequence's enabled audio clips. Subclips resolve to their media; duplicates are removed.
fn targets(s: &Session, p: &Value) -> Vec<ItemId> {
    let mut raw = ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])).unwrap_or_default();
    if raw.is_empty() {
        raw = s.state.project_selection.clone();
    }
    if raw.is_empty()
        && let Some(q) = s.active_sequence()
    {
        raw = q.audio_tracks.iter().flat_map(|t| t.items.iter()).filter(|it| it.enabled).map(|it| it.item).collect();
    }
    let mut out = Vec::new();
    for i in raw {
        if let Some(m) = media_item(s, i)
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    out
}

fn speech_err(e: SpeechError) -> EngineError {
    EngineError::Other(e.to_string())
}

fn generate(s: &mut Session, p: &Value) -> Result<Value> {
    let items = targets(s, p);
    if items.is_empty() {
        return Err(bad("transcript.generate", "nothing to transcribe (pass `items`, select clips, or open a sequence with audio)"));
    }
    // Settings ▸ Media Analysis & Transcription: language (or auto-detect) and speaker labelling
    let ma = &s.prefs.media_analysis;
    let default_language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()) };
    let opts = Options {
        preserve_fillers: bool_p(p, "preserveFillers").unwrap_or(false),
        language: match str_p(p, "language") {
            Some(l) => Some(l).filter(|l| !l.is_empty() && *l != "auto").map(str::to_string),
            None => default_language,
        },
        diarize: bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off"),
        max_speakers: u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(Options::default().max_speakers),
    };
    crate::transcript_jobs::start(s, p, items, opts)
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let item = u64_p(p, "item").map(ItemId).ok_or_else(|| bad("transcript.set", "`item` is required"))?;
    let item = media_item(s, item).ok_or_else(|| bad("transcript.set", "no such media item"))?;
    let v = p.get("transcript").cloned().ok_or_else(|| bad("transcript.set", "`transcript` is required"))?;
    let mut t: Transcript = serde_json::from_value(v).map_err(|e| bad("transcript.set", e.to_string()))?;
    if t.source.is_empty() {
        t.source = "imported".into();
    }
    t.normalize();
    t.check().map_err(|e| bad("transcript.set", e))?;
    let n = t.words.len();
    s.edit("Set Transcript", move |pr, _| {
        pr.transcripts.insert(item, Arc::new(t));
        Ok(())
    })?;
    Ok(json!({"item": item.0, "words": n}))
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let items: Vec<ItemId> = match ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])) {
        Some(v) => v.into_iter().filter_map(|i| media_item(s, i)).collect(),
        None => s.project.transcripts.keys().copied().collect(),
    };
    let n = items.iter().filter(|i| s.project.transcripts.contains_key(i)).count();
    if n == 0 {
        return Err(EngineError::Other("no transcript to delete".into()));
    }
    s.edit("Delete Transcript", move |pr, _| {
        for i in items {
            pr.transcripts.remove(&i);
        }
        Ok(())
    })?;
    Ok(json!({"deleted": n}))
}

fn word_json(i: usize, w: &SeqWord) -> Value {
    json!({"i": i, "text": w.text, "start": w.start.0, "end": w.end.0, "speaker": w.speaker, "clip": w.clip.0, "item": w.item.0, "confidence": w.confidence, "timingValid": w.end > w.start,"fillerChecked":w.filler_checked,"fillerBoundsRevision":w.filler_bounds_revision,"fillerBounds":w.filler_bounds.map(|r|json!({"start":r.start.0,"end":r.end().0}))})
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let gap = Tick::from_seconds_f64(f64_p(p, "paragraphGapSeconds").unwrap_or(1.5));
    let paras: Vec<Value> = tx::paragraphs(&words, gap)
        .into_iter()
        .map(|r| {
            let text = words[r.clone()].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            json!({"from": r.start, "to": r.end - 1, "speaker": words[r.start].speaker, "start": words[r.start].start.0, "end": words[r.end - 1].end.0, "text": text})
        })
        .collect();
    let speakers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for w in &words {
            if let Some(n) = &w.speaker
                && !v.contains(n)
            {
                v.push(n.clone());
            }
        }
        v
    };
    let current = tx::word_at(&words, s.playhead());
    Ok(json!({
        "words": words.iter().enumerate().map(|(i, w)| word_json(i, w)).collect::<Vec<_>>(),
        "paragraphs": paras,
        "speakers": speakers,
        "current": current,
        "items": s.project.transcripts.iter().map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "language": t.language, "source": t.source, "speakers": t.speakers.iter().map(|k| &k.name).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    }))
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let q = str_p(p, "query").ok_or_else(|| bad("transcript.search", "`query` is required"))?;
    let words = sequence_words(s);
    let hits: Vec<Value> = tx::search(&words, q)
        .into_iter()
        .map(|r| json!({"from": r.start, "to": r.end - 1, "start": words[r.start].start.0, "end": words[r.end - 1].end.0}))
        .collect();
    Ok(json!({"matches": hits}))
}

/// Timeline range of the words `from..=to` (frame-snapped outward).
fn range_p(s: &Session, p: &Value, cmd: &str) -> Result<TimeRange> {
    let words = sequence_words(s);
    let from = u64_p(p, "from").and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(cmd, "a valid `from` word index is required"))?;
    let to = match u64_p(p, "to") {
        Some(n) => usize::try_from(n).map_err(|_| bad(cmd, "word index out of range"))?,
        None => from,
    };
    if from >= words.len() || to >= words.len() {
        return Err(bad(cmd, format!("word index out of range (the transcript has {} words)", words.len())));
    }
    tx::word_range(&words, from, to, s.sequence_rate()).ok_or_else(|| bad(cmd, "selected word has no usable timing; correct the transcript before editing"))
}

fn range_json(r: TimeRange) -> Value {
    json!({"start": r.start.0, "end": r.end().0})
}

fn select(s: &mut Session, p: &Value) -> Result<Value> {
    let r = range_p(s, p, "transcript.select")?;
    let fd = s.sequence_rate().frame_duration();
    s.edit_sequence("Mark Transcript Selection", |q, _, _| {
        q.mark_in = Some(r.start);
        q.mark_out = Some(r.end() - fd);
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn extract_or_lift(s: &mut Session, p: &Value, extract: bool) -> Result<Value> {
    let cmd = if extract { "transcript.extract" } else { "transcript.lift" };
    let r = range_p(s, p, cmd)?;
    let tg = s.targeting().targeted;
    s.edit_sequence(if extract { "Extract Text" } else { "Lift Text" }, |q, ctx, _| {
        if extract {
            edit::extract(q, &tg, r, ctx);
        } else {
            edit::lift(q, &tg, r, ctx);
        }
        q.mark_in = None;
        q.mark_out = None;
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn rename_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad("transcript.renameSpeaker", "`name` is required"))?.to_string();
    let item = u64_p(p, "item").map(ItemId).and_then(|i| media_item(s, i));
    // `speaker`: the current name (every transcript), or an index (needs `item`)
    let (old_name, index) = match p.get("speaker") {
        Some(Value::String(n)) => (Some(n.clone()), None),
        Some(v) if v.is_u64() => (None, v.as_u64().map(|n| n as usize)),
        _ => return Err(bad("transcript.renameSpeaker", "`speaker` (name, or index with `item`) is required")),
    };
    if index.is_some() && item.is_none() {
        return Err(bad("transcript.renameSpeaker", "a speaker index needs `item`"));
    }
    let mut n = 0;
    let mut next = s.project.transcripts.clone();
    for (i, t) in next.iter_mut() {
        if item.is_some_and(|x| x != *i) {
            continue;
        }
        let tt = Arc::make_mut(t);
        for (k, sp) in tt.speakers.iter_mut().enumerate() {
            if old_name.as_ref().is_some_and(|o| *o == sp.name) || index == Some(k) {
                sp.name = name.clone();
                n += 1;
            }
        }
    }
    if n == 0 {
        return Err(EngineError::Other("no such speaker".into()));
    }
    s.edit("Rename Speaker", move |pr, _| {
        pr.transcripts = next;
        Ok(())
    })?;
    Ok(json!({"renamed": n}))
}

fn remove_ranges(s: &mut Session, label: &str, ranges: Vec<TimeRange>) -> Result<Value> {
    let n = ranges.len();
    if n == 0 {
        return Ok(json!({"removed": 0, "ticks": 0}));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let words = sequence_words(s);
    for range in &ranges {
        if let Some(reason) = cleanup_conflict(q, &s.project, &words, *range) {
            return Err(EngineError::Other(reason.into()));
        }
    }
    let total = s.edit_sequence(label, |q, ctx, _| Ok(tx::ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({"removed": n, "ticks": total.0, "seconds": total.0 as f64 / TICKS_PER_SECOND as f64}))
}

/// The displayed transcript suppresses lower dialogue tracks. A global ripple cut must not
/// treat that suppression as proof that those tracks are silent. Only aligned copies of the
/// same source are safe to cut together without separately reviewing their audio.
fn cleanup_conflict(q: &filmcraft_project::Sequence, project: &filmcraft_project::Project, words: &[SeqWord], range: TimeRange) -> Option<&'static str> {
    if q.all_tracks().any(|track| track.locked && track.items.iter().any(|clip| clip.end() > range.start))
        || q.caption_tracks.iter().any(|track| track.locked && track.captions.iter().any(|caption| caption.end() > range.start))
    {
        return Some("A locked track would lose sync; unlock it before applying cleanup");
    }
    let anchor = words.iter().rev().find(|word| word.start <= range.start);
    let origin = anchor.and_then(|word| q.audio_tracks.iter().flat_map(|track| &track.items).find(|clip| clip.id == word.clip));
    // Recognition currently downmixes the media's default stream, not a clip's custom
    // channel/stream selection. Those transcripts cannot authorize edits of another voice.
    if anchor.is_some_and(|word| word.filler_requires_check) && origin.is_some_and(|clip| clip.audio_stream != 0 || !clip.source_channels.is_empty()) {
        return Some("This transcript does not verify the selected audio stream/channels; review that audio separately");
    }
    for clip in q.audio_tracks.iter().flat_map(|track| &track.items).filter(|clip| clip.start < range.end() && clip.end() > range.start) {
        // Built-in procedural tones/demo music contain no speech. Imported music, a name
        // such as "music.wav", or absence of a transcript is never that proof.
        let procedural_without_speech = !project.transcripts.contains_key(&clip.item) && project.item(clip.item).is_some_and(|item| {
            use filmcraft_media::Generator;
            matches!(&item.kind, filmcraft_project::ItemKind::Media(media) if matches!(&media.media,
                filmcraft_project::MediaRef::Generator(Generator::BarsAndTone | Generator::ColorMatte { .. } | Generator::BlackVideo | Generator::TransparentVideo | Generator::CountingLeader | Generator::Demo(_) | Generator::Tone { .. })))
        });
        if procedural_without_speech {
            continue;
        }
        let aligned_copy = origin.is_some_and(|source| {
            clip.id == source.id
                || (clip.item == source.item
                    && clip.audio_stream == source.audio_stream
                    && clip.source_channels == source.source_channels
                    && clip.effects == source.effects
                    && clip.reverse == source.reverse
                    && clip.frame_hold == source.frame_hold
                    && clip.speed == source.speed
                    && ((clip.start == source.start && clip.source_in == source.source_in)
                        || (clip.speed == 1.0
                            && clip.source_in.0.checked_sub(clip.start.0).zip(source.source_in.0.checked_sub(source.start.0)).is_some_and(|(a, b)| a == b))))
        });
        if !aligned_copy {
            return Some("Another audio source overlaps this cut; review the tracks separately to preserve its speech");
        }
    }
    None
}

/// Read-only candidates. Apply recomputes them rather than trusting caller-supplied cut times.
fn cleanup_preview(s: &mut Session, p: &Value) -> Result<Value> {
    use std::hash::{Hash, Hasher};
    let cmd = "transcript.previewCleanup";
    let seconds = |name: &str, default: f64, max: f64| -> Result<f64> {
        let value = match p.get(name) {
            None => default,
            Some(v) => v.as_f64().ok_or_else(|| bad(cmd, format!("{name} must be a number")))?,
        };
        if !value.is_finite() || value < 0.0 || value > max {
            return Err(bad(cmd, format!("{name} must be between 0 and {max}")));
        }
        Ok(value)
    };
    let min = seconds("minSeconds", 1.0, 3600.0)?;
    let keep = seconds("keepSeconds", 0.15, 60.0)?;
    let fillers: Vec<String> = match p.get("fillers") {
        Some(v) => {
            let a = v.as_array().filter(|a| a.len() <= 256).ok_or_else(|| bad(cmd, "fillers must be an array of at most 256 phrases"))?;
            a.iter()
                .map(|v| {
                    v.as_str()
                        .filter(|v| !v.trim().is_empty() && v.len() <= 256)
                        .map(str::to_owned)
                        .ok_or_else(|| bad(cmd, "each filler must be a nonempty string of at most 256 bytes"))
                })
                .collect::<Result<_>>()?
        }
        None => ["음", "어", "으음", "어어"].iter().map(|x| x.to_string()).collect(),
    };
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let snapshot = serde_json::to_vec(&(q, &s.project.transcripts, min, keep, &fillers)).map_err(|e| EngineError::Other(e.to_string()))?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    snapshot.hash(&mut hash);
    let token = format!("{}:{:016x}", s.revision, hash.finish());
    let words = sequence_words(s);
    let rate = s.sequence_rate();
    let mut candidates = Vec::new();
    for r in tx::find_pauses(&words, Tick::from_seconds_f64(min), Tick::from_seconds_f64(keep), rate) {
        candidates.push(json!({"kind": "pause", "text": "", "start": r.start.0, "end": r.end().0}));
    }
    for hit in tx::find_fillers(&words, &fillers) {
        let text = words.get(hit.clone()).unwrap_or_default().iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let confidence = words.get(hit.clone()).unwrap_or_default().iter().map(|w| w.confidence).fold(1.0f32, f32::min);
        let context = words
            .get(hit.start.saturating_sub(2)..(hit.end + 2).min(words.len()))
            .unwrap_or_default()
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let audio_checked = words.get(hit.clone()).is_some_and(|ws| !ws.is_empty() && ws.iter().all(|w| w.filler_requires_check));
        let ranges = tx::filler_ranges(&words, std::slice::from_ref(&hit), rate);
        if ranges.is_empty() {
            let selected = words.get(hit).unwrap_or_default();
            if let (Some(first), Some(last)) = (selected.first(), selected.last()) {
                if last.end <= first.start {
                    continue;
                }
                candidates.push(json!({"kind":"filler","text":text,"start":first.start.0,"end":last.end.0,"confidence":confidence,"context":context,"actionable":false,"reason":"No confirmed interior timing; retranscribe or review the timing on the timeline"}));
            }
        } else {
            let reason = if audio_checked {
                "Audio passes agree on a guarded voiced interior. Review timing and meaning before applying"
            } else {
                "Review the transcript timing and meaning before applying"
            };
            for r in ranges {
                candidates.push(json!({"kind": "filler", "text": text, "start": r.start.0, "end": r.end().0, "confidence": confidence, "context": context,"actionable":true,"reason":reason}));
            }
        }
    }
    if candidates.len() > 10_000 {
        return Err(bad(cmd, "more than 10000 candidates; increase the pause threshold or reduce the filler list"));
    }
    candidates.sort_by_key(|c| c["start"].as_i64());
    for (i, c) in candidates.iter_mut().enumerate() {
        c["id"] = json!(i);
        if let (Some(start), Some(end)) = (c["start"].as_i64(), c["end"].as_i64())
            && let Some(reason) = cleanup_conflict(q, &s.project, &words, TimeRange::from_bounds(Tick(start), Tick(end)))
        {
            c["actionable"] = json!(false);
            c["reason"] = json!(reason);
        }
    }
    Ok(json!({"token": token, "params": {"minSeconds": min, "keepSeconds": keep, "fillers": fillers}, "candidates": candidates}))
}

fn apply_cleanup(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "transcript.applyCleanup";
    let preview = cleanup_preview(s, p.get("params").unwrap_or(&Value::Null))?;
    let token = p.get("token").and_then(Value::as_str).ok_or_else(|| bad(cmd, "token is required"))?;
    if preview["token"].as_str() != Some(token) {
        return Err(bad(cmd, "the project or transcript changed; preview candidates again"));
    }
    let selected = p
        .get("selected")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= 10_000)
        .ok_or_else(|| bad(cmd, "select between 1 and 10000 candidate ids"))?;
    let candidates = preview["candidates"].as_array().ok_or_else(|| bad(cmd, "invalid candidate list"))?;
    let mut ids = std::collections::BTreeSet::new();
    let mut ranges = Vec::new();
    for id in selected {
        let id = id.as_u64().and_then(|id| usize::try_from(id).ok()).ok_or_else(|| bad(cmd, "candidate id must be a nonnegative integer"))?;
        if !ids.insert(id) {
            return Err(bad(cmd, "duplicate candidate id"));
        }
        let c = candidates.get(id).ok_or_else(|| bad(cmd, "unknown candidate id"))?;
        if c["actionable"].as_bool() == Some(false) {
            return Err(bad(cmd, c["reason"].as_str().unwrap_or("candidate is not actionable; preview it again")));
        }
        let start = c["start"].as_i64().ok_or_else(|| bad(cmd, "invalid candidate start"))?;
        let end = c["end"].as_i64().ok_or_else(|| bad(cmd, "invalid candidate end"))?;
        ranges.push(TimeRange::from_bounds(Tick(start), Tick(end)));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    if q.all_tracks().any(|track| track.locked && track.items.iter().any(|clip| ranges.iter().any(|r| clip.end() > r.start))) {
        return Err(bad(cmd, "a locked track would lose sync after the selected cuts; unlock it before applying cleanup"));
    }
    remove_ranges(s, "Apply Reviewed Transcript Cleanup", tx::merge_ranges(ranges))
}

fn remove_pauses(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let min = Tick::from_seconds_f64(f64_p(p, "minSeconds").unwrap_or(1.0));
    let keep = Tick::from_seconds_f64(f64_p(p, "keepSeconds").unwrap_or(0.15));
    let ranges = tx::find_pauses(&words, min, keep, s.sequence_rate());
    remove_ranges(s, "Remove Pauses", ranges)
}

fn remove_fillers(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let fillers: Vec<String> = match p.get("fillers").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => tx::DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect(),
    };
    let hits = tx::find_fillers(&words, &fillers);
    let ranges = tx::filler_ranges(&words, &hits, s.sequence_rate());
    remove_ranges(s, "Remove Filler Words", ranges)
}

fn create_captions(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let d = CaptionRules::default();
    let rules = CaptionRules {
        max_chars: u64_p(p, "maxChars").map(|n| n as usize).unwrap_or(d.max_chars),
        lines: u64_p(p, "lines").map(|n| n as usize).unwrap_or(d.lines),
        min_duration: f64_p(p, "minSeconds").map(Tick::from_seconds_f64).unwrap_or(d.min_duration),
        max_duration: f64_p(p, "maxSeconds").map(Tick::from_seconds_f64).unwrap_or(d.max_duration),
        gap_frames: p.get("gapFrames").and_then(Value::as_i64).unwrap_or(d.gap_frames),
        break_pause: d.break_pause,
    };
    let blocks = tx::caption_blocks(&words, &rules, s.sequence_rate());
    let format = str_p(p, "format").and_then(CaptionFormat::from_name).unwrap_or_default();
    let name = str_p(p, "name").unwrap_or("Transcript").to_string();
    let n = blocks.len();
    let tid = s.edit_sequence("Create Captions", |q, ctx, st| {
        let tid = TrackId(ctx.alloc());
        let mut t = CaptionTrack::new(tid, name, format);
        t.captions = tx::blocks_to_captions(&blocks, ctx);
        q.caption_tracks.insert(0, t);
        st.caption_selection.clear();
        Ok(tid)
    })?;
    Ok(json!({"track": tid.0, "captions": n}))
}

fn models(_: &mut Session, _: &Value) -> Result<Value> {
    let dir = models_dir();
    Ok(json!({
        "available": filmcraft_speech::available(),
        "default": filmcraft_speech::models::DEFAULT_MODEL,
        "dir": dir.as_ref().map(|d| d.to_string_lossy().to_string()),
        "models": filmcraft_speech::models::catalogue().iter().map(|m| json!({
            "id": m.id, "name": m.name, "multilingual": m.multilingual, "description": m.description,
            "license": m.license, "source": m.source, "size": m.size(),
            "installed": dir.as_ref().is_some_and(|d| filmcraft_speech::models::installed(d, m)),
        })).collect::<Vec<_>>(),
    }))
}

/// Download a catalogue model into `<data dir>/models` (feature `speech-download`). Hosts show
/// the size, source and licence (`transcript.models`) and ask before running this.
fn download_model(_: &mut Session, p: &Value) -> Result<Value> {
    let id = str_p(p, "model").unwrap_or(filmcraft_speech::models::DEFAULT_MODEL);
    let m = filmcraft_speech::models::find(id).ok_or_else(|| speech_err(SpeechError::UnknownModel(id.into())))?;
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    #[cfg(feature = "speech-download")]
    {
        filmcraft_speech::models::download(&dir, m, &mut |_, _, _| true).map_err(speech_err)?;
        Ok(json!({"model": m.id, "dir": filmcraft_speech::models::model_dir(&dir, m).to_string_lossy()}))
    }
    #[cfg(not(feature = "speech-download"))]
    {
        let _ = (m, dir);
        Err(EngineError::Other("model downloads are not available in this build (built without the `speech-download` feature)".into()))
    }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "transcript.generate",
            "Transcribe…",
            &["Sequence", "Transcript"],
            r#"{"items":[id]?,"model":"whisper-base"?,"language":"ko|en|auto"?,"diarize":bool?,"maxSpeakers":n?,"preserveFillers":bool=false,"wait":bool=false}"#,
            can_transcribe,
            generate,
            true,
        ),
        spec("transcript.jobs", "Transcription Jobs", &[], "{}", always, crate::transcript_jobs::list, false),
        spec("transcript.retry", "Retry Transcription", &[], r#"{"job":id,"wait":bool=false}"#, can_transcribe, crate::transcript_jobs::retry, true),
        spec(
            "transcript.set",
            "Import Transcript",
            &[],
            r#"{"item":id,"transcript":{"language":str,"speakers":[{"name":str}],"words":[{"text":str,"start":tick,"end":tick,"speaker":n?}]}}"#,
            always,
            set,
            true,
        ),
        spec("transcript.delete", "Delete Transcript", &["Sequence", "Transcript"], r#"{"items":[id]?}"#, has_transcripts, delete, true),
        spec("transcript.inspect", "Inspect Transcript", &[], r#"{"paragraphGapSeconds":f?}"#, always, inspect, false),
        spec("transcript.search", "Search Transcript", &[], r#"{"query":str}"#, always, search, false),
        spec("transcript.models", "List Speech Models", &[], "{}", always, models, false),
        spec(
            "transcript.previewCleanup",
            "Preview Transcript Cleanup",
            &[],
            r#"{"minSeconds":f?,"keepSeconds":f?,"fillers":[str]?}"#,
            has_transcript,
            cleanup_preview,
            false,
        ),
        spec(
            "transcript.applyCleanup",
            "Apply Reviewed Transcript Cleanup",
            &[],
            r#"{"token":str,"params":object,"selected":[candidateId]}"#,
            has_transcript,
            apply_cleanup,
            true,
        ),
        spec("transcript.downloadModel", "Download Speech Model", &[], r#"{"model":"whisper-base"?}"#, can_download, download_model, true),
        spec("transcript.select", "Mark Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, select, true),
        spec("transcript.extract", "Extract Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, true), true),
        spec("transcript.lift", "Lift Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, false), true),
        spec(
            "transcript.renameSpeaker",
            "Rename Speaker…",
            &[],
            r#"{"speaker":"Speaker 1"|index,"name":str,"item":id?}"#,
            has_transcripts,
            rename_speaker,
            true,
        ),
        spec(
            "transcript.removePauses",
            "Remove Pauses",
            &["Sequence", "Transcript"],
            r#"{"minSeconds":f?,"keepSeconds":f?}"#,
            has_transcript,
            remove_pauses,
            true,
        ),
        spec("transcript.removeFillers", "Remove Filler Words", &["Sequence", "Transcript"], r#"{"fillers":[str]?}"#, has_transcript, remove_fillers, true),
        spec(
            "transcript.createCaptions",
            "Create Captions from Transcript…",
            &["Sequence", "Transcript"],
            r#"{"maxChars":n?,"lines":1|2?,"minSeconds":f?,"maxSeconds":f?,"gapFrames":n?,"format":str?,"name":str?}"#,
            has_transcript,
            create_captions,
            true,
        ),
    ]
}
