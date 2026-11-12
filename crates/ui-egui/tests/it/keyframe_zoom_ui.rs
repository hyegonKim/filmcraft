//! #2202: the Effect Controls keyframe lane zooms. Its footer has a zoom scroll bar between − and +
//! buttons and a button back to the whole clip; ⌘ / Ctrl + wheel and a pinch zoom about the
//! pointer, a sideways swipe (and the wheel over the footer) scrolls. The ruler, the playhead,
//! keyframes, their drags and the marquee all follow the zoomed view. Driven headless through the
//! control channel, as a user would: clicks, drags, wheel, keys.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::ClipId;
use filmcraft_time::Tick;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

#[derive(Clone, Copy)]
struct Clip {
    id: u64,
    start: i64,
    duration: i64,
}

impl Clip {
    /// The timeline time at the fraction `f` of the clip, in seconds.
    fn seconds_at(&self, f: f64) -> f64 {
        Tick(self.start + (f * self.duration as f64) as i64).seconds()
    }
}

impl Driver {
    /// The demo project with its first video clip selected and Effect Controls showing.
    fn demo() -> (Self, Clip) {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_step_dt(1.0 / 60.0).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        let clip = d.clip(0);
        d.exec("timeline.select", json!({"clips": [clip.id]}));
        d.ok("ui.panel.show", json!({"panel": "EffectControls"}));
        d.frames(3);
        (d, clip)
    }
    /// The `i`th clip of the first video track.
    fn clip(&mut self, i: usize) -> Clip {
        let seq = self.exec("sequence.inspect", json!({}));
        let c = &seq["video"][0]["items"][i];
        Clip { id: c["clip"].as_u64().unwrap(), start: c["start"].as_i64().unwrap(), duration: c["duration"].as_i64().unwrap() }
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
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params.clone());
        assert_eq!(r["ok"], true, "{method} {params}: {r}");
        r["result"].clone()
    }
    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }
    /// The element's rect `[x, y, w, h]`, or `None` when it is not on screen.
    fn find(&mut self, id: &str) -> Option<[f64; 4]> {
        self.frames(2);
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == id)?;
        let r: Vec<f64> = e["rect"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        Some([r[0], r[1], r[2], r[3]])
    }
    fn rect(&mut self, id: &str) -> [f64; 4] {
        self.find(id).unwrap_or_else(|| panic!("no element {id}"))
    }
    fn click(&mut self, id: &str) {
        self.rect(id);
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }
    fn drag(&mut self, from: (f64, f64), to: (f64, f64)) {
        self.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": to.0, "y": to.1}, "steps": 8}));
        self.frames(3);
    }
    fn scroll(&mut self, at: (f64, f64), dx: f64, dy: f64, command: bool) {
        self.ok("ui.scroll", json!({"x": at.0, "y": at.1, "dx": dx, "dy": dy, "modifiers": {"command": command}}));
        // egui spreads a wheel step over a few frames
        self.frames(20);
    }
    fn playhead(&mut self) -> i64 {
        self.exec("sequence.inspect", json!({}))["playhead"].as_i64().unwrap()
    }
    fn seek(&mut self, seconds: f64) -> i64 {
        self.exec("playhead.set", json!({"seconds": seconds}));
        self.frames(2);
        self.playhead()
    }
    /// The zoom saved for `clip`, as `ui.inspect` reports it; `None` while it shows whole.
    fn zoom_of(&mut self, clip: &Clip) -> Option<(f64, f64)> {
        let zooms = self.ok("ui.inspect", json!({}))["ui"]["effect_controls_zoom"].clone();
        let z = zooms.as_array()?.iter().find(|z| z["clip"] == clip.id)?.clone();
        Some((z["range"][0].as_f64().unwrap(), z["range"][1].as_f64().unwrap()))
    }
    /// The stretch of `clip` the lane shows, as (start, end) fractions of it.
    fn range(&mut self, clip: &Clip) -> (f64, f64) {
        self.zoom_of(clip).unwrap_or((0.0, 1.0))
    }
    /// Add a Motion ▸ Scale keyframe at the fraction `f` of the clip; returns its media time.
    fn scale_key(&mut self, clip: &Clip, f: f64) -> i64 {
        let at = self.seek(clip.seconds_at(f));
        self.exec("effects.addKeyframe", json!({"clip": clip.id, "effect": "motion", "param": "scale"}));
        let it = self.harness.state().session.active_sequence().unwrap().find_item(ClipId(clip.id)).unwrap().1.clone();
        it.source_time_at(Tick(at)).0
    }
    fn scale_keys(&self, clip: &Clip) -> Vec<i64> {
        let s = &self.harness.state().session;
        let it = s.active_sequence().unwrap().find_item(ClipId(clip.id)).unwrap().1;
        it.effect("motion").unwrap().param("scale").unwrap().keyframes.iter().map(|k| k.time.0).collect()
    }
}

fn center(r: [f64; 4]) -> (f64, f64) {
    (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
}

fn close(a: (f64, f64), b: (f64, f64), tolerance: f64) -> bool {
    (a.0 - b.0).abs() <= tolerance && (a.1 - b.1).abs() <= tolerance
}

/// The buttons zoom about the playhead, the ruler and the playhead follow, the zoom bar's thumb
/// shows the stretch and scrolls it (dragged, or the wheel over it), its handles zoom, and the fit
/// button (or a double-click on the bar, or \ with the panel focused) shows the whole clip again.
#[test]
fn zoom_scroll_and_fit_with_the_buttons_and_the_zoom_bar() {
    let (mut d, clip) = Driver::demo();
    let frame = Tick::from_seconds_f64(1.0 / 23.976).0;
    let lane = d.rect("effectControls.lane");
    let track = d.rect("effectControls.zoomBar.track");
    assert!(track[1] > lane[1] + lane[3], "the zoom bar is under the lane: {track:?} vs {lane:?}");
    assert!(track[0] >= lane[0] && track[0] + track[2] <= lane[0] + lane[2], "and within its width");
    assert_eq!(d.rect("effectControls.zoomBar")[2], track[2], "the thumb fills the bar: the whole clip");
    for id in ["effectControls.zoomIn", "effectControls.zoomOut", "effectControls.zoomFit"] {
        d.rect(id);
    }

    // + zooms in about the playhead (in the middle of the clip)
    d.seek(clip.seconds_at(0.5));
    d.click("effectControls.zoomIn");
    let (s, e) = d.range(&clip);
    // (about the playhead, which is on a frame: within a frame of the clip's middle)
    assert!(close((s, e), (0.25, 0.75), 0.01) && (e - s - 0.5).abs() < 1e-9, "{:?}", (s, e));
    let thumb = d.rect("effectControls.zoomBar");
    assert!((thumb[2] - track[2] / 2.0).abs() <= 1.0, "the thumb is half the bar: {thumb:?} vs {track:?}");
    let head = d.rect("effectControls.playhead");
    assert!((center(head).0 - (lane[0] + lane[2] / 2.0)).abs() <= 2.0, "the playhead stays in the middle of the lane: {head:?}");
    d.click("effectControls.zoomIn");
    let (s, e) = d.range(&clip);
    assert!(close((s, e), (0.375, 0.625), 0.01) && (e - s - 0.25).abs() < 1e-9, "{:?}", (s, e));
    d.click("effectControls.zoomOut");
    let (s, e) = d.range(&clip);
    assert!(close((s, e), (0.25, 0.75), 0.01) && (e - s - 0.5).abs() < 1e-9, "− undoes +: {:?}", (s, e));

    // the ruler shows the zoomed stretch: a click at its left quarter is at 3/8 of the clip
    let ruler = d.rect("effectControls.ruler");
    d.ok("ui.click", json!({"x": ruler[0] + ruler[2] * 0.25, "y": ruler[1] + 6.0}));
    d.frames(2);
    let want = clip.start + ((s + 0.25 * (e - s)) * clip.duration as f64) as i64;
    assert!((d.playhead() - want).abs() <= frame, "the ruler follows the zoom: {} vs {want}", d.playhead());

    // the wheel over the zoom bar scrolls: down = later
    let (s0, e0) = d.range(&clip);
    d.scroll(center(track), 0.0, -40.0, false);
    let (s1, e1) = d.range(&clip);
    assert!(s1 > s0 + 0.01 && ((e1 - s1) - (e0 - s0)).abs() < 1e-9, "scrolled later, same zoom: {:?} → {:?}", (s0, e0), (s1, e1));
    // a sideways swipe over the rows scrolls back; the plain wheel there scrolls the rows, not time
    d.scroll((lane[0] + lane[2] / 2.0, lane[1] + lane[3] * 0.6), 40.0, 0.0, false);
    let (s2, _) = d.range(&clip);
    assert!(s2 < s1 - 0.01, "a sideways swipe scrolls earlier: {s1} → {s2}");
    d.scroll((lane[0] + lane[2] / 2.0, lane[1] + lane[3] * 0.6), 0.0, -40.0, false);
    assert_eq!(d.range(&clip).0, s2, "the vertical wheel over the rows leaves the time alone");

    // drag the thumb's middle: scroll, by the bar's scale
    let thumb = d.rect("effectControls.zoomBar");
    let (start, end) = d.range(&clip);
    let span = end - start;
    let (cx, cy) = center(thumb);
    d.drag((cx, cy), (cx + 40.0, cy));
    let moved = d.range(&clip);
    let per_point = (1.0 - span) / (track[2] - thumb[2]);
    assert!((moved.0 - (start + 40.0 * per_point)).abs() < 0.01, "the thumb follows the pointer: {start} → {moved:?}");
    assert!(((moved.1 - moved.0) - span).abs() < 1e-9, "and keeps the zoom");
    // drag its right handle left: zoom in, the left end staying
    let thumb = d.rect("effectControls.zoomBar");
    d.drag((thumb[0] + thumb[2] - 4.0, cy), (thumb[0] + thumb[2] - 44.0, cy));
    let narrower = d.range(&clip);
    assert!((narrower.0 - moved.0).abs() < 1e-9 && narrower.1 < moved.1 - 0.05, "the right handle zooms: {moved:?} → {narrower:?}");

    // back to the whole clip: the button, a double-click on the bar, and \ (View ▸ Zoom to Sequence)
    // with the panel focused
    d.click("effectControls.zoomFit");
    assert_eq!(d.zoom_of(&clip), None, "fit shows the whole clip");
    assert_eq!(d.rect("effectControls.zoomBar")[2], track[2]);
    d.click("effectControls.zoomIn");
    // (egui counts clicks anywhere: right after the + click this would be a triple-click)
    d.frames(40);
    d.ok("ui.click", json!({"id": "effectControls.zoomBar", "count": 2}));
    d.frames(3);
    assert_eq!(d.zoom_of(&clip), None, "a double-click on the bar shows the whole clip");
    d.ok("ui.set", json!({"focused": "EffectControls"}));
    d.ok("ui.key", json!({"key": "="}));
    d.frames(2);
    let (s, e) = d.range(&clip);
    assert!(e - s < 0.6, "= zooms the focused lane in: {:?}", (s, e));
    d.ok("ui.key", json!({"key": "\\"}));
    d.frames(2);
    assert_eq!(d.zoom_of(&clip), None, "\\ shows the whole clip");
    // with the Timeline focused, = zooms the Timeline, not the lane
    d.ok("ui.set", json!({"focused": "Timeline"}));
    let pps = d.ok("ui.inspect", json!({}))["ui"]["timeline"]["target_pps"].as_f64().unwrap();
    d.ok("ui.key", json!({"key": "="}));
    d.frames(2);
    assert_eq!(d.zoom_of(&clip), None);
    assert!(d.ok("ui.inspect", json!({}))["ui"]["timeline"]["target_pps"].as_f64().unwrap() > pps, "the Timeline zoomed");
}

/// ⌘ / Ctrl + wheel and a pinch over the lane zoom about the pointer: the time under it stays.
/// Over the effect list they leave the lane alone.
#[test]
fn command_wheel_and_pinch_zoom_about_the_pointer() {
    let (mut d, clip) = Driver::demo();
    let lane = d.rect("effectControls.lane");
    let at = (lane[0] + lane[2] * 0.75, lane[1] + lane[3] * 0.5);
    d.scroll(at, 0.0, 80.0, true);
    let (s, e) = d.range(&clip);
    assert!(e - s < 0.8, "⌘ + wheel up zooms in: {:?}", (s, e));
    assert!((s + 0.75 * (e - s) - 0.75).abs() < 0.01, "the time under the pointer stays there: {:?}", (s, e));
    d.scroll(at, 0.0, -80.0, true);
    let (s, e) = d.range(&clip);
    assert!(e - s > 0.95, "⌘ + wheel down zooms back out: {:?}", (s, e));

    // a pinch, at a quarter of the lane
    d.click("effectControls.zoomFit");
    let at = (lane[0] + lane[2] * 0.25, lane[1] + lane[3] * 0.5);
    let pos = egui::pos2(at.0 as f32, at.1 as f32);
    d.harness.input_mut().events.push(egui::Event::PointerMoved(pos));
    d.frames(1);
    d.harness.input_mut().events.push(egui::Event::Zoom(2.0));
    d.frames(3);
    let (s, e) = d.range(&clip);
    assert!((e - s - 0.5).abs() < 1e-6, "a pinch zooms by its factor: {:?}", (s, e));
    assert!((s + 0.25 * (e - s) - 0.25).abs() < 1e-6, "about the pointer: {:?}", (s, e));

    // over the effect list ⌘ + wheel does nothing to the lane
    let row = d.rect("effectControls.effect.motion");
    d.scroll(center(row), 0.0, 80.0, true);
    assert_eq!(d.range(&clip), (s, e), "the lane only zooms under the pointer");
}

/// Zoomed in, keyframes are drawn and hit where the zoomed ruler puts them, a drag moves a
/// keyframe by the zoomed scale (finer than at the whole clip), keyframes off the view are not
/// drawn and a marquee across the lane selects only those it shows.
#[test]
fn keyframes_drag_and_select_in_the_zoomed_view() {
    let (mut d, clip) = Driver::demo();
    let frame = Tick::from_seconds_f64(1.0 / 23.976).0 as f64;
    let first = d.scale_key(&clip, 0.4);
    let second = d.scale_key(&clip, 0.8);
    let id = |t: i64| format!("effectControls.motion.scale.keyframe.{t}");
    d.seek(clip.seconds_at(0.4));
    d.click("effectControls.zoomIn");
    d.click("effectControls.zoomIn");
    let (s, e) = d.range(&clip);
    assert!(close((s, e), (0.3, 0.55), 0.01) && (e - s - 0.25).abs() < 1e-9, "{:?}", (s, e));
    let lane = d.rect("effectControls.lane");
    let key = d.rect(&id(first));
    let it = d.harness.state().session.active_sequence().unwrap().find_item(ClipId(clip.id)).unwrap().1.clone();
    let at = (first - it.source_in.0) as f64 / it.speed / clip.duration as f64;
    let want = lane[0] + (at - s) / (e - s) * lane[2];
    assert!((center(key).0 - want).abs() <= 1.5, "the keyframe sits on the zoomed ruler: {key:?}, want x = {want}");
    assert!(d.find(&id(second)).is_none(), "a keyframe off the view is not drawn");

    // a marquee over the whole lane, across the Scale row, selects the keyframe it shows only
    let (kx, ky) = center(key);
    let from = egui::pos2((lane[0] + 2.0) as f32, (ky - 10.0) as f32);
    let to = egui::pos2((lane[0] + lane[2] - 2.0) as f32, (ky + 10.0) as f32);
    d.ok("ui.drag", json!({"from": {"x": from.x, "y": from.y}, "to": {"x": to.x, "y": to.y}, "steps": 8}));
    d.frames(3);
    let selected: Vec<i64> = d.harness.state().ui.keyframe_selection.iter().map(|k| k.time.0).collect();
    assert_eq!(selected, vec![first], "only the keyframe in view is selected");

    // drag it right by a fifth of the lane: a fifth of the zoomed stretch, not of the clip
    let it_speed = d.harness.state().session.active_sequence().unwrap().find_item(ClipId(clip.id)).unwrap().1.speed;
    d.drag((kx, ky), (kx + lane[2] * 0.2, ky));
    let keys = d.scale_keys(&clip);
    let moved = keys.iter().find(|t| **t != second).copied().unwrap();
    let want = 0.2 * (e - s) * clip.duration as f64 * it_speed;
    assert!(((moved - first) as f64 - want).abs() <= frame * 1.01, "moved {} ticks, want {want} (a frame is {frame})", moved - first);
    assert!(keys.contains(&second), "the keyframe off the view stays");
    let key = d.rect(&id(moved));
    assert!((center(key).0 - (kx + lane[2] * 0.2)).abs() <= 3.0, "and it is drawn where it was dropped: {key:?}");
}

/// The zoom belongs to the clip it was set on: another clip shows whole, and `ui.set` sets it for
/// the clip shown (a range narrower than five frames widens about its middle).
#[test]
fn another_clip_shows_whole_and_ui_set_zooms_the_shown_clip() {
    let (mut d, clip) = Driver::demo();
    d.ok("ui.set", json!({"effectControlsZoom": [0.2, 0.4]}));
    assert!(close(d.range(&clip), (0.2, 0.4), 1e-9));
    let track = d.rect("effectControls.zoomBar.track");
    assert!(d.rect("effectControls.zoomBar")[2] < track[2] * 0.5);
    let other = d.clip(1);
    d.exec("timeline.select", json!({"clips": [other.id]}));
    d.frames(3);
    assert_eq!(d.range(&other), (0.0, 1.0), "another clip shows whole");
    assert_eq!(d.rect("effectControls.zoomBar")[2], track[2]);
    d.exec("timeline.select", json!({"clips": [clip.id]}));
    d.frames(3);
    assert!(close(d.range(&clip), (0.2, 0.4), 1e-9), "back on the clip, its zoom is still there");
    // each clip keeps its own zoom
    d.exec("timeline.select", json!({"clips": [other.id]}));
    d.frames(3);
    d.ok("ui.set", json!({"effectControlsZoom": [0.6, 0.9]}));
    d.exec("timeline.select", json!({"clips": [clip.id]}));
    d.frames(3);
    assert!(close(d.range(&clip), (0.2, 0.4), 1e-9), "zooming another clip keeps this one's");
    assert!(d.rect("effectControls.zoomBar")[2] < track[2] * 0.5);
    assert!(close(d.range(&other), (0.6, 0.9), 1e-9));

    d.ok("ui.set", json!({"effectControlsZoom": [0.5, 0.5000001]}));
    let (s, e) = d.range(&clip);
    let five_frames = 5.0 * Tick::from_seconds_f64(1.0 / 23.976).0 as f64 / clip.duration as f64;
    assert!((e - s - five_frames).abs() < 1e-3 && ((s + e) / 2.0 - 0.5).abs() < 1e-6, "{:?}", (s, e));
    assert_eq!(d.call("ui.set", json!({"effectControlsZoom": [0.6, 0.2]}))["ok"], false, "a reversed range is refused");
    d.ok("ui.set", json!({"effectControlsZoom": null}));
    assert_eq!(d.zoom_of(&clip), None);

    // a refused zoom leaves the rest of the request undone
    let tool = d.ok("ui.inspect", json!({}))["ui"]["tool"].clone();
    assert_eq!(d.call("ui.set", json!({"tool": "razor", "effectControlsZoom": [0.6, 0.2]}))["ok"], false);
    assert_eq!(d.ok("ui.inspect", json!({}))["ui"]["tool"], tool, "the tool did not change");

    // in another project, a clip with the same id shows whole
    d.ok("ui.set", json!({"effectControlsZoom": [0.2, 0.4]}));
    d.exec("file.openDemoProject", json!({}));
    d.frames(3);
    d.exec("timeline.select", json!({"clips": [clip.id]}));
    d.frames(3);
    assert_eq!(d.rect("effectControls.zoomBar")[2], track[2], "the zoom stayed with the other project");
}
