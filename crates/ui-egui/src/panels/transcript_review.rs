//! Reviewed transcript cleanup: no candidate is selected automatically.
use crate::FilmcraftApp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewUi {
    pub model: String,
    pub preserve_fillers: bool,
    pub min_seconds: f64,
    pub keep_seconds: f64,
    pub fillers: String,
    #[serde(skip)]
    pub open: bool,
    #[serde(skip)]
    pub preview: Option<Value>,
    #[serde(skip)]
    pub selected: Vec<usize>,
    #[serde(skip)]
    pub error: String,
}
impl Default for ReviewUi {
    fn default() -> Self {
        Self {
            model: "whisper-small".into(),
            preserve_fillers: false,
            min_seconds: 1.0,
            keep_seconds: 0.15,
            fillers: "음, 어, 으음, 어어".into(),
            open: false,
            preview: None,
            selected: Vec::new(),
            error: String::new(),
        }
    }
}
impl ReviewUi {
    fn params(&self) -> Value {
        let fillers: Vec<&str> = self.fillers.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        json!({"minSeconds": self.min_seconds, "keepSeconds": self.keep_seconds, "fillers": fillers})
    }
}

pub fn show(app: &mut FilmcraftApp, ui: &egui::Ui) {
    if !app.ui.transcript_review.open {
        return;
    }
    let mut open = true;
    let mut open_tts = false;
    let mut action: Option<(&str, Value)> = None;
    egui::Window::new(tl!("Review pauses and fillers")).id(egui::Id::new("transcript-review")).open(&mut open).default_width(580.0).resizable(true).show(
        ui.ctx(),
        |ui| {
            ui.label(tl!("Candidates are suggestions, not verified silence. Listen before selecting. Nothing is selected automatically."));
            let r = ui.button(tl!("Open Text to Speech"));
            app.auto.add("text.review.tts", r.rect, "Open Text to Speech");
            if r.clicked() {
                open_tts = true;
                if filmcraft_tts::LANGUAGES.iter().any(|(id, _)| *id == "ko-KR") {
                    app.ui.tts.language = "ko-KR".into();
                    app.ui.tts.voice = "macos-yuna".into();
                }
                app.show_panel(crate::dock::PanelKind::TextToSpeech);
            }
            ui.horizontal(|ui| {
                ui.label(tl!("Korean speech model"));
                let r = egui::ComboBox::from_id_salt("review-model").selected_text(&app.ui.transcript_review.model).show_ui(ui, |ui| {
                    for model in ["whisper-base", "whisper-small"] {
                        let r = ui.selectable_value(&mut app.ui.transcript_review.model, model.into(), model);
                        app.auto.add(&format!("text.review.model.{model}"), r.rect, model);
                    }
                });
                app.auto.add("text.review.model", r.response.rect, "Korean speech model");
                let r = ui.add_enabled(app.session.is_enabled("transcript.downloadModel"), egui::Button::new(tl!("Download model")));
                app.auto.add("text.review.download", r.rect, "Download model");
                if r.clicked() {
                    action = Some(("transcript.downloadModel", json!({"model": app.ui.transcript_review.model})));
                }
            });
            ui.label(tl!("Local multilingual models: base about 293 MB, small about 970 MB. Transcription runs on CPU and may take time."));
            let r = ui.checkbox(&mut app.ui.transcript_review.preserve_fillers, tl!("Include spoken fillers (experimental)"));
            app.auto.add("text.review.preserveFillers", r.rect, "Include spoken fillers (experimental)");
            if r.hovered() {
                r.on_hover_text("Context can recover omitted fillers or add unsupported words. Review candidates before cutting.");
            }
            let r = ui.add_enabled(super::text::filmcraft_speech_available(app), egui::Button::new(tl!("Transcribe Korean")));
            app.auto.add("text.review.transcribe", r.rect, "Transcribe Korean");
            if r.clicked() {
                action = Some((
                    "transcript.generate",
                    json!({"language": "ko", "model": app.ui.transcript_review.model, "preserveFillers":app.ui.transcript_review.preserve_fillers}),
                ));
            }
            jobs(app, ui);
            ui.separator();
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label(tl!("Minimum pause (s)"));
                let r = ui.add(egui::DragValue::new(&mut app.ui.transcript_review.min_seconds).range(0.1..=30.0).speed(0.1));
                changed |= r.changed();
                app.auto.add("text.review.minSeconds", r.rect, "Minimum pause");
                ui.label(tl!("Keep on each side (s)"));
                let r = ui.add(egui::DragValue::new(&mut app.ui.transcript_review.keep_seconds).range(0.0..=2.0).speed(0.05));
                changed |= r.changed();
                app.auto.add("text.review.keepSeconds", r.rect, "Keep on each side");
            });
            ui.label(tl!("Filler phrases (comma separated)"));
            let r = ui.text_edit_singleline(&mut app.ui.transcript_review.fillers);
            changed |= r.changed();
            app.auto.add("text.review.fillers", r.rect, "Filler phrases");
            if changed {
                app.ui.transcript_review.preview = None;
                app.ui.transcript_review.selected.clear();
            }
            let r = ui.add_enabled(!super::text::cached_words(app).is_empty(), egui::Button::new(tl!("Find candidates")));
            app.auto.add("text.review.find", r.rect, "Find candidates");
            if r.clicked() {
                action = Some(("transcript.previewCleanup", app.ui.transcript_review.params()));
            }
            if let Some(preview) = app.ui.transcript_review.preview.clone() {
                let candidates = preview["candidates"].as_array().cloned().unwrap_or_default();
                ui.label(tlf!("{count} candidates", count = candidates.len()));
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for c in &candidates {
                        let Some(id) = c["id"].as_u64().and_then(|n| usize::try_from(n).ok()) else { continue };
                        let start = c["start"].as_i64().unwrap_or_default();
                        let end = c["end"].as_i64().unwrap_or_default();
                        let kind = if c["kind"] == "pause" {
                            tl!("Pause")
                        } else if c["actionable"].as_bool() == Some(false) {
                            tl!("Unconfirmed filler")
                        } else {
                            tl!("Filler candidate")
                        };
                        let text = c["text"].as_str().unwrap_or_default();
                        ui.horizontal(|ui| {
                            let mut selected = app.ui.transcript_review.selected.contains(&id);
                            let label = format!(
                                "{kind}: {text}  {:.2}–{:.2} s",
                                start as f64 / filmcraft_time::TICKS_PER_SECOND as f64,
                                end as f64 / filmcraft_time::TICKS_PER_SECOND as f64
                            );
                            let r = ui.add_enabled(c["actionable"].as_bool() != Some(false), egui::Checkbox::new(&mut selected, label));
                            app.auto.add(&format!("text.review.candidate.{id}"), r.rect, "Select candidate");
                            if r.changed() {
                                app.ui.transcript_review.selected.retain(|n| *n != id);
                                if selected {
                                    app.ui.transcript_review.selected.push(id);
                                }
                            }
                            let r = ui.button(tl!("Listen"));
                            app.auto.add(&format!("text.review.listen.{id}"), r.rect, "Listen to candidate");
                            if r.clicked() {
                                let context = filmcraft_time::TICKS_PER_SECOND / 4;
                                let duration = app.session.active_sequence().map(|q| q.duration().0).unwrap_or(end);
                                action = Some((
                                    "playback.previewRange",
                                    json!({"start": start.saturating_sub(context).max(0), "end": end.saturating_add(context).min(duration)}),
                                ));
                            }
                        });
                        if let Some(reason) = c["reason"].as_str() {
                            ui.add(egui::Label::new(reason).wrap());
                        }
                        if let Some(context) = c["context"].as_str() {
                            ui.add(egui::Label::new(context).wrap());
                            if let Some(score) = c["confidence"].as_f64() {
                                ui.add(
                                    egui::Label::new(format!(
                                        "Reported score: {:.0}% (not verified; a matching word may be a meaningful response)",
                                        score * 100.0
                                    ))
                                    .wrap(),
                                );
                            }
                        }
                    }
                });
                let r = ui.add_enabled(!app.ui.transcript_review.selected.is_empty(), egui::Button::new(tl!("Apply selected (one undo step)")));
                app.auto.add("text.review.apply", r.rect, "Apply selected candidates");
                if r.clicked() {
                    action = Some((
                        "transcript.applyCleanup",
                        json!({"token": preview["token"], "params": preview["params"], "selected": app.ui.transcript_review.selected}),
                    ));
                }
            }
            if !app.ui.transcript_review.error.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &app.ui.transcript_review.error);
            }
            ui.label(tl!("Undo restores all selected cuts together. A changed timeline requires a fresh preview."));
            let r = ui.add_enabled(app.session.history.can_undo(), egui::Button::new(tl!("Undo")));
            app.auto.add("text.review.undo", r.rect, "Undo cleanup");
            if r.clicked() {
                action = Some(("edit.undo", json!({})));
            }
        },
    );
    app.ui.transcript_review.open = open && !open_tts;
    if let Some((cmd, params)) = action {
        app.ui.transcript_review.error.clear();
        if cmd == "transcript.applyCleanup" {
            app.stop();
        }
        match crate::menus::invoke(app, ui.ctx(), cmd, params) {
            Ok(result) if cmd == "transcript.previewCleanup" => {
                app.ui.transcript_review.preview = Some(result);
                app.ui.transcript_review.selected.clear();
            }
            Ok(_) if matches!(cmd, "transcript.applyCleanup" | "transcript.generate" | "edit.undo") => {
                app.ui.transcript_review.preview = None;
                app.ui.transcript_review.selected.clear();
                app.ui.transcript_sel = None;
            }
            Ok(_) => {}
            Err(e) => {
                app.ui.transcript_review.error = e.clone();
                app.ui.status = e;
                if cmd == "transcript.applyCleanup" {
                    app.ui.transcript_review.preview = None;
                    app.ui.transcript_review.selected.clear();
                }
            }
        }
    }
}

/// Shared by the transcript tab and review dialog; workers never borrow the UI session.
pub(super) fn jobs(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let Ok(jobs) = app.session.execute("transcript.jobs", json!({})) else { return };
    let Some(jobs) = jobs.as_array() else { return };
    let Some(job) = jobs.last() else { return };
    let Some(id) = job["id"].as_u64() else { return };
    let finished = job["finished"].as_bool().unwrap_or(false);
    ui.horizontal(|ui| {
        let stopping = !finished && job["cancelRequested"] == true;
        let status = if stopping { "Stopping transcription; nothing will be changed" } else { job["status"].as_str().unwrap_or_default() };
        let r = ui.add(
            egui::ProgressBar::new(job["progress"].as_f64().unwrap_or_default() as f32)
                .desired_width((ui.available_width() - 100.0).clamp(80.0, 480.0))
                .show_percentage()
                .text(status),
        );
        app.auto.add("text.transcription.progress", r.rect, status);
        if !finished {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            let r = ui.add_enabled(!stopping, egui::Button::new(tl!("Cancel")));
            app.auto.add("text.transcription.cancel", r.rect, "Cancel transcription");
            if r.clicked() {
                let _ = crate::menus::invoke(app, ui.ctx(), "jobs.cancel", json!({"job":id}));
            }
        } else if job["canRetry"] == true {
            let r = ui.button(tl!("Retry"));
            app.auto.add("text.transcription.retry", r.rect, "Retry transcription");
            if r.clicked() {
                let _ = crate::menus::invoke(app, ui.ctx(), "transcript.retry", json!({"job":id}));
            }
        }
    });
}
