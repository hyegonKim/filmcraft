use serde_json::json;

use crate::Session;
use filmcraft_project::ClipId;
use filmcraft_time::{TICKS_PER_SECOND, Tick};

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

#[test]
fn captions_import_edit_export_and_burn_in() {
    let dir = std::env::temp_dir().join(format!("fc-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let srt = dir.join("talk.srt");
    std::fs::write(
        &srt,
        "\u{feff}1\r\n00:00:00,500 --> 00:00:02,000\r\nHello there\r\n\r\n2\r\n00:00:02,500 --> 00:00:04,000\r\nSecond line\r\nwith two rows\r\n",
    )
    .unwrap();
    let mut s = demo();
    let r = s.execute("file.import", json!({"paths": [srt.to_string_lossy()]})).unwrap();
    assert_eq!(r["documents"][0]["captions"], 2, "{r}");
    let q = s.active_sequence().unwrap();
    let rate = q.settings.frame_rate;
    let tr = q.caption_tracks[0].clone();
    assert_eq!(tr.name, "talk");
    assert_eq!(tr.captions[0].start, rate.snap_nearest(Tick(TICKS_PER_SECOND / 2)), "snapped to frames");
    let first = tr.captions[0].id;
    let second = tr.captions[1].id;

    // edit text, split, merge, trim, move, add at playhead, delete — each one undoable
    s.execute("captions.setText", json!({"caption": first.0, "text": "Hi!", "speaker": "Ann"})).unwrap();
    let split_at = rate.tick_of(rate.frame_at(tr.captions[1].start) + 10);
    let r = s.execute("captions.split", json!({"caption": second.0, "time": split_at.0})).unwrap();
    let right = ClipId(r["captions"][0].as_u64().unwrap());
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].captions.len(), 3);
    s.execute("captions.merge", json!({"captions": [second.0, right.0]})).unwrap();
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].captions.len(), 2);
    s.execute("captions.trim", json!({"caption": second.0, "edge": "out", "deltaFrames": 12})).unwrap();
    s.execute("captions.move", json!({"captions": [second.0], "deltaFrames": 24})).unwrap();
    s.execute("playhead.set", json!({"seconds": 8.0})).unwrap();
    let r = s.execute("captions.add", json!({"text": "Added"})).unwrap();
    let added = ClipId(r["caption"].as_u64().unwrap());
    assert_eq!(s.state.caption_selection, vec![added]);
    s.execute("edit.clear", json!({})).unwrap();
    assert!(s.active_sequence().unwrap().find_caption(added).is_none(), "Clear deletes selected captions");
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active_sequence().unwrap().find_caption(added).is_some());
    let list = s.execute("captions.list", json!({})).unwrap();
    assert_eq!(list["tracks"][0]["captions"][0]["text"], "Hi!");
    assert_eq!(list["tracks"][0]["captions"][0]["speaker"], "Ann");
    s.active_sequence().unwrap().check().unwrap();

    // export in all three formats and read back: same texts
    for ext in ["srt", "vtt", "scc"] {
        let path = dir.join(format!("out.{ext}")).to_string_lossy().to_string();
        let r = s.execute("captions.export", json!({"path": path})).unwrap();
        assert_eq!(r["captions"], 3, "{ext}: {r}");
        let back = filmcraft_captions::parse(&std::fs::read(&path).unwrap(), filmcraft_captions::Format::from_name(ext).unwrap()).unwrap();
        let texts: Vec<String> = back.cues.iter().map(|c| filmcraft_project::plain_text(&c.text)).collect();
        assert_eq!(texts.len(), 3, "{ext}");
        assert_eq!(texts[0], "Hi!", "{ext}");
    }

    // the program renders captions; hiding the track removes them
    s.execute("playhead.set", json!({"seconds": 1.0})).unwrap();
    let with = s.render_program(0.25).unwrap();
    s.execute("captions.hideAll", json!({})).unwrap();
    let without = s.render_program(0.25).unwrap();
    let diff = with.px.iter().zip(&without.px).filter(|(a, b)| (*a - *b).abs() > 0.05).count();
    assert!(diff > 200, "captions change the picture ({diff} samples)");
    let half = with.w * 4 * (with.h / 2);
    let top_diff = with.px[..half].iter().zip(&without.px[..half]).filter(|(a, b)| (*a - *b).abs() > 0.05).count();
    assert_eq!(top_diff, 0, "bottom-anchored captions stay in the lower half");
    s.execute("captions.showAll", json!({})).unwrap();
    let plan = filmcraft_render::plan::plan_frame(
        &s.project,
        s.state.active_sequence.unwrap(),
        s.playhead(),
        filmcraft_render::RenderOptions { scale: 0.25, captions: true, ..Default::default() },
        &s.media.provider(s.project.clone(), s.services.clone()),
    );
    let cpu = filmcraft_render::plan::execute_cpu(&plan);
    let pd = cpu.px.iter().zip(&with.px).filter(|(a, b)| (*a - *b).abs() > 0.02).count();
    assert!(pd < cpu.px.len() / 200, "the GPU plan matches the CPU render ({pd})");

    // save/open keeps caption tracks
    let proj = dir.join("cap.fcproj").to_string_lossy().to_string();
    s.execute("file.saveAs", json!({"path": proj})).unwrap();
    let before = s.active_sequence().unwrap().caption_tracks.clone();
    s.execute("file.open", json!({"path": proj})).unwrap();
    assert_eq!(s.active_sequence().unwrap().caption_tracks, before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn caption_track_follows_sync_locked_insert() {
    let mut s = demo();
    s.execute("captions.newTrack", json!({"format": "CEA-608"})).unwrap();
    s.execute("playhead.set", json!({"seconds": 4.0})).unwrap();
    s.execute("captions.add", json!({"text": "later"})).unwrap();
    let start = s.active_sequence().unwrap().caption_tracks[0].captions[0].start;
    let item = s.project.items.values().find(|i| i.as_media().is_some_and(|m| m.info.has_video())).unwrap().id;
    s.execute("source.open", json!({"item": item.0})).unwrap();
    s.execute("playhead.set", json!({"seconds": 0.0})).unwrap();
    s.execute("source.insert", json!({})).unwrap();
    let after = s.active_sequence().unwrap().caption_tracks[0].captions[0].start;
    assert!(after > start, "caption moved right with the insert");
    s.active_sequence().unwrap().check().unwrap();
}

#[test]
fn caption_commands_need_tracks_and_navigate() {
    let mut s = demo();
    assert!(s.execute("captions.split", json!({})).is_err(), "disabled without captions");
    for (sec, text) in [(1.0, "one"), (3.0, "two"), (6.0, "three")] {
        s.execute("playhead.set", json!({"seconds": sec})).unwrap();
        s.execute("captions.add", json!({"text": text, "durationSeconds": 1.0})).unwrap();
    }
    s.execute("playhead.set", json!({"seconds": 0.0})).unwrap();
    s.execute("captions.next", json!({})).unwrap();
    s.execute("captions.next", json!({})).unwrap();
    let id = s.state.caption_selection[0];
    assert_eq!(s.active_sequence().unwrap().find_caption(id).unwrap().1.text, "two");
    s.execute("captions.previous", json!({})).unwrap();
    let id = s.state.caption_selection[0];
    assert_eq!(s.active_sequence().unwrap().find_caption(id).unwrap().1.text, "one");
    s.execute("captions.setStyle", json!({"color": "#ffff00", "anchor": "top", "size": 40})).unwrap();
    let st = &s.active_sequence().unwrap().caption_tracks[0].style;
    assert_eq!(st.color, [255, 255, 0, 255]);
    assert!(s.execute("captions.setStyle", json!({"anchor": "sideways"})).is_err());
    s.execute("captions.setTimes", json!({"caption": id.0, "startSeconds": 0.5, "endSeconds": 2.5})).unwrap();
    assert!(s.execute("captions.setTimes", json!({"caption": id.0, "endSeconds": 3.5})).is_err(), "would overlap `two`");
    s.execute("captions.deleteTrack", json!({"track": "C1"})).unwrap();
    assert!(s.active_sequence().unwrap().caption_tracks.is_empty());
}

#[test]
fn caption_list_track_filter_never_falls_back_to_every_track() {
    let mut s = demo();
    s.execute("captions.newTrack", json!({})).unwrap();
    s.execute("captions.newTrack", json!({})).unwrap();
    let all = s.execute("captions.list", json!({})).unwrap();
    let ids: Vec<u64> = all["tracks"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 2, "{all}");
    for (i, id) in ids.iter().enumerate() {
        for sel in [json!(id), json!(format!("C{}", i + 1))] {
            let r = s.execute("captions.list", json!({"track": sel})).unwrap();
            let got: Vec<u64> = r["tracks"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect();
            assert_eq!(got, vec![*id], "{sel}");
        }
    }
    let missing = ids.iter().max().unwrap() + 999;
    for sel in [json!(missing), json!("C3"), json!("C0"), json!("nope")] {
        assert!(s.execute("captions.list", json!({"track": sel})).is_err(), "{sel} names no caption track");
    }
}

fn caption_spans(s: &Session) -> Vec<(f64, f64, String)> {
    let secs = |t: Tick| (t.seconds() * 10.0).round() / 10.0; // the demo sequence snaps to 23.976 fps frames
    s.active_sequence().unwrap().caption_tracks[0].captions.iter().map(|c| (secs(c.start), secs(c.end()), c.text.clone())).collect()
}

#[test]
fn captions_duplicate_copies_and_overwrites() {
    let mut s = demo();
    for (sec, text) in [(1.0, "one"), (3.0, "two")] {
        s.execute("playhead.set", json!({"seconds": sec})).unwrap();
        s.execute("captions.add", json!({"text": text, "durationSeconds": 1.0})).unwrap();
    }
    let one = s.active_sequence().unwrap().caption_tracks[0].captions[0].id;
    // Option-drag: a copy 5 s later, the original stays; the copy is selected
    let r = s.execute("captions.duplicate", json!({"captions": [one.0], "delta": 5 * TICKS_PER_SECOND})).unwrap();
    assert_eq!(caption_spans(&s), vec![(1.0, 2.0, "one".into()), (3.0, 4.0, "two".into()), (6.0, 7.0, "one".into())]);
    assert_eq!(s.state.caption_selection.len(), 1);
    assert_eq!(r["captions"][0].as_u64(), Some(s.state.caption_selection[0].0));
    // a copy dropped half over `two` overwrites that half
    s.execute("captions.duplicate", json!({"captions": [one.0], "delta": TICKS_PER_SECOND * 3 / 2})).unwrap();
    assert_eq!(caption_spans(&s)[1..3], [(2.5, 3.5, "one".into()), (3.5, 4.0, "two".into())]);
    // never before zero; zero delta is nothing
    s.execute("captions.duplicate", json!({"captions": [one.0], "delta": -100 * TICKS_PER_SECOND})).unwrap();
    assert_eq!(caption_spans(&s)[0], (0.0, 1.0, "one".into()));
    assert!(s.execute("captions.duplicate", json!({"captions": [one.0], "delta": 0})).is_err());
    assert!(s.execute("captions.duplicate", json!({"captions": [one.0]})).is_err(), "needs a delta");
    assert!(s.execute("captions.duplicate", json!({"captions": [], "delta": TICKS_PER_SECOND})).is_err(), "needs a caption");
    let before = s.active_sequence().unwrap().caption_tracks[0].captions.len();
    s.execute("captions.duplicate", json!({"captions": [one.0, one.0], "delta": 20 * TICKS_PER_SECOND})).unwrap();
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].captions.len(), before + 1, "a caption named twice is copied once");
    s.active_sequence().unwrap().check().unwrap();
}

#[test]
fn captions_copy_cut_and_paste() {
    let mut s = demo();
    // Copy / Paste with only captions selected copies captions, pasted at the playhead
    s.execute("playhead.set", json!({"seconds": 1.0})).unwrap();
    s.execute("captions.add", json!({"text": "hi", "durationSeconds": 1.0})).unwrap();
    assert!(s.state.selection.is_empty());
    assert_eq!(s.execute("edit.copy", json!({})).unwrap()["copied"], 1);
    s.execute("playhead.set", json!({"seconds": 10.0})).unwrap();
    s.execute("edit.paste", json!({})).unwrap();
    assert_eq!(caption_spans(&s), vec![(1.0, 2.0, "hi".into()), (10.0, 11.0, "hi".into())]);
    assert!((s.playhead().seconds() - 11.0).abs() < 0.05, "the playhead moves past the paste");
    // Cut takes it away and Paste puts it back
    s.execute("edit.cut", json!({})).unwrap();
    assert_eq!(caption_spans(&s), vec![(1.0, 2.0, "hi".into())]);
    s.execute("playhead.set", json!({"seconds": 4.0})).unwrap();
    s.execute("edit.paste", json!({})).unwrap();
    assert_eq!(caption_spans(&s)[1], (4.0, 5.0, "hi".into()));
    // copying clips afterwards makes Paste place clips again, not captions
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
    s.execute("timeline.select", json!({"clips": [clip.0]})).unwrap();
    s.execute("edit.copy", json!({})).unwrap();
    assert!(s.state.caption_clipboard.is_empty() && !s.state.clipboard.is_empty());
    // undo takes back a caption paste as one step
    let mut s = demo();
    s.execute("captions.add", json!({"text": "x", "durationSeconds": 1.0})).unwrap();
    s.execute("edit.copy", json!({})).unwrap();
    s.execute("playhead.set", json!({"seconds": 5.0})).unwrap();
    s.execute("edit.paste", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(caption_spans(&s).len(), 1);
}

/// The demo with a transcript on the first A1 clip's media (sequence words "Hello world. Second
/// speaker here.").
fn demo_with_transcript() -> Session {
    let mut s = demo();
    let q = s.active_sequence().unwrap();
    let clip = &q.audio_tracks[0].items[0];
    let (item, sin) = (clip.item, clip.source_in);
    let at = |x: f64| (sin + Tick::from_seconds_f64(x)).0;
    let words: Vec<_> = [("Hello", 0.2, 0.5), ("world.", 0.6, 1.0), ("Second", 2.6, 3.0), ("speaker", 3.0, 3.5), ("here.", 3.5, 4.0)]
        .iter()
        .map(|&(text, a, b)| json!({"text": text, "start": at(a), "end": at(b)}))
        .collect();
    s.execute("transcript.set", json!({"item": item.0, "transcript": {"language": "en", "words": words}})).unwrap();
    s
}

#[test]
fn auto_generate_captions_without_a_transcript_is_an_error_and_changes_nothing() {
    let mut s = demo();
    s.transcriber = None;
    if crate::transcript::can_transcribe(&s).is_ok() {
        return; // a speech-to-text build transcribes instead (covered by the transcript tests)
    }
    let before = s.project.clone();
    let e = s.execute("captions.autoGenerate", json!({"language": "ko"})).unwrap_err().to_string();
    assert!(e.contains("no transcript"), "{e}");
    assert_eq!(*s.project, *before, "no placeholder transcript or captions were made");
    assert!(s.project.transcripts.is_empty());
}

#[test]
fn auto_generate_captions_lays_out_the_transcript() {
    let mut s = demo_with_transcript();
    let r = s.execute("captions.autoGenerate", json!({})).unwrap();
    assert_eq!(r["transcribed"], false);
    assert_eq!(r["replaced"], false);
    let q = s.active_sequence().unwrap();
    assert_eq!(q.caption_tracks.len(), 1);
    let text: Vec<&str> = q.caption_tracks[0].captions.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(text.join(" ").split_whitespace().collect::<Vec<_>>(), ["Hello", "world.", "Second", "speaker", "here."]);
    assert_eq!(r["captions"].as_u64(), Some(text.len() as u64));

    // A named track that has captions is replaced only when asked.
    let (track, first) = (q.caption_tracks[0].id.0, q.caption_tracks[0].captions[0].id.0);
    s.execute("captions.setText", json!({"caption": first, "text": "Kept"})).unwrap();
    let e = s.execute("captions.autoGenerate", json!({"track": track})).unwrap_err().to_string();
    assert!(e.contains("replace"), "{e}");
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].captions[0].text, "Kept");
    let r = s.execute("captions.autoGenerate", json!({"track": track, "replace": true, "language": "en"})).unwrap();
    assert_eq!(r["replaced"], true);
    let q = s.active_sequence().unwrap();
    assert_eq!(q.caption_tracks.len(), 1);
    assert_eq!(q.caption_tracks[0].language, "en");
    assert!(q.caption_tracks[0].captions[0].text.starts_with("Hello"));
}

/// Editing a caption changes the caption only: the transcript's words and their timings stay.
#[test]
fn editing_a_caption_keeps_the_transcript() {
    let mut s = demo_with_transcript();
    s.execute("captions.autoGenerate", json!({})).unwrap();
    let item = s.active_sequence().unwrap().audio_tracks[0].items[0].item;
    let words = s.project.transcripts[&item].words.clone();
    let first = s.active_sequence().unwrap().caption_tracks[0].captions[0].id;
    s.execute("captions.setText", json!({"caption": first.0, "text": "한국어 자막 수정"})).unwrap();
    assert_eq!(s.project.transcripts[&item].words, words);
}

#[test]
fn hostile_caption_layout_parameters_are_errors_or_clamped_never_panics() {
    let mut s = demo_with_transcript();
    for p in [
        json!({"maxChars": u64::MAX, "lines": u64::MAX, "gapFrames": i64::MAX, "minSeconds": 1e300, "maxSeconds": -5.0}),
        json!({"maxChars": 0, "lines": 0, "gapFrames": i64::MIN, "minSeconds": -1e300, "maxSeconds": 1e300}),
        json!({"format": "nonsense", "name": "", "language": "   "}),
    ] {
        assert!(s.execute("captions.autoGenerate", p.clone()).is_ok(), "{p}");
        // the same layout rules: Create Captions overflowed `maxChars × lines` before
        assert!(s.execute("transcript.createCaptions", p.clone()).is_ok(), "{p}");
    }
    let before = s.project.clone();
    for p in [json!({"track": "C999"}), json!({"track": 123_456_789}), json!({"track": true}), json!({"track": "Cx"}), json!({"track": -1})] {
        assert!(s.execute("captions.autoGenerate", p.clone()).is_err(), "{p}");
    }
    assert_eq!(*s.project, *before, "refused requests change nothing");
}

#[test]
fn open_gzipped_prproj_via_file_open() {
    let dir = std::env::temp_dir().join(format!("fc-prproj-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prproj = dir.join("premiere_2026.prproj");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <Project ObjectID="1" Name="Premiere 2026">
    <Sequence ObjectRef="2"/>
  </Project>
  <Sequence ObjectID="2">
    <Name>Timeline 01</Name>
    <FrameRate>10584000000</FrameRate>
    <FrameRect>0,0,1920,1080</FrameRect>
    <TrackGroup>
      <VideoTrackGroup ObjectRef="3"/>
      <CaptionTrackGroup ObjectRef="4"/>
    </TrackGroup>
  </Sequence>
  <VideoTrackGroup ObjectID="3">
    <VideoClipTrack ObjectID="30">
      <Name>V1</Name>
      <TrackItems>
        <VideoClipTrackItem ObjectID="100">
          <Start>0</Start>
          <End>762048000000</End>
          <Name>clip_01.mp4</Name>
        </VideoClipTrackItem>
      </TrackItems>
    </VideoClipTrack>
  </VideoTrackGroup>
  <CaptionTrackGroup ObjectID="4">
    <CaptionTrack ObjectID="40" Name="C1">
      <Caption ObjectID="400">
        <Start>0</Start>
        <End>508032000000</End>
        <Text>프리미어 프로 자막 트랙</Text>
      </Caption>
    </CaptionTrack>
  </CaptionTrackGroup>
</PremiereData>"#;
    let gz = filmcraft_interchange::prproj::gzip_compress_stored(xml.as_bytes());
    std::fs::write(&prproj, &gz).unwrap();

    let mut s = Session::default();
    s.execute("file.open", json!({"path": prproj.to_string_lossy()})).unwrap();
    let seq = s.active_sequence().expect("active sequence should be opened from .prproj");
    assert_eq!(seq.video_tracks[0].items.len(), 1);
    assert_eq!(seq.video_tracks[0].items[0].duration, Tick::from_seconds_f64(3.0));
    assert_eq!(seq.caption_tracks.len(), 1);
    assert_eq!(seq.caption_tracks[0].captions[0].text, "프리미어 프로 자막 트랙");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Opening a Premiere project that cannot be read used to replace the open project with an empty
/// one first; the open project, its history and its path now stay as they were.
#[test]
fn a_premiere_project_that_cannot_be_read_leaves_the_open_project_alone() {
    let dir = std::env::temp_dir().join(format!("fc-prproj-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saved = dir.join("mine.fcproj");
    let mut s = demo();
    s.execute("file.saveAs", json!({"path": saved.to_string_lossy()})).unwrap();
    s.execute("markers.add", json!({})).unwrap();
    assert!(s.is_enabled("edit.undo"));
    let (project, path) = (s.project.clone(), s.path.clone());

    let xml = br#"<PremiereData Version="3"><Sequence ObjectID="1"><Name>S</Name></Sequence></PremiereData>"#;
    let good = filmcraft_interchange::prproj::gzip_compress_stored(xml);
    let native = filmcraft_format::encode(&s.project, false);
    for (name, bytes) in [
        ("garbage.prproj", vec![0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ("truncated.prproj", good[..good.len() - 6].to_vec()),
        ("json.prproj", native),
        ("text.prproj", b"not xml at all".to_vec()),
    ] {
        let f = dir.join(name);
        std::fs::write(&f, &bytes).unwrap();
        assert!(s.execute("file.open", json!({"path": f.to_string_lossy()})).is_err(), "{name}");
        assert_eq!(*s.project, *project, "{name}: the open project is kept");
        assert_eq!(s.path, path, "{name}: and its path");
        assert!(s.is_enabled("edit.undo"), "{name}: and its history");
    }
    s.execute("edit.undo", json!({})).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// There is no Premiere project writer: saving under a `.prproj` name is refused and writes nothing.
#[test]
fn saving_as_a_premiere_project_is_refused() {
    let dir = std::env::temp_dir().join(format!("fc-prproj-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = demo();
    for (cmd, name) in [("file.saveAs", "cut.prproj"), ("file.saveCopy", "copy.PRPROJ"), ("file.save", "save.PrProj")] {
        let f = dir.join(name);
        let e = s.execute(cmd, json!({"path": f.to_string_lossy()})).unwrap_err().to_string();
        assert!(e.contains("cannot write Premiere projects"), "{cmd}: {e}");
        assert!(!f.exists(), "{cmd} wrote {name}");
    }
    assert_eq!(s.path, None);
    let _ = std::fs::remove_dir_all(&dir);
}
