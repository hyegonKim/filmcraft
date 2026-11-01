use super::{FilmcraftApp, menus};
#[cfg(test)]
use crate::{ControlRequest, PendingUiRequest, control};
#[cfg(test)]
use filmcraft_engine::Session;
#[cfg(test)]
use serde_json::json;

impl FilmcraftApp {
    // ---------------------------------------------------------------- input

    /// Sends the menu tree to the native menu bar when it changed, so its items show the same
    /// labels, shortcuts and enablement as the in-window menus (Undo with nothing to undo is
    /// disabled). The items a focused text field takes stay enabled while one has the keyboard.
    /// Without a native menu bar this does nothing.
    pub(super) fn sync_native_menu(&mut self, ctx: &egui::Context) {
        if self.hooks.menu_changed.is_none() {
            return;
        }
        let mut items = menus::menu_items(self);
        if menus::text_field_window(ctx).is_some() {
            for it in items.iter_mut().filter(|it| menus::edits_text(&it.id)) {
                it.enabled = true;
            }
        } else if self.ui.gfx_edit.is_some() {
            // a graphic's text: its Copy, Cut, Paste and Select All, whatever the project allows (#1953)
            for it in items.iter_mut().filter(|it| menus::edits_graphic_text(&it.id)) {
                it.enabled = true;
            }
        }
        if items == self.menu_sent {
            return;
        }
        if let Some(hook) = self.hooks.menu_changed.as_mut() {
            hook(&items);
        }
        self.menu_sent = items;
    }
}

impl FilmcraftApp {
    /// Drain native menu/control command ids, preserving the control-only Quit exception.
    pub(super) fn logic_command_inbox(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.command_inbox.take() {
            while let Ok(id) = rx.try_recv() {
                // control-only mode ignores native menu commands, but Quit stays available (#1943)
                if self.control_input_only && id != "app.quit" {
                    continue;
                }
                if let Err(e) = menus::activate_native(self, ctx, &id) {
                    self.ui.status = e;
                }
            }
            self.command_inbox = Some(rx);
        }
    }
}

#[cfg(test)]
mod pending_ui_request_tests {
    use super::*;

    #[test]
    fn expired_widget_retry_reports_the_missing_element_after_ui_passes() {
        let ctx = egui::Context::default();
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
        app.styled = true;
        let (request, reply) = ControlRequest::new("ui.click", json!({"id": "qa.missing.widget"}));
        app.ui_requests.push(PendingUiRequest { request, deadline: 10.0, retry_error: None });
        app.control_time = 1.0;
        app.drain_ui_requests(&ctx);
        assert!(reply.try_recv().is_err(), "a missing widget can still appear before its deadline");
        app.control_time = 11.0;
        app.drain_control(&ctx);
        let response = reply.try_recv().expect("the retry must expire");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("qa.missing.widget"), "{response}");
        assert!(!response["error"].as_str().unwrap().contains("no UI pass"), "{response}");
        assert!(app.ui_requests.is_empty());
        assert!(app.synthetic.is_empty(), "a missing widget must never queue input");
    }

    #[test]
    fn request_without_a_ui_pass_keeps_the_ui_pass_timeout() {
        let ctx = egui::Context::default();
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
        app.styled = true;
        let (request, reply) = ControlRequest::new("ui.elements", json!({}));
        app.ui_requests.push(PendingUiRequest { request, deadline: 10.0, retry_error: None });
        app.control_time = 11.0;
        app.drain_control(&ctx);
        let response = reply.try_recv().expect("a request without UI passes must expire");
        assert_eq!(response["error"], "no UI pass ran before the control request deadline");
        assert!(app.ui_requests.is_empty());
    }
}

#[cfg(test)]
mod hidden_control_input_tests {
    use super::*;
    #[test]
    fn a_started_control_gesture_is_not_discarded_during_occlusion() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control_input_only(rx);
        let (request, reply) = ControlRequest::new("ui.click", json!({"x":10.0,"y":10.0}));
        tx.send(request).unwrap();
        app.control_time = 10.0;
        app.drain_control(&ctx);
        let mut visible = egui::RawInput { time: Some(10.1), ..Default::default() };
        visible.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(false);
        eframe::App::raw_input_hook(&mut app, &ctx, &mut visible);
        assert!(app.control_input_started);
        let queued = app.synthetic.len();
        assert!(queued > 0);
        let mut hidden = egui::RawInput { time: Some(14.0), ..Default::default() };
        hidden.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(true);
        eframe::App::raw_input_hook(&mut app, &ctx, &mut hidden);
        assert_eq!(app.synthetic.len(), queued, "do not synthesize release or discard a partially delivered gesture");
        assert!(reply.try_recv().is_err());
    }

    #[test]
    fn an_undelivered_hidden_click_expires_without_late_input_or_project_edits() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control_input_only(rx);
        let before = serde_json::to_value(&app.session.project).unwrap();
        let (request, reply) = ControlRequest::new("ui.click", json!({"x":10.0,"y":10.0}));
        tx.send(request).unwrap();
        for seconds in [10.0, 14.0] {
            let mut raw = egui::RawInput { time: Some(seconds), ..Default::default() };
            raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(true);
            eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
            let _ = ctx.run_logic(&raw, |ctx| app.drain_control(ctx));
        }
        let response = reply.try_recv().expect("input acknowledgements need a deadline even without UI passes");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("input"));
        assert!(app.synthetic.is_empty(), "timed-out input must not click later");
        assert!(app.input_waiters.is_empty());
        let mut resumed = egui::RawInput { time: Some(15.0), ..Default::default() };
        resumed.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(false);
        eframe::App::raw_input_hook(&mut app, &ctx, &mut resumed);
        assert!(!resumed.events.iter().any(|e| matches!(e, egui::Event::PointerButton { .. } | egui::Event::Key { .. })));
        assert_eq!(serde_json::to_value(&app.session.project).unwrap(), before);
    }

    #[test]
    fn hidden_state_queries_and_patches_do_not_reorder_the_native_window() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
        let raises = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = raises.clone();
        app.hooks.raise_without_focus = Some(Box::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let (inspect, inspect_reply) = ControlRequest::new("ui.inspect", json!({}));
        let (patch, patch_reply) = ControlRequest::new("ui.set", json!({"timeline":{"scroll":5.0}}));
        tx.send(inspect).unwrap();
        tx.send(patch).unwrap();
        let mut raw = egui::RawInput { time: Some(10.0), ..Default::default() };
        raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(true);
        eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
        let _ = ctx.run_logic(&raw, |ctx| app.drain_control(ctx));
        assert_eq!(inspect_reply.try_recv().unwrap()["ok"], true);
        assert_eq!(patch_reply.try_recv().unwrap()["ok"], true);
        assert_eq!(app.ui.timeline.target_scroll, 5.0);
        assert_eq!(raises.load(std::sync::atomic::Ordering::SeqCst), 0, "data requests must not synchronously reorder a hidden macOS window");
    }

    #[test]
    fn control_raise_restores_visibility_without_focus_and_is_throttled() {
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(Session::default());
        app.control_time = 10.0;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.raise_for_control(ui.ctx());
            app.raise_for_control(ui.ctx());
        });
        output.textures_delta.clear();
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert_eq!(commands.iter().filter(|c| matches!(c, egui::ViewportCommand::Visible(true))).count(), 1);
        assert_eq!(commands.iter().filter(|c| matches!(c, egui::ViewportCommand::Minimized(false))).count(), 1);
        assert!(!commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)));
    }
    #[test]
    fn a_hidden_logic_pass_does_not_consume_a_synthetic_click() {
        let ctx = egui::Context::default();
        let mut app = FilmcraftApp::new(Session::default());
        app.control_input_only = true;
        let (request, _) = ControlRequest::new("ui.click", json!({"x":10.0,"y":10.0}));
        assert!(matches!(control::handle(&mut app, &ctx, &request), control::Outcome::AfterInput));
        let queued = app.synthetic.len();
        assert!(queued > 0);
        for _ in 0..10 {
            let mut raw = egui::RawInput::default();
            raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(true);
            eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
            assert_eq!(app.synthetic.len(), queued, "press/release must wait for a visible UI pass");
        }
        let mut edges = Vec::new();
        for _ in 0..10 {
            let mut raw = egui::RawInput::default();
            raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(false);
            eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
            let buttons: Vec<_> = raw
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PointerButton { pressed, .. } => Some(*pressed),
                    _ => None,
                })
                .collect();
            assert!(buttons.len() <= 1, "the click edges must be on different visible frames");
            edges.extend(buttons);
        }
        assert!(app.synthetic.is_empty());
        assert_eq!(edges, [true, false]);
    }
}

#[cfg(test)]
mod hidden_control_clock_tests {
    use super::*;
    #[test]
    fn missing_widget_retry_expires_even_when_egui_time_is_frozen() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
        let (request, reply) = ControlRequest::new("ui.click", json!({"id":"not-yet-rendered"}));
        tx.send(request).unwrap();
        for seconds in [10.0, 14.0] {
            let mut raw = egui::RawInput { time: Some(seconds), ..Default::default() };
            raw.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().occluded = Some(true);
            eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
            app.drain_control(&ctx);
        }
        let response = reply.try_recv().expect("a hidden window must not keep a retry alive forever");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("not-yet-rendered"));
        assert!(app.deferred.is_empty());
    }
}

#[cfg(test)]
mod native_menu_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn enabled(items: &[menus::MenuItem], id: &str) -> Option<bool> {
        items.iter().find(|it| it.id == id).map(|it| it.enabled)
    }

    /// The native menu bar (macOS) gets the menu tree whenever it changes, so Edit ▸ Undo and Redo
    /// follow the history instead of staying enabled (#902).
    #[test]
    fn native_menu_follows_undo_and_redo_availability() {
        let sent: Rc<RefCell<Vec<Vec<menus::MenuItem>>>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        let sink = sent.clone();
        app.hooks.menu_changed = Some(Box::new(move |items: &[menus::MenuItem]| sink.borrow_mut().push(items.to_vec())));
        let ctx = egui::Context::default();

        app.sync_native_menu(&ctx);
        let last = |sent: &Rc<RefCell<Vec<Vec<menus::MenuItem>>>>, id: &str| sent.borrow().last().and_then(|items| enabled(items, id));
        assert_eq!(last(&sent, "edit.undo"), Some(false), "nothing to undo in an empty project");
        assert_eq!(last(&sent, "edit.redo"), Some(false), "nothing to redo in an empty project");
        assert_eq!(last(&sent, "file.newProject"), Some(true));

        app.sync_native_menu(&ctx);
        assert_eq!(sent.borrow().len(), 1, "an unchanged menu is not sent again");

        app.session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.undo"), Some(true), "an edit can be undone");
        assert_eq!(last(&sent, "edit.redo"), Some(false));

        app.session.execute("edit.undo", json!({})).unwrap();
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.undo"), Some(false));
        assert_eq!(last(&sent, "edit.redo"), Some(true), "the undone edit can be redone");
    }

    /// #1953: AppKit does not send a disabled item's command, so while a graphic's text is edited
    /// its Copy, Cut, Paste and Select All stay enabled with no clip selected or copied; Undo stays
    /// the project's.
    #[test]
    fn native_edit_items_stay_enabled_while_a_graphic_text_is_edited() {
        let sent: Rc<RefCell<Vec<Vec<menus::MenuItem>>>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        let sink = sent.clone();
        app.hooks.menu_changed = Some(Box::new(move |items: &[menus::MenuItem]| sink.borrow_mut().push(items.to_vec())));
        let ctx = egui::Context::default();
        let last = |sent: &Rc<RefCell<Vec<Vec<menus::MenuItem>>>>, id: &str| sent.borrow().last().and_then(|items| enabled(items, id));
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.copy"), Some(false), "nothing selected to copy");
        app.ui.gfx_edit = Some(crate::state::GfxEdit::default());
        app.sync_native_menu(&ctx);
        for id in ["edit.copy", "edit.cut", "edit.paste", "edit.selectAll"] {
            assert_eq!(last(&sent, id), Some(true), "{id} while a graphic's text is edited");
        }
        assert_eq!(last(&sent, "edit.undo"), Some(false), "Undo stays the project's");
    }
}
