//! Neutral branding for this local modified build (AGENTS.md §1.8).
pub fn wordmark(_ctx: &egui::Context, _dark: bool) -> Option<egui::TextureHandle> {
    None
}
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, _dark: bool) -> Option<egui::Rect> {
    let galley = ui.painter().layout_no_wrap("FilmCraft Korean".into(), egui::FontId::proportional(height * 0.7), ui.visuals().text_color());
    let rect = egui::Rect::from_min_size(egui::pos2(left_center.x, left_center.y - galley.size().y / 2.0), galley.size());
    ui.painter().galley(rect.min, galley, ui.visuals().text_color());
    Some(rect)
}
