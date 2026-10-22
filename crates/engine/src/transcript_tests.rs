use std::sync::Arc;

use serde_json::json;

use crate::Session;
use filmcraft_project::{ItemId, Transcript, Word};
use filmcraft_speech::FixedTranscriber;
use filmcraft_time::Tick;

/// The demo project with a fake transcriber whose transcript fits the first A1 clip's media:
/// "Hello um world." (Speaker 1), a 1.2 s pause, "Second speaker here." (Speaker 2).
fn session() -> (Session, ItemId, Tick) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let a = &q.audio_tracks[0].items[0];
    let (item, sin) = (a.item, a.source_in);
    let sec = |x: f64| sin + Tick::from_seconds_f64(x);
    let mut t = Transcript { language: "en".into(), ..Default::default() };
    for (text, a, b, sp) in
        [("Hello", 0.2, 0.5, 0), ("um", 0.6, 0.9, 0), ("world.", 1.0, 1.4, 0), ("Second", 2.6, 3.0, 1), ("speaker", 3.0, 3.5, 1), ("here.", 3.5, 4.0, 1)]
    {
        let mut w = Word::new(text, sec(a), sec(b));
        w.speaker = Some(sp);
        t.words.push(w);
    }
    t.normalize();
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: t, id: "fixed".into() }));
    let start = a_start(&s);
    (s, item, start)
}

fn a_start(s: &Session) -> Tick {
    s.active_sequence().unwrap().audio_tracks[0].items[0].start
}

fn words(s: &mut Session) -> Vec<String> {
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    r["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap().to_string()).collect()
}

#[test]
fn generate_inspect_search_and_rename() {
    let (mut s, item, start) = session();
    assert!(s.execute("transcript.select", json!({"from": 0})).is_err(), "disabled before transcribing");
    let r = s.execute("transcript.generate", json!({"wait": true, "items": [item.0]})).unwrap();
    assert_eq!(r["items"][0]["words"], 6, "{r}");
    assert_eq!(r["items"][0]["source"], "fixed");
    assert_eq!(s.project.transcripts[&item].words.len(), 6);

    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(words(&mut s), ["Hello", "um", "world.", "Second", "speaker", "here."]);
    assert_eq!(r["words"][0]["start"].as_i64().unwrap(), (start + Tick::from_seconds_f64(0.2)).0, "mapped to sequence time");
    assert_eq!(r["paragraphs"].as_array().unwrap().len(), 2, "speaker change splits paragraphs: {}", r["paragraphs"]);
    assert_eq!(r["speakers"], json!(["Speaker 1", "Speaker 2"]));

    let r = s.execute("transcript.search", json!({"query": "second spea"})).unwrap();
    assert_eq!(r["matches"], json!([{"from": 3, "to": 4, "start": r["matches"][0]["start"], "end": r["matches"][0]["end"]}]));

    s.execute("transcript.renameSpeaker", json!({"speaker": "Speaker 2", "name": "Ann"})).unwrap();
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(r["words"][3]["speaker"], "Ann");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[1].name, "Speaker 2");
    s.execute("transcript.renameSpeaker", json!({"speaker": 0, "item": item.0, "name": "Bo"})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[0].name, "Bo");
    assert!(s.execute("transcript.renameSpeaker", json!({"speaker": "Nobody", "name": "X"})).is_err());

    // transcribing is one undo step
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.transcripts.is_empty());
}

#[test]
fn select_extract_and_lift_by_words() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"wait": true, "items": [item.0]})).unwrap();
    let rate = s.sequence_rate();
    let before = s.active_sequence().unwrap().duration();

    let r = s.execute("transcript.select", json!({"from": 3, "to": 5})).unwrap();
    let (a, b) = (Tick(r["start"].as_i64().unwrap()), Tick(r["end"].as_i64().unwrap()));
    let q = s.active_sequence().unwrap();
    assert_eq!(q.mark_in, Some(a));
    assert_eq!(q.mark_out, Some(b - rate.frame_duration()), "Out is the last frame inside");
    assert_eq!(rate.snap(a), a, "frame aligned");
    assert_eq!(s.playhead(), a);

    // Extract "um": the sequence gets shorter by the word's frames and the word is gone
    let r = s.execute("transcript.extract", json!({"from": 1})).unwrap();
    let cut = Tick(r["end"].as_i64().unwrap()) - Tick(r["start"].as_i64().unwrap());
    assert!(cut > Tick::ZERO);
    assert_eq!(s.active_sequence().unwrap().duration(), before - cut);
    assert!(!words(&mut s).contains(&"um".to_string()), "{:?}", words(&mut s));
    s.active_sequence().unwrap().check().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);

    // Lift leaves a gap: same duration, word gone
    s.execute("transcript.lift", json!({"from": 1, "to": 1})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);
    assert_eq!(words(&mut s), ["Hello", "world.", "Second", "speaker", "here."]);
    assert!(s.execute("transcript.extract", json!({"from": 99})).is_err());
}

#[test]
fn remove_fillers_pauses_and_create_captions() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"wait": true, "items": [item.0]})).unwrap();
    let before = s.active_sequence().unwrap().duration();
    let r = s.execute("transcript.removeFillers", json!({})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    assert!(!words(&mut s).contains(&"um".to_string()));
    let r = s.execute("transcript.removePauses", json!({"minSeconds": 1.0, "keepSeconds": 0.1})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    let after = s.active_sequence().unwrap().duration();
    assert!(after < before - Tick::from_seconds_f64(1.0), "the pause and the filler are gone");
    assert_eq!(words(&mut s).len(), 5);
    s.active_sequence().unwrap().check().unwrap();

    let r = s.execute("transcript.createCaptions", json!({"maxChars": 32})).unwrap();
    assert_eq!(r["captions"], 2, "one caption per speaker: {r}");
    let q = s.active_sequence().unwrap();
    let tr = &q.caption_tracks[0];
    assert_eq!(tr.captions[0].text, "Hello world.");
    assert_eq!(tr.captions[1].speaker.as_deref(), Some("Speaker 2"));
    q.check().unwrap();
}

#[test]
fn set_delete_and_models() {
    let (mut s, item, _) = session();
    let t = json!({"language": "en", "words": [
        {"text": "b", "start": 2000, "end": 3000, "speaker": 1},
        {"text": "a", "start": 0, "end": 1000},
    ]});
    let r = s.execute("transcript.set", json!({"item": item.0, "transcript": t})).unwrap();
    assert_eq!(r["words"], 2);
    let tr = &s.project.transcripts[&item];
    assert_eq!(tr.words[0].text, "a", "normalized: sorted");
    assert_eq!(tr.speakers.len(), 2, "missing speaker labels added");
    assert_eq!(tr.source, "imported");
    assert!(s.execute("transcript.set", json!({"item": 999_999, "transcript": {}})).is_err());
    assert!(s.execute("transcript.set", json!({"item": item.0, "transcript": {"words": 3}})).is_err());

    // survives save/load
    let bytes = filmcraft_format::encode(&s.project, true);
    let back = filmcraft_format::decode(&bytes).unwrap();
    assert_eq!(back.project.transcripts[&item].words.len(), 2);

    s.execute("transcript.delete", json!({"items": [item.0]})).unwrap();
    assert!(s.project.transcripts.is_empty());
    assert!(s.execute("transcript.delete", json!({})).is_err());

    let m = s.execute("transcript.models", json!({})).unwrap();
    assert_eq!(m["available"], filmcraft_speech::available());
    assert!(m["models"].as_array().unwrap().iter().any(|x| x["id"] == "whisper-base"));
}

#[test]
fn generate_without_a_transcriber() {
    let (mut s, item, _) = session();
    s.transcriber = None;
    let e = s.execute("transcript.generate", json!({"wait": true, "items": [item.0], "model": "nope"})).unwrap_err().to_string();
    // a build without speech-to-text says that first: the command is disabled (#97)
    let why = if filmcraft_speech::available() { "unknown speech model" } else { "not available in this build" };
    assert!(e.contains(why), "{e}");
    if !filmcraft_speech::available() {
        assert!(!s.is_enabled("sequence.transcribe"), "Transcribe Sequence follows transcript.generate");
        let e = s.execute("transcript.generate", json!({"wait": true, "items": [item.0]})).unwrap_err().to_string();
        assert!(e.contains("whisper") && e.contains("not available"), "{e}");
        #[cfg(not(feature = "speech-download"))]
        assert!(s.execute("transcript.downloadModel", json!({})).unwrap_err().to_string().contains("not available"));
    }
    // defaults to the media of the open sequence's audio clips
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: Transcript::default(), id: "empty".into() }));
    assert!(s.is_enabled("sequence.transcribe"), "an installed recogniser enables Transcribe Sequence");
    let r = s.execute("transcript.generate", json!({"wait": true, })).unwrap();
    assert!(r["items"].as_array().unwrap().len() >= 2, "{r}");
}

fn korean_review_session() -> (Session, serde_json::Value) {
    let (mut s, item, _) = session();
    let sin = s.active_sequence().unwrap().audio_tracks[0].items[0].source_in;
    let mut t = Transcript { language: "ko".into(), ..Default::default() };
    for (text, start, end) in [("안녕하세요", 0.2, 0.5), ("음,", 0.6, 0.9), ("오늘은", 1.0, 1.4), ("테스트입니다", 2.8, 3.4)] {
        t.words.push(Word::new(text, sin + Tick::from_seconds_f64(start), sin + Tick::from_seconds_f64(end)));
    }
    s.execute("transcript.set", json!({"item": item.0, "transcript": t})).unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    (s, preview)
}

#[test]
fn reviewed_korean_cleanup_is_read_only_selective_and_one_undo() {
    let (mut s, preview) = korean_review_session();
    assert_eq!(preview["candidates"].as_array().unwrap().len(), 2, "{preview}");
    assert_eq!(preview["candidates"][0]["kind"], "filler");
    assert_eq!(preview["candidates"][0]["text"], "음,");
    assert_eq!(preview["candidates"][1]["kind"], "pause");
    let before = serde_json::to_value(&s.project).unwrap();
    let revision = s.revision;
    s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    assert_eq!(s.revision, revision);
    s.execute("playhead.set", json!({"seconds": 0.6})).unwrap();
    let apply = |selected| json!({"token": preview["token"], "params": preview["params"], "selected": selected});
    let result = s.execute("transcript.applyCleanup", apply(json!([0]))).unwrap();
    assert_eq!(result["removed"], 1);
    assert!(!words(&mut s).contains(&"음,".into()));
    assert!(words(&mut s).contains(&"오늘은".into()));
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    let result = s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": preview["params"], "selected": [0, 1]})).unwrap();
    assert_eq!(result["removed"], 2);
    s.active_sequence().unwrap().check().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before, "all cuts restored by one undo");
    s.execute("edit.redo", json!({})).unwrap();
    assert!(!words(&mut s).contains(&"음,".into()));
}

#[test]
fn reviewed_cleanup_rejects_stale_and_hostile_inputs_without_changes() {
    let (mut s, preview) = korean_review_session();
    let before = serde_json::to_value(&s.project).unwrap();
    for selected in [json!([]), json!([99]), json!([0, 0]), json!([-1]), json!(["0"])] {
        assert!(s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": preview["params"], "selected": selected})).is_err());
        assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    }
    for params in [json!({"minSeconds": -1}), json!({"keepSeconds": "bad"}), json!({"fillers": "음"}), json!({"fillers": [null]})] {
        assert!(s.execute("transcript.previewCleanup", params).is_err());
    }
    assert!(s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": {"fillers": ["오늘은"]}, "selected": [0]})).is_err());
    s.execute("transcript.extract", json!({"from": 0})).unwrap();
    let changed = serde_json::to_value(&s.project).unwrap();
    assert!(s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": preview["params"], "selected": [0]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), changed);
}

#[test]
fn reviewed_cleanup_refuses_locked_audio_video_desynchronization() {
    let (mut s, _) = korean_review_session();
    s.edit_sequence("Lock audio", |q, _, _| {
        q.audio_tracks[0].locked = true;
        Ok(())
    })
    .unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    assert!(s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": preview["params"], "selected": [0]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
}

#[test]
fn reviewed_cleanup_refuses_locked_clips_after_the_cut() {
    let (mut s, _) = korean_review_session();
    s.edit_sequence("Lock trailing picture", |q, _, _| {
        let track = &mut q.video_tracks[0];
        let mut clip = track.items[0].clone();
        clip.start = Tick::from_seconds_f64(20.0);
        track.items = vec![clip];
        track.locked = true;
        Ok(())
    })
    .unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    assert!(s.execute("transcript.applyCleanup", json!({"token": preview["token"], "params": preview["params"], "selected": [0]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
}

#[test]
fn reviewed_cleanup_preserves_hidden_second_track_speech() {
    let (mut s, _) = korean_review_session();
    s.edit_sequence("Add another dialogue", |q, ctx, _| {
        let mut track = q.audio_tracks[0].clone();
        track.id = filmcraft_project::TrackId(ctx.alloc());
        track.items.truncate(1);
        track.items[0].id = filmcraft_project::ClipId(ctx.alloc());
        // Same media at a different source time is different speech, not a duplicate channel.
        track.items[0].source_in += Tick::from_seconds_f64(0.5);
        q.audio_tracks.push(track);
        Ok(())
    })
    .unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    let revision = s.revision;
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(preview["candidates"][0]["actionable"], false, "hidden second-track speech must block a global ripple cut");
    assert!(s.execute("transcript.applyCleanup", json!({"token":preview["token"],"params":preview["params"],"selected":[0]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    assert_eq!(s.revision, revision);
}

#[test]
fn reviewed_cleanup_cuts_aligned_duplicate_audio_with_exact_undo_redo() {
    let (mut s, _) = korean_review_session();
    s.edit_sequence("Duplicate dialogue", |q, ctx, _| {
        let mut track = q.audio_tracks[0].clone();
        track.id = filmcraft_project::TrackId(ctx.alloc());
        for clip in &mut track.items {
            clip.id = filmcraft_project::ClipId(ctx.alloc());
        }
        q.audio_tracks.push(track);
        Ok(())
    })
    .unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    s.execute("transcript.applyCleanup", json!({"token":preview["token"],"params":preview["params"],"selected":[0]})).unwrap();
    let after = serde_json::to_value(&s.project).unwrap();
    let q = s.active_sequence().unwrap();
    let ranges = |track: &filmcraft_project::Track| track.items.iter().map(|clip| (clip.start, clip.duration, clip.source_in)).collect::<Vec<_>>();
    assert_eq!(ranges(&q.audio_tracks[0]), ranges(q.audio_tracks.last().unwrap()));
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&s.project).unwrap(), after);
}

#[test]
fn aligned_source_with_different_audio_channels_is_not_a_duplicate() {
    let (mut s, _) = korean_review_session();
    s.edit_sequence("Split other source channel", |q, ctx, _| {
        let mut track = q.audio_tracks[0].clone();
        track.id = filmcraft_project::TrackId(ctx.alloc());
        track.items.truncate(1);
        track.items[0].id = filmcraft_project::ClipId(ctx.alloc());
        track.items[0].source_channels = vec![1];
        q.audio_tracks.push(track);
        Ok(())
    })
    .unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(preview["candidates"][0]["actionable"], false);
    assert!(s.execute("transcript.removeFillers", json!({"fillers":["음"]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
}

#[test]
fn automatic_default_stream_transcript_cannot_cut_custom_channels() {
    let (mut s, _) = korean_review_session();
    let item = s.active_sequence().unwrap().audio_tracks[0].items[0].item;
    let mut transcript = s.project.transcripts[&item].as_ref().clone();
    transcript.source = "whisper-small".into();
    for word in &mut transcript.words {
        if word.normalized() == "음" {
            word.filler_checked = true;
            word.filler_bounds = Some(word.range());
            word.filler_bounds_revision = filmcraft_project::transcript::FILLER_BOUNDS_REVISION;
        }
    }
    s.execute("transcript.set", json!({"item":item.0,"transcript":transcript})).unwrap();
    s.edit_sequence("Select different voice channel", |q, _, _| {
        q.audio_tracks[0].items[0].source_channels = vec![1];
        Ok(())
    })
    .unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(preview["candidates"][0]["actionable"], false);
    assert!(preview["candidates"][0]["reason"].as_str().unwrap().contains("stream/channels"));
    assert!(s.execute("transcript.applyCleanup", json!({"token":preview["token"],"params":preview["params"],"selected":[0]})).is_err());
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
}

#[test]
fn locked_caption_and_legacy_cleanup_also_preserve_the_project() {
    let (mut s, _) = korean_review_session();
    s.execute("captions.newTrack", json!({})).unwrap();
    s.execute("captions.add", json!({"text":"보존할 자막","seconds":20,"durationSeconds":1})).unwrap();
    s.edit_sequence("Lock captions", |q, _, _| {
        q.caption_tracks[0].locked = true;
        Ok(())
    })
    .unwrap();
    let before = serde_json::to_value(&s.project).unwrap();
    let revision = s.revision;
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(preview["candidates"][0]["actionable"], false);
    for (command, params) in [("transcript.removeFillers", json!({"fillers":["음"]})), ("transcript.removePauses", json!({"minSeconds":1}))] {
        assert!(s.execute(command, params).is_err());
        assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
        assert_eq!(s.revision, revision);
    }
}

#[test]
fn zero_duration_transcript_commands_preserve_the_project() {
    let (mut s, item, _) = session();
    let source_in = s.active_sequence().unwrap().audio_tracks[0].items[0].source_in;
    let at = source_in + Tick::from_seconds_f64(0.2);
    s.execute("transcript.set", json!({"item":item.0,"transcript":{"language":"ko","words":[{"text":"음","start":at.0,"end":at.0}]}})).unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    for command in ["transcript.select", "transcript.extract", "transcript.lift"] {
        let error = s.execute(command, json!({"from":0})).unwrap_err().to_string();
        assert!(error.contains("timing"), "{command}: {error}");
        assert!(Arc::ptr_eq(&s.project, &before));
        assert_eq!(s.revision, revision);
    }
    assert_eq!(s.execute("transcript.inspect", json!({})).unwrap()["words"][0]["timingValid"], false);
}

#[test]
fn unconfirmed_automatic_filler_cannot_be_applied_even_with_a_valid_preview_token() {
    let (mut s, item, _) = session();
    let source_in = s.active_sequence().unwrap().audio_tracks[0].items[0].source_in;
    let a = source_in + Tick::from_seconds_f64(0.5);
    let b = source_in + Tick::from_seconds_f64(1.5);
    s.execute("transcript.set", json!({"item":item.0,"transcript":{"source":"whisper-small","language":"ko","words":[{"text":"음","start":a.0,"end":b.0}]}}))
        .unwrap();
    let preview = s.execute("transcript.previewCleanup", json!({})).unwrap();
    assert_eq!(preview["candidates"][0]["actionable"], false);
    let before = s.project.clone();
    assert!(s.execute("transcript.applyCleanup", json!({"token":preview["token"],"params":preview["params"],"selected":[0]})).is_err());
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(s.execute("transcript.removeFillers", json!({"fillers":["음"]})).unwrap()["removed"], 0);
}
