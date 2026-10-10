//! Look and feel: bundled fonts, Phosphor icons and a light palette with a
//! purple accent.

use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontFamily, FontId, Margin, Stroke, TextStyle};

pub use egui_phosphor::regular as icon;

/// A connection's colour tag: the strong colour and a light tint for backgrounds.
pub fn conn_color(c: dbm_core::config::ConnColor) -> (Color32, Color32) {
    use dbm_core::config::ConnColor::*;
    match c {
        Red => (Color32::from_rgb(220, 60, 60), Color32::from_rgb(253, 232, 232)),
        Orange => (Color32::from_rgb(232, 128, 30), Color32::from_rgb(254, 239, 222)),
        Yellow => (Color32::from_rgb(214, 172, 20), Color32::from_rgb(253, 246, 214)),
        Green => (Color32::from_rgb(52, 160, 80), Color32::from_rgb(228, 245, 232)),
        Blue => (Color32::from_rgb(50, 120, 220), Color32::from_rgb(226, 238, 252)),
        Purple => (Color32::from_rgb(130, 80, 210), Color32::from_rgb(240, 232, 252)),
    }
}

/// Palette shared by the custom-drawn parts of the UI.
pub mod color {
    use eframe::egui::Color32;

    pub const ACCENT: Color32 = Color32::from_rgb(91, 52, 224);
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(76, 43, 199);
    /// Selected rows, chips and active sidebar items.
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(238, 235, 253);
    pub const TEXT: Color32 = Color32::from_rgb(31, 31, 35);
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(110, 110, 120);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(150, 150, 160);
    pub const BG: Color32 = Color32::from_rgb(255, 255, 255);
    pub const BG_SUBTLE: Color32 = Color32::from_rgb(247, 247, 249);
    pub const BG_SUNKEN: Color32 = Color32::from_rgb(241, 241, 244);
    /// Every other row of the results grid.
    pub const STRIPE: Color32 = Color32::from_rgb(245, 245, 248);
    pub const BORDER: Color32 = Color32::from_rgb(226, 226, 232);
    pub const CHANGED: Color32 = Color32::from_rgb(255, 248, 214);
    pub const CHANGED_EDGE: Color32 = Color32::from_rgb(232, 196, 64);
    pub const ADDED: Color32 = Color32::from_rgb(234, 247, 236);
    pub const DELETED: Color32 = Color32::from_rgb(253, 236, 236);
    pub const SUCCESS: Color32 = Color32::from_rgb(52, 168, 83);
    pub const WARNING: Color32 = Color32::from_rgb(220, 160, 30);
    pub const DANGER: Color32 = Color32::from_rgb(210, 60, 60);
    pub const LINK: Color32 = Color32::from_rgb(76, 43, 199);
}

/// Font families beyond egui's two built-in ones.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn font(size: f32, family: FontFamily) -> FontId {
    FontId::new(size, family)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let add = |fonts: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "inter", include_bytes!("../../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "inter-medium", include_bytes!("../../assets/fonts/Inter-Medium.ttf"));
    add(&mut fonts, "inter-semibold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut fonts, "jetbrains", include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"));

    // Each family falls back to egui's defaults for glyphs the bundled fonts lack.
    let defaults =
        |fonts: &egui::FontDefinitions, family: &FontFamily| fonts.families.get(family).cloned().unwrap_or_default();
    let proportional_fallback = defaults(&fonts, &FontFamily::Proportional);
    let mono_fallback = defaults(&fonts, &FontFamily::Monospace);
    let family =
        |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect();
    fonts.families.insert(FontFamily::Proportional, family("inter", &proportional_fallback));
    fonts.families.insert(medium(), family("inter-medium", &proportional_fallback));
    fonts.families.insert(semibold(), family("inter-semibold", &proportional_fallback));
    fonts.families.insert(FontFamily::Monospace, family("jetbrains", &mono_fallback));

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Icons inside monospace and weighted text too. The bundled Inter is
    // subset without private-use glyphs, which would otherwise shadow
    // Phosphor's codepoints.
    for f in [FontFamily::Monospace, medium(), semibold()] {
        if let Some(keys) = fonts.families.get_mut(&f) {
            keys.insert(1.min(keys.len()), "phosphor".into());
        }
    }
    ctx.set_fonts(fonts);
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::Theme::Light);
    ctx.style_mut_of(egui::Theme::Light, |style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Heading, FontId::new(15.0, semibold())),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.menu_margin = Margin::same(6);
        style.interaction.selectable_labels = false;

        let v = &mut style.visuals;
        *v = egui::Visuals::light();
        v.override_text_color = Some(color::TEXT);
        v.panel_fill = color::BG;
        v.window_fill = color::BG;
        v.extreme_bg_color = color::BG;
        v.faint_bg_color = color::BG_SUBTLE;
        v.code_bg_color = color::BG_SUNKEN;
        v.window_stroke = Stroke::new(1.0, color::BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow.color = Color32::from_black_alpha(28);
        v.popup_shadow.color = Color32::from_black_alpha(24);
        v.hyperlink_color = color::LINK;
        v.selection.bg_fill = color::ACCENT_SOFT;
        v.selection.stroke = Stroke::new(1.0, color::ACCENT);
        v.error_fg_color = color::DANGER;
        v.warn_fg_color = color::WARNING;

        let radius = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_stroke = Stroke::new(1.0, color::BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, color::TEXT);
        w.noninteractive.corner_radius = radius;
        w.inactive.bg_fill = color::BG;
        w.inactive.weak_bg_fill = color::BG;
        w.inactive.bg_stroke = Stroke::new(1.0, color::BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, color::TEXT);
        w.inactive.corner_radius = radius;
        w.hovered.bg_fill = color::BG_SUBTLE;
        w.hovered.weak_bg_fill = color::BG_SUBTLE;
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(205, 205, 214));
        w.hovered.fg_stroke = Stroke::new(1.0, color::TEXT);
        w.hovered.corner_radius = radius;
        w.active.bg_fill = color::BG_SUNKEN;
        w.active.weak_bg_fill = color::BG_SUNKEN;
        w.active.bg_stroke = Stroke::new(1.0, color::ACCENT);
        w.active.fg_stroke = Stroke::new(1.0, color::TEXT);
        w.active.corner_radius = radius;
        w.open = w.hovered;
    });
}

/// The filled purple button used for the main action of a toolbar.
pub fn primary_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(Color32::WHITE).font(font(13.0, medium())))
        .fill(color::ACCENT)
        .stroke(Stroke::NONE)
}

/// A borderless button, for secondary toolbar actions.
pub fn flat_button(text: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(text).frame_when_inactive(false)
}

/// Small grey section caption, as in the sidebar ("TABLES").
pub fn caption(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).font(font(10.5, semibold())).color(color::TEXT_WEAK)
}

/// A rounded pill with soft background, for chips and badges.
pub fn pill(ui: &mut egui::Ui, text: egui::RichText, fill: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| ui.label(text))
        .response
}
