use super::{ControlRequest, FilmcraftApp, PendingUiRequest, capture, json};
use crate::control;

impl FilmcraftApp {
    // ---------------------------------------------------------------- control channel

    /// Make the UI pass run for a control request without stealing the user's keyboard focus.
    pub(crate) fn raise_for_control(&mut self, ctx: &egui::Context) {
        // Ordering alone cannot restore a hidden/minimized window. Limit native ordering:
        // hidden logic may tick hundreds of times while AppKit waits on the compositor.
        let now = self.control_time.max(self.now());
        if self.last_control_raise.is_some_and(|last| now - last < 0.25) {
            ctx.request_repaint();
            return;
        }
        self.last_control_raise = Some(now);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        if let Some(raise) = self.hooks.raise_without_focus.as_mut() {
            raise();
        }
        ctx.request_repaint();
    }

    /// Drain queued control requests without blocking: UI requests wait for a layout pass with a 3 s deadline (except screenshots), non-UI requests run now,
    /// and hidden windows are raised and repainted.
    pub(super) fn drain_control(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.control_rx.take() else { return };
        let now = self.control_time.max(self.now());
        let mut reqs: Vec<(ControlRequest, f64)> = std::mem::take(&mut self.deferred);
        while let Ok(req) = rx.try_recv() {
            // Only widget input needs a visible UI pass. Queries and state patches must
            // not synchronously order an AppKit window while background jobs are running.
            if matches!(req.method.as_str(), "ui.click" | "ui.move" | "ui.drag" | "ui.scroll" | "ui.key" | "ui.type") && now - self.last_ui_time > 0.25 {
                self.raise_for_control(ctx);
                // A hidden native window still needs a real UI pass for widget state/geometry.
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            }
            reqs.push((req, now + 3.0));
        }
        for (req, deadline) in reqs {
            if self.styled && req.method.starts_with("ui.") && req.method != "ui.screenshot" {
                self.ui_requests.push(PendingUiRequest { request: req, deadline, retry_error: None });
                ctx.request_repaint();
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                continue;
            }
            let reply = req.reply.clone();
            match control::handle(self, ctx, &req) {
                control::Outcome::Done(v) => {
                    let _ = reply.send(v);
                }
                control::Outcome::Retry(msg) => {
                    if now < deadline {
                        self.deferred.push((req, deadline));
                        ctx.request_repaint();
                    } else {
                        let _ = reply.send(json!({"ok": false, "error": msg}));
                    }
                }
                control::Outcome::AfterInput => {
                    if self.input_waiters.is_empty() {
                        self.control_input_started = false;
                    }
                    self.input_waiters.push((reply, now + 3.0));
                }
                control::Outcome::Screenshot { path, crop } => {
                    capture::admit(self, ctx, path, crop, reply);
                }
            }
        }
        self.control_rx = Some(rx);
        self.ui_requests.retain(|pending| {
            if now > pending.deadline {
                let error = pending.retry_error.as_deref().unwrap_or("no UI pass ran before the control request deadline");
                let _ = pending.request.reply.send(json!({"ok": false, "error": error}));
                false
            } else {
                true
            }
        });
    }

    /// A hidden viewport may never deliver a queued gesture. Cancel only an unstarted batch:
    /// injecting a release to abort an already-started drag could commit a partial edit.
    pub(super) fn expire_undelivered_control_input(&mut self) {
        let now = self.control_time.max(self.now());
        if !self.control_input_started && self.input_waiters.iter().any(|(_, deadline)| now >= *deadline) {
            self.synthetic.clear();
            self.synthetic_pending_step = None;
            self.synthetic_step_offered = false;
            self.synthetic_modifiers_offered = false;
            self.synthetic_modifiers = egui::Modifiers::NONE;
            for (reply, _) in self.input_waiters.drain(..) {
                let _ = reply.send(json!({"ok": false, "error": "UI input was not delivered: window hidden or display unavailable; queued input cancelled"}));
            }
        }
    }

    /// Process control requests that need the current layout/response. Called at the end of a
    /// final, non-discarded UI pass; `ui.click`/`ui.drag` queue synthetic input for the next pass.
    pub(super) fn drain_ui_requests(&mut self, ctx: &egui::Context) {
        let reqs = std::mem::take(&mut self.ui_requests);
        let now = self.control_time.max(self.now());
        let mut pending = reqs.into_iter();
        while let Some(PendingUiRequest { request: req, deadline, .. }) = pending.next() {
            // A panel/dock/resize request changes the next layout. Anything after it in this
            // batch (ui.click, ui.elements, ui.screenshot, …) must see the new geometry, so it
            // waits for the next accepted pass rather than reading the layout drawn before it.
            let changes_layout = matches!(req.method.as_str(), "ui.set" | "ui.panel.show" | "ui.panel.close" | "ui.resize");
            let reply = req.reply.clone();
            match control::handle(self, ctx, &req) {
                control::Outcome::Done(v) => {
                    let _ = reply.send(v);
                }
                control::Outcome::Retry(msg) => {
                    if now < deadline {
                        self.ui_requests.push(PendingUiRequest { request: req, deadline, retry_error: Some(msg) });
                        ctx.request_repaint();
                    } else {
                        let _ = reply.send(json!({"ok": false, "error": msg}));
                    }
                }
                control::Outcome::AfterInput => {
                    if self.input_waiters.is_empty() {
                        self.control_input_started = false;
                    }
                    self.input_waiters.push((reply, now + 3.0));
                }
                control::Outcome::Screenshot { path, crop } => capture::admit(self, ctx, path, crop, reply),
            }
            if changes_layout {
                let rest: Vec<_> = pending.collect();
                if !rest.is_empty() {
                    self.ui_requests.extend(rest);
                    ctx.request_repaint();
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                }
                break;
            }
        }
    }
}
