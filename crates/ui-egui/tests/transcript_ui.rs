//! Headless UI test of the Text panel's Transcript tab: the Transcribe button, the word view,
//! clicking words (In/Out from the selection) and extracting the selected text.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render the window offscreen with wgpu and write
//! `transcript-*.png` there; without it no GPU is needed.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        Self::with_session(session)
    }

    fn with_session(session: Session) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }

    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        self.frames(2);
        match self.harness.render() {
            Ok(img) => {
                std::fs::create_dir_all(&dir).unwrap();
                img.save(dir.join(format!("transcript-{name}.png"))).unwrap();
            }
            Err(e) => eprintln!("snapshot {name} skipped: {e}"),
        }
    }
}

#[test]
fn transcript_tab_selects_and_extracts_words() {
    let mut d = Driver::demo();
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);
    assert_eq!(d.ids("text.transcript.generate"), vec!["text.transcript.generate".to_string()], "empty state offers Transcribe");

    // bring a transcript for the first A1 clip's media (as an agent without a speech model would)
    let mut probe = Session::default();
    probe.execute("file.openDemoProject", json!({})).unwrap();
    let a = probe.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let tk = |s: f64| a.source_in.0 + (s * filmcraft_time::TICKS_PER_SECOND as f64) as i64;
    let words: Vec<Value> = ["Hello", "um", "there", "friend."]
        .iter()
        .enumerate()
        .map(|(i, w)| json!({"text": w, "start": tk(0.5 + i as f64 * 0.5), "end": tk(0.9 + i as f64 * 0.5), "speaker": 0}))
        .collect();
    d.ok("engine.execute", json!({"command": "transcript.set", "params": {"item": a.item.0, "transcript": {"language": "en", "words": words}}}));
    d.frames(3);
    let ids = d.ids("text.transcript.word.");
    assert_eq!(ids.len(), 4, "{ids:?}");
    d.snapshot("words");

    d.ok("ui.click", json!({"id": "text.transcript.word.1"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.transcript.extract"}));
    d.frames(3);
    let r = d.ok("engine.execute", json!({"command": "transcript.inspect", "params": {}}));
    let left: Vec<&str> = r["words"].as_array().unwrap().iter().filter_map(|w| w["text"].as_str()).collect();
    assert_eq!(left, ["Hello", "there", "friend."]);
    assert_eq!(d.ids("text.transcript.word.").len(), 3);
}

#[test]
fn korean_cleanup_review_requires_selection_and_undo_restores_cuts() {
    let mut d = Driver::demo();
    let q = d.harness.state().session.active_sequence().unwrap();
    let clip = q.audio_tracks[0].items[0].clone();
    let tk = |s: f64| clip.source_in.0 + filmcraft_time::Tick::from_seconds_f64(s).0;
    let words: Vec<Value> = [("안녕하세요", 0.2, 0.5), ("음", 0.6, 0.9), ("오늘은", 1.0, 1.4), ("테스트입니다", 2.8, 3.4)]
        .into_iter()
        .map(|(text, a, b)| json!({"text": text, "start": tk(a), "end": tk(b)}))
        .collect();
    d.ok("engine.execute", json!({"command": "transcript.set", "params": {"item": clip.item.0, "transcript": {"language": "ko", "words": words}}}));
    let before = serde_json::to_value(&d.harness.state().session.project).unwrap();
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.transcript.review"}));
    d.frames(3);
    assert!(!d.ids("text.review.find").is_empty());
    d.ok("ui.click", json!({"id": "text.review.find"}));
    d.frames(3);
    assert_eq!(d.ids("text.review.candidate.").len(), 2);
    assert!(!d.harness.state().ui.transcript_review.preserve_fillers);
    d.ok("ui.click", json!({"id":"text.review.preserveFillers"}));
    d.frames(2);
    assert!(d.harness.state().ui.transcript_review.preserve_fillers);
    assert!(d.harness.state().ui.transcript_review.selected.is_empty());
    assert_eq!(serde_json::to_value(&d.harness.state().session.project).unwrap(), before);
    d.snapshot("korean-review");
    d.ok("ui.click", json!({"id": "text.review.listen.0"}));
    d.frames(2);
    assert!(d.harness.state().playback.stop_at.is_some(), "audition has a bounded end");
    assert_eq!(serde_json::to_value(&d.harness.state().session.project).unwrap(), before, "listening is read-only");
    d.ok("engine.execute", json!({"command": "playback.stop", "params": {}}));
    d.ok("ui.click", json!({"id": "text.review.candidate.0"}));
    d.frames(3);
    assert_eq!(d.harness.state().ui.transcript_review.selected, [0]);
    d.ok("ui.click", json!({"id": "text.review.apply"}));
    d.frames(3);
    let r = d.ok("engine.execute", json!({"command": "transcript.inspect", "params": {}}));
    assert!(!r["words"].as_array().unwrap().iter().any(|w| w["text"] == "음"));
    d.ok("ui.click", json!({"id": "text.review.undo"}));
    d.frames(3);
    assert_eq!(serde_json::to_value(&d.harness.state().session.project).unwrap(), before);
}

/// Optional real-file visual check, after scripts/verify_korean_cleanup.py has transcribed it.
#[test]
fn real_korean_project_review_layout() {
    let Some(path) = std::env::var_os("FILMCRAFT_KOREAN_REAL_PROJECT") else { return };
    let mut d = Driver::demo();
    d.ok("engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.transcript.review"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.review.find"}));
    d.frames(3);
    assert_eq!(d.ids("text.review.candidate.").len(), 3);
    assert!(d.harness.state().ui.transcript_review.selected.is_empty());
    d.snapshot("real-korean-review");
}

#[test]
fn background_transcription_keeps_widgets_live_and_offers_cancel_retry() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Waiting(Arc<AtomicBool>);
    impl filmcraft_speech::Transcriber for Waiting {
        fn id(&self) -> String {
            "waiting".into()
        }
        fn transcribe(
            &self,
            _: &[f32],
            _: &filmcraft_speech::Options,
            progress: filmcraft_speech::ProgressFn,
        ) -> Result<filmcraft_project::Transcript, filmcraft_speech::SpeechError> {
            while !self.0.load(Ordering::Relaxed) {
                if !progress(0.4, "Background Korean transcription") {
                    return Err(filmcraft_speech::SpeechError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Ok(filmcraft_project::Transcript { language: "ko".into(), ..Default::default() })
        }
    }
    let release = Arc::new(AtomicBool::new(false));
    let mut session = Session::default();
    session.execute("file.openDemoProject", json!({})).unwrap();
    let item = session.active_sequence().unwrap().audio_tracks[0].items[0].item;
    let before = session.project.clone();
    session.transcriber = Some(Arc::new(Waiting(release.clone())));
    let mut d = Driver::with_session(session);
    d.ok("ui.set", json!({"workspace":"Captions and Graphics"}));
    d.ok("ui.click", json!({"id":"text.tab.Transcript"}));
    d.ok("engine.execute", json!({"command":"transcript.generate","params":{"items":[item.0],"language":"ko"}}));
    d.frames(4);
    assert!(!d.ids("text.transcription.progress").is_empty());
    // Control requests and a tab switch must still complete while the worker is blocked.
    d.ok("ui.click", json!({"id":"text.tab.Captions"}));
    d.ok("ui.click", json!({"id":"text.tab.Transcript"}));
    d.ok("ui.click", json!({"id":"text.transcription.cancel"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let jobs = d.ok("engine.execute", json!({"command":"transcript.jobs"}));
        if jobs[0]["finished"] == true {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(*d.harness.state().session.project, *before);
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.transcription.retry"}));
    release.store(true, Ordering::Relaxed);
    loop {
        let jobs = d.ok("engine.execute", json!({"command":"transcript.jobs"}));
        if jobs.as_array().unwrap().len() == 2 && jobs[1]["finished"] == true {
            assert!(jobs[1]["result"]["error"].is_null());
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(d.harness.state().session.project.transcripts[&item].language, "ko");
}

#[test]
fn long_transcript_layout_is_bounded_and_pages_reach_the_last_word() {
    let mut d = Driver::demo();
    let q = d.harness.state().session.active_sequence().unwrap();
    let clip = q.audio_tracks[0].items[0].clone();
    let base = clip.source_in.0;
    let words: Vec<Value> = (0..10000).map(|i| json!({"text":format!("word{i}"),"start":base+i*10000000,"end":base+(i+1)*10000000})).collect();
    d.ok("engine.execute", json!({"command":"transcript.set","params":{"item":clip.item.0,"transcript":{"language":"ko","source":"manual","words":words}}}));
    d.ok("ui.set", json!({"workspace":"Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.tab.Transcript"}));
    d.frames(3);
    assert!(d.ids("text.transcript.word.").len() <= 500);
    d.ok("ui.click", json!({"id":"text.transcript.page.last"}));
    d.frames(3);
    assert!(!d.ids("text.transcript.word.9999").is_empty());
    assert!(d.ids("text.transcript.word.").len() <= 500);
    // Search must cover words outside the currently displayed page.
    assert!(d.ids("text.transcript.word.7532").is_empty());
    d.ok("ui.click", json!({"id":"text.transcript.search"}));
    d.ok("ui.type", json!({"text":"word7532"}));
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.transcript.page.match"}));
    d.frames(3);
    assert!(!d.ids("text.transcript.word.7532").is_empty());
    assert!(d.ids("text.transcript.word.").len() <= 500);
    d.ok("engine.execute", json!({"command":"edit.undo","params":{}}));
    d.frames(3);
    assert!(d.ids("text.transcript.word.").is_empty());
}

#[test]
fn unconfirmed_korean_filler_checkbox_cannot_select_or_apply_a_cut() {
    let mut d = Driver::demo();
    let clip = d.harness.state().session.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let tk = |s: f64| clip.source_in.0 + filmcraft_time::Tick::from_seconds_f64(s).0;
    let words: Vec<Value> =
        [("앞말", 0.2, 0.5), ("어", 0.6, 1.2), ("다음", 1.3, 1.6)].into_iter().map(|(text, a, b)| json!({"text":text,"start":tk(a),"end":tk(b)})).collect();
    d.ok(
        "engine.execute",
        json!({"command":"transcript.set","params":{"item":clip.item.0,"transcript":{"language":"ko","source":"whisper-small","words":words}}}),
    );
    let before = d.harness.state().session.project.clone();
    d.ok("ui.set", json!({"workspace":"Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.tab.Transcript"}));
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.transcript.review"}));
    d.frames(3);
    d.ok("ui.click", json!({"id":"text.review.find"}));
    d.frames(3);
    assert_eq!(d.ids("text.review.candidate.").len(), 1);
    d.ok("ui.click", json!({"id":"text.review.candidate.0"}));
    d.frames(3);
    assert!(d.harness.state().ui.transcript_review.selected.is_empty());
    d.ok("ui.click", json!({"id":"text.review.apply"}));
    d.frames(3);
    assert_eq!(*d.harness.state().session.project, *before);
}
