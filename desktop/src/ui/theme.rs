use eframe::egui::{self, Color32, Context, FontData, FontFamily, FontId, RichText, Stroke, Vec2};

pub(super) const BACKGROUND: Color32 = Color32::from_rgb(29, 30, 37);
pub(super) const SURFACE: Color32 = Color32::from_rgb(40, 41, 48);
pub(super) const DIVIDER: Color32 = Color32::from_rgb(57, 58, 68);
pub(super) const TEXT: Color32 = Color32::from_rgb(244, 244, 247);
pub(super) const MUTED: Color32 = Color32::from_rgb(166, 171, 184);
pub(super) const GREEN: Color32 = Color32::from_rgb(74, 222, 128);
pub(super) const AMBER: Color32 = Color32::from_rgb(255, 179, 64);
pub(super) const CORAL: Color32 = Color32::from_rgb(255, 118, 111);
pub(super) const BLUE: Color32 = Color32::from_rgb(23, 105, 214);
pub(super) const BLUE_TEXT: Color32 = Color32::from_rgb(132, 184, 255);
const SEMIBOLD: &str = "Inter SemiBold";

pub(super) fn regular(label: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(label).size(size).color(color)
}

pub(super) fn semibold(label: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(label)
        .font(FontId::new(size, FontFamily::Name(SEMIBOLD.into())))
        .color(color)
}

pub(super) fn install(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        FontData::from_static(include_bytes!("../../assets/Inter-Regular.otf")).into(),
    );
    fonts.font_data.insert(
        SEMIBOLD.to_owned(),
        FontData::from_static(include_bytes!("../../assets/Inter-SemiBold.otf")).into(),
    );
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "Inter".to_owned());
    let mut semibold = proportional.clone();
    semibold.insert(0, SEMIBOLD.to_owned());
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.window_corner_radius = 12.into();
    visuals.window_stroke = Stroke::new(1.0_f32, DIVIDER);
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.hyperlink_color = BLUE_TEXT;
    visuals.selection.bg_fill = Color32::from_rgb(50, 69, 101);
    visuals.selection.stroke = Stroke::new(1.0_f32, BLUE_TEXT);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, DIVIDER);
    widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    widgets.inactive.bg_fill = Color32::from_rgb(49, 50, 59);
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 55, 66);
    widgets.hovered.bg_stroke = Stroke::NONE;
    widgets.hovered.expansion = 0.0;
    widgets.open.weak_bg_fill = widgets.hovered.weak_bg_fill;
    widgets.open.bg_stroke = Stroke::NONE;
    widgets.active.expansion = 0.0;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, BLUE_TEXT);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = Vec2::new(7.0, 0.0);
        style.spacing.button_padding = Vec2::new(7.0, 4.0);
        style.spacing.interact_size.y = 24.0;
        style.spacing.icon_width = 11.0;
        style.spacing.icon_spacing = 5.0;
    });
}
