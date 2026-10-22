//! Transcription workers only publish complete results; the session commits one undo step.
use crate::commands::{bad, bool_p, str_p, u64_p};
use crate::{EngineError, Result, Session};
use filmcraft_project::{ItemId, Project, Transcript};
use filmcraft_speech::{Options, Transcriber};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::Ordering};

type Output = std::result::Result<Vec<(ItemId, Transcript)>, String>;
pub(crate) struct Task {
    id: u64,
    epoch: u64,
    before: Arc<Project>,
    params: Value,
    items: Vec<ItemId>,
    output: Arc<Mutex<Option<Output>>>,
    applied: bool,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn change_document(s: &mut Session) {
    s.document_epoch = s.document_epoch.wrapping_add(1);
    for t in &s.transcript_jobs {
        if !t.applied
            && let Some(j) = s.jobs.iter().find(|j| j.id == t.id)
        {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) fn start(s: &mut Session, p: &Value, items: Vec<ItemId>, opts: Options) -> Result<Value> {
    poll(s);
    if s.transcript_jobs.iter().any(|t| !t.applied) {
        return Err(EngineError::Other("a transcription is already running; cancel it first".into()));
    }
    let model = str_p(p, "model").unwrap_or(&s.prefs.media_analysis.whisper_model).to_owned();
    if s.transcriber.is_none() && filmcraft_speech::models::find(&model).is_none() {
        return Err(bad("transcript.generate", "unknown speech model"));
    }
    if !p.is_object() {
        return Err(bad("transcript.generate", "parameters must be an object"));
    }
    let mut params = p.clone();
    params["items"] = json!(items.iter().map(|i| i.0).collect::<Vec<_>>());
    params["language"] = json!(opts.language.as_deref().unwrap_or("auto"));
    params["diarize"] = json!(opts.diarize);
    params["preserveFillers"] = json!(opts.preserve_fillers);
    params["maxSpeakers"] = json!(opts.max_speakers);
    params["model"] = json!(model);
    params["wait"] = json!(false);
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0).saturating_add(1);
    let job = crate::Job { id, label: "Transcription".into(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1_000_000, Ordering::Relaxed);
    *lock(&job.progress.status) = "Loading speech model".into();
    let output = Arc::new(Mutex::new(None));
    let (project, media, services, injected) = (s.project.clone(), s.media.clone(), s.services.clone(), s.transcriber.clone());
    let (out, prog) = (output.clone(), job.progress.clone());
    let work_items = items.clone();
    let dir = crate::transcript::models_dir();
    let run = move || {
        let result = (|| -> Output {
            let check = || if prog.cancel.load(Ordering::Relaxed) { Err("stopped: nothing was changed".to_owned()) } else { Ok(()) };
            check()?;
            let t: Arc<dyn Transcriber> = match injected {
                Some(t) => t,
                None => filmcraft_speech::load(&dir.ok_or("no speech models directory")?, &model).map_err(|e| e.to_string())?,
            };
            check()?;
            let mut done = Vec::new();
            for (index, item) in work_items.iter().enumerate() {
                check()?;
                let (_, m, _) = project.resolve_media(*item).ok_or("media no longer exists")?;
                let src = media.full_res_source(&project, *item, &*services).ok_or("cannot open media")?;
                if !src.info().has_audio() {
                    return Err("the selected media has no audio".into());
                }
                let sr = filmcraft_speech::SAMPLE_RATE;
                let len = m.duration().to_units_floor(sr as i64);
                // Bound untrusted durations and keep decoding interruptible in five-second blocks.
                if len <= 0 || len > i64::from(sr) * 4 * 60 * 60 {
                    return Err("transcription supports media up to four hours per item".into());
                }
                let len = usize::try_from(len).map_err(|e| e.to_string())?;
                let mut audio = Vec::new();
                audio.try_reserve_exact(len).map_err(|e| e.to_string())?;
                for offset in (0..len).step_by(sr as usize * 5) {
                    check()?;
                    let n = (len - offset).min(sr as usize * 5);
                    let buf = src.audio(offset as i64, n, sr).map_err(|e| e.to_string())?;
                    let mono = filmcraft_speech::downmix(&buf.channels);
                    if mono.len() != n {
                        return Err("audio decoder returned an incomplete block".into());
                    }
                    audio.extend(mono);
                    let frac = (index as f32 + 0.1 * audio.len() as f32 / len as f32) / work_items.len() as f32;
                    prog.done.store((frac * 1_000_000.0) as u64, Ordering::Relaxed);
                    *lock(&prog.status) = format!("Decoding audio ({}/{})", index + 1, work_items.len());
                }
                check()?;
                let mut tr = t
                    .transcribe(&audio, &opts, &mut |fraction, message| {
                        let frac = (index as f32 + 0.1 + 0.89 * fraction.clamp(0.0, 1.0)) / work_items.len() as f32;
                        prog.done.fetch_max((frac * 1_000_000.0) as u64, Ordering::Relaxed);
                        *lock(&prog.status) = format!("{message} ({}/{})", index + 1, work_items.len());
                        !prog.cancel.load(Ordering::Relaxed)
                    })
                    .map_err(|e| e.to_string())?;
                check()?;
                tr.normalize();
                tr.check()?;
                done.push((*item, tr));
            }
            Ok(done)
        })();
        *lock(&out) = Some(result);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let wait = bool_p(p, "wait").unwrap_or(false) || cfg!(target_arch = "wasm32");
    // Register only after spawn succeeds, so a thread-creation failure is retryable.
    if wait {
        run();
    } else {
        std::thread::Builder::new().name("filmcraft-transcribe".into()).spawn(run).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    s.jobs.push(job);
    s.transcript_jobs.push(Task { id, epoch: s.document_epoch, before: s.project.clone(), params, items, output, applied: false });
    if wait {
        poll(s);
        let j = s.jobs.iter().find(|j| j.id == id).ok_or_else(|| EngineError::Other("missing job".into()))?;
        if let Some(Err(e)) = &*lock(&j.result) {
            return Err(EngineError::Other(e.clone()));
        }
        let t = s.transcript_jobs.iter().find(|t| t.id == id).ok_or_else(|| EngineError::Other("missing transcription".into()))?;
        return Ok(
            json!({"job": id, "items": t.items.iter().filter_map(|i| s.project.transcripts.get(i).map(|t| json!({"item":i.0,"words":t.words.len(),"speakers":t.speakers.len(),"language":t.language,"source":t.source}))).collect::<Vec<_>>(), "skipped": []}),
        );
    }
    Ok(json!({"job":id}))
}

pub(crate) fn poll(s: &mut Session) {
    for index in 0..s.transcript_jobs.len() {
        if s.transcript_jobs[index].applied {
            continue;
        }
        let id = s.transcript_jobs[index].id;
        let Some(job) = s.jobs.iter().find(|j| j.id == id).cloned() else { continue };
        let output = lock(&s.transcript_jobs[index].output).take();
        let Some(output) = output else {
            if let Some(Err(error)) = &*lock(&job.result) {
                *lock(&job.progress.status) = error.clone();
                s.transcript_jobs[index].applied = true;
            }
            continue;
        };
        let task = &s.transcript_jobs[index];
        let valid = task.epoch == s.document_epoch
            && task.items.iter().all(|i| s.project.item(*i) == task.before.item(*i) && s.project.transcripts.get(i) == task.before.transcripts.get(i));
        let result = if job.progress.cancel.load(Ordering::Relaxed) {
            Err("stopped: nothing was changed".into())
        } else if !valid {
            Err("media or transcript changed during transcription; nothing was changed; retry to analyse the current version".into())
        } else {
            output.and_then(|done| {
                s.edit("Transcribe", move |pr, _| {
                    for (i, tr) in done {
                        pr.transcripts.insert(i, Arc::new(tr));
                    }
                    Ok(())
                })
                .map_err(|e| e.to_string())
            })
        };
        let status = match &result {
            Ok(()) => "Transcription complete".into(),
            Err(e) => e.clone(),
        };
        *lock(&job.progress.status) = status.clone();
        if result.is_ok() {
            job.progress.done.store(1_000_000, Ordering::Relaxed);
        } else {
            *lock(&job.progress.error) = Some(status);
        }
        *lock(&job.result) = Some(result.map(|()| filmcraft_export::Report {
            path: String::new(),
            frames: 0,
            seconds: 0.0,
            bytes: 0,
            render_fps: 0.0,
            extra_files: Vec::new(),
        }));
        job.progress.finished.store(true, Ordering::Release);
        s.transcript_jobs[index].applied = true;
    }
}

pub(crate) fn list(s: &mut Session, _: &Value) -> Result<Value> {
    poll(s);
    Ok(json!(
        s.transcript_jobs
            .iter()
            .filter_map(|t| s.jobs.iter().find(|j| j.id == t.id).map(|j| {
                let mut v = j.to_json();
                v["canRetry"] = json!(t.applied && t.epoch == s.document_epoch && lock(&j.result).as_ref().is_some_and(|r| r.is_err()));
                v
            }))
            .collect::<Vec<_>>()
    ))
}
pub(crate) fn retry(s: &mut Session, p: &Value) -> Result<Value> {
    poll(s);
    let id = u64_p(p, "job").ok_or_else(|| bad("transcript.retry", "need job"))?;
    let task = s.transcript_jobs.iter().find(|t| t.id == id).ok_or_else(|| bad("transcript.retry", "no such transcription"))?;
    let failed = s.jobs.iter().find(|j| j.id == id).is_some_and(|j| lock(&j.result).as_ref().is_some_and(|r| r.is_err()));
    if !task.applied || !failed || task.epoch != s.document_epoch {
        return Err(bad("transcript.retry", "only failed or cancelled jobs in this document can be retried"));
    }
    let mut params = task.params.clone();
    params["wait"] = json!(bool_p(p, "wait").unwrap_or(false));
    s.execute("transcript.generate", params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_speech::{ProgressFn, SpeechError};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    struct Controlled {
        entered: AtomicBool,
        release: AtomicBool,
        calls: AtomicUsize,
        fail_first: bool,
        received: Mutex<Vec<Options>>,
    }
    impl Transcriber for Controlled {
        fn id(&self) -> String {
            "controlled".into()
        }
        fn transcribe(&self, _: &[f32], opts: &Options, progress: ProgressFn) -> std::result::Result<Transcript, SpeechError> {
            lock(&self.received).push(opts.clone());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                if !progress(0.5, "working") {
                    return Err(SpeechError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if self.fail_first && call == 0 {
                return Err(SpeechError::Model("test failure".into()));
            }
            Ok(Transcript {
                language: "ko".into(),
                words: vec![filmcraft_project::Word::new("안녕하세요", filmcraft_time::Tick(1), filmcraft_time::Tick(2))],
                ..Default::default()
            })
        }
    }
    fn setup(fail: bool) -> (Session, ItemId, Arc<Controlled>) {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let item = s.active_sequence().unwrap().audio_tracks[0].items[0].item;
        let control = Arc::new(Controlled {
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            fail_first: fail,
            received: Mutex::new(Vec::new()),
        });
        s.transcriber = Some(control.clone());
        (s, item, control)
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(10);
        while !condition() {
            assert!(web_time::Instant::now() < deadline, "worker timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    fn finish(s: &mut Session) {
        until(|| {
            poll(s);
            s.transcript_jobs.iter().all(|t| t.applied)
        });
    }
    #[test]
    fn background_cancel_retry_atomic_undo() {
        let (mut s, item, c) = setup(false);
        let before = s.project.clone();
        let r = s.execute("transcript.generate", json!({"items":[item.0],"language":"ko","preserveFillers":true})).unwrap();
        until(|| c.entered.load(Ordering::SeqCst));
        let j = list(&mut s, &json!({})).unwrap();
        assert!(j[0]["progress"].as_f64().unwrap() > 0.0);
        assert!(Arc::ptr_eq(&s.project, &before));
        s.execute("jobs.cancel", json!({"job":r["job"]})).unwrap();
        finish(&mut s);
        assert!(Arc::ptr_eq(&s.project, &before));
        assert_eq!(list(&mut s, &json!({})).unwrap()[0]["canRetry"], true);
        c.release.store(true, Ordering::SeqCst);
        // A different current selection must not change the frozen retry target.
        s.state.project_selection.clear();
        s.execute("transcript.retry", json!({"job":r["job"]})).unwrap();
        finish(&mut s);
        assert_eq!(s.project.transcripts[&item].language, "ko");
        assert!(lock(&c.received).iter().all(|opts| opts.preserve_fillers && opts.language.as_deref() == Some("ko")));
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(*s.project, *before);
        s.execute("edit.redo", json!({})).unwrap();
        assert_eq!(s.project.transcripts[&item].words.len(), 1);
    }
    #[test]
    fn cancel_after_worker_finishes_before_commit_preserves_project() {
        let (mut s, item, c) = setup(false);
        c.release.store(true, Ordering::SeqCst);
        let before = s.project.clone();
        let r = s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
        until(|| lock(&s.transcript_jobs[0].output).is_some());
        s.execute("jobs.cancel", json!({"job":r["job"]})).unwrap();
        finish(&mut s);
        assert!(Arc::ptr_eq(&s.project, &before));
    }
    #[test]
    fn failure_retries_and_transcript_edit_is_never_overwritten() {
        let (mut s, item, c) = setup(true);
        c.release.store(true, Ordering::SeqCst);
        let r = s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
        finish(&mut s);
        assert!(s.project.transcripts.is_empty());
        c.release.store(false, Ordering::SeqCst);
        c.entered.store(false, Ordering::SeqCst);
        s.execute("transcript.retry", json!({"job":r["job"]})).unwrap();
        until(|| c.entered.load(Ordering::SeqCst));
        s.execute("transcript.set", json!({"item":item.0,"transcript":{"language":"manual"}})).unwrap();
        c.release.store(true, Ordering::SeqCst);
        finish(&mut s);
        assert_eq!(s.project.transcripts[&item].language, "manual");
        assert!(s.jobs.last().unwrap().to_json()["result"]["error"].as_str().unwrap().contains("changed"));
    }
    #[test]
    fn new_document_cancels_worker_even_with_reused_item_ids() {
        let (mut s, item, c) = setup(false);
        s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
        until(|| c.entered.load(Ordering::SeqCst));
        s.execute("file.openDemoProject", json!({})).unwrap();
        let before = s.project.clone();
        c.release.store(true, Ordering::SeqCst);
        finish(&mut s);
        assert!(Arc::ptr_eq(&s.project, &before));
        assert_eq!(list(&mut s, &json!({})).unwrap()[0]["canRetry"], false);
    }
}
