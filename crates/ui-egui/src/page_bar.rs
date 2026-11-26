//! DaVinci Resolve-inspired bottom 7-Page Navigation Bar:
//! `Media · Cut · Edit · Fusion · Color · Fairlight · Deliver` plus quick `Plugins` studio toggle.
//!
//! Rendered inline on the right cluster of the bottom status bar so panel bodies keep 100% of their
//! vertical workspace height while keeping 1-click Resolve page switching always accessible.

use egui::{Align2, Rect, Sense, Stroke, pos2, vec2};

use crate::FilmcraftApp;
use crate::dock::PanelKind;
use crate::icons::{self, Icon};
use crate::state::Mode;
use crate::theme::Tokens;

struct PageTab {
    id: &'static str,
    label: &'static str,
    icon: Icon,
    tip: &'static str,
}

const PAGES: [PageTab; 7] = [
    PageTab { id: "media", label: "Media", icon: Icon::Folder, tip: "Media Page — ingest, browse drives, and organize bins" },
    PageTab { id: "cut", label: "Cut", icon: Icon::Razor, tip: "Cut Page — fast assembly with Dual-Timeline full-sequence overview" },
    PageTab { id: "edit", label: "Edit", icon: Icon::Film, tip: "Edit Page — precision multi-track timeline and trim tools" },
    PageTab { id: "fusion", label: "Fusion", icon: Icon::Sparkle, tip: "Fusion / VFX Page — visual effects, compositing, and DCTL / OpenFX plugins" },
    PageTab { id: "color", label: "Color", icon: Icon::Adjust, tip: "Color Page — serial color nodes, scopes, CST, Warper, and PowerGrades" },
    PageTab { id: "fairlight", label: "Fairlight", icon: Icon::Audio, tip: "Fairlight Page — track mixer, Dialogue Leveler, and VST3 audio effects" },
    PageTab { id: "deliver", label: "Deliver", icon: Icon::Export, tip: "Deliver Page — render queue, codecs, and master export" },
];

fn active_page(app: &FilmcraftApp) -> &'static str {
    match app.ui.mode {
        Mode::Import => "media",
        Mode::Export => "deliver",
        Mode::Edit => match app.ui.workspace.as_str() {
            "Assembly" => "cut",
            "Effects" => "fusion",
            "Color" => "color",
            "Audio" => "fairlight",
            _ => "edit",
        },
    }
}

pub fn activate_page(app: &mut FilmcraftApp, id: &str) {
    match id {
        "media" => app.ui.mode = Mode::Import,
        "cut" => {
            app.ui.mode = Mode::Edit;
            app.set_workspace("Assembly");
            app.ui.timeline.cut_overview = true;
        }
        "edit" => {
            app.ui.mode = Mode::Edit;
            app.set_workspace("Editing");
        }
        "fusion" => {
            app.ui.mode = Mode::Edit;
            app.set_workspace("Effects");
            app.show_panel(PanelKind::Plugins);
        }
        "color" => {
            app.ui.mode = Mode::Edit;
            app.set_workspace("Color");
        }
        "fairlight" => {
            app.ui.mode = Mode::Edit;
            app.set_workspace("Audio");
        }
        "deliver" => app.ui.mode = Mode::Export,
        _ => {}
    }
}

/// Draws the 7-Page Navigation Bar + `Plugins` button inside the right side of the bottom footer
/// bar (`sb`, ending at `right_edge`) and returns the x-coordinate where the cluster begins.
pub fn show_inline(app: &mut FilmcraftApp, ui: &mut egui::Ui, sb: Rect, right_edge: f32) -> f32 {
    let t = app.tokens;
    let p = ui.painter().clone();
    let cur = active_page(app);
    let font = Tokens::semibold(10.5);

    let plug_label = tl!("Plugins");
    let pg = p.layout_no_wrap(plug_label.to_string(), Tokens::ui(10.5), t.tab_text);
    let plug_w = pg.size().x + 22.0;

    let mut widths = [0.0_f32; 7];
    let mut total_w = plug_w + 6.0;
    for (i, tab) in PAGES.iter().enumerate() {
        let shown = crate::i18n::t(tab.label);
        let g = p.layout_no_wrap(shown.to_string(), font.clone(), t.tab_text);
        let w = (g.size().x + 22.0).max(48.0);
        widths[i] = w;
        total_w += w + 2.0;
    }

    let start_x = (right_edge - 8.0 - total_w).max(sb.min.x + 260.0);
    if start_x + total_w > right_edge - 4.0 {
        return right_edge;
    }

    let mut x = start_x;
    for (i, tab) in PAGES.iter().enumerate() {
        let w = widths[i];
        let pill = Rect::from_min_size(pos2(x, sb.min.y + 1.5), vec2(w, sb.height() - 3.0));
        let active = cur == tab.id;
        let tip = crate::i18n::t(tab.tip);
        let resp = ui.interact(pill, egui::Id::unique(("page-bar", tab.id)), Sense::click()).on_hover_text(tip);
        app.auto.add(&format!("status.page.{}", tab.id), pill, tab.label);

        if active {
            p.rect_filled(pill, 3.0, t.pressed);
            p.line_segment([pos2(pill.min.x + 5.0, pill.max.y - 0.5), pos2(pill.max.x - 5.0, pill.max.y - 0.5)], Stroke::new(1.8, t.accent));
        } else if resp.hovered() {
            p.rect_filled(pill, 3.0, t.hover);
        }

        let col = if active || resp.hovered() { t.tab_text_active } else { t.tab_text };
        let icon_r = Rect::from_center_size(pos2(pill.min.x + 9.0, pill.center().y), vec2(10.5, 10.5));
        icons::paint(&p, icon_r, tab.icon, if active { t.accent } else { col });
        p.text(pos2(icon_r.max.x + 3.5, pill.center().y), Align2::LEFT_CENTER, crate::i18n::t(tab.label), font.clone(), col);

        if resp.clicked() {
            activate_page(app, tab.id);
        }
        x += w + 2.0;
    }

    x += 4.0;
    let pr = Rect::from_min_size(pos2(x, sb.min.y + 1.5), vec2(plug_w, sb.height() - 3.0));
    let open = app.ui.mode == Mode::Edit && app.ui.dock.contains(&PanelKind::Plugins);
    let presp = ui.interact(pr, egui::Id::unique("page-bar-plugins"), Sense::click()).on_hover_text(tl!("Open Plugin Hub & Live DCTL Studio"));
    app.auto.add("status.page.plugins", pr, "Plugins");
    if open {
        p.rect_filled(pr, 3.0, t.pressed);
    } else if presp.hovered() {
        p.rect_filled(pr, 3.0, t.hover);
    }
    let col = if open || presp.hovered() { t.tab_text_active } else { t.text_dim };
    icons::paint(&p, Rect::from_center_size(pos2(pr.min.x + 9.0, pr.center().y), vec2(10.5, 10.5)), Icon::Fx, col);
    p.galley_with_override_text_color(pos2(pr.min.x + 17.0, pr.center().y - pg.size().y * 0.5), pg, col);
    if presp.clicked() {
        app.ui.mode = Mode::Edit;
        app.show_panel(PanelKind::Plugins);
    }

    start_x
}
