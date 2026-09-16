//! Design tokens for the viewer UI: colour palette, spacing scale, type
//! scale, and the embedded typeface. [`install`] compiles all of it into an
//! `egui::Style` (one per light/dark theme) and applies it once, up front,
//! so every screen draws from the same tokens instead of hand-tuned
//! literals scattered through `viewer/mod.rs`.

use egui::epaint::text::VariationCoords;
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Stroke, TextStyle};
use std::sync::Arc;

/// Named font family for medium-weight text (buttons, status text).
pub const MEDIUM: &str = "medium";
/// Named font family for semibold-weight text (headings).
pub const SEMIBOLD: &str = "semibold";

/// 4px spacing scale. Every `add_space` and layout gap in the viewer should
/// come from here rather than a hand-picked pixel value.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
    /// Reserved for later phases (e.g. spacing above a hero moment).
    #[allow(dead_code)]
    pub const XXXL: f32 = 48.0;
}

/// Type scale: four sizes, covering everything except the one deliberate
/// "hero" display size used for the session code.
pub mod size {
    pub const SM: f32 = 12.0;
    pub const BODY: f32 = 15.0;
    pub const LG: f32 = 20.0;
    pub const HERO: f32 = 48.0;
}

/// Semantic colour tokens for one theme (light or dark). Reused everywhere
/// a screen previously reached for a raw `Color32::` literal.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub surface: Color32,
    pub surface_2: Color32,
    pub ink: Color32,
    pub ink_2: Color32,
    pub ink_3: Color32,
    pub hairline: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_wash: Color32,
    /// Text/icon colour to place on top of a filled `accent` background.
    pub on_accent: Color32,
    pub connected: Color32,
    pub connected_wash: Color32,
    pub reconnecting: Color32,
    pub reconnecting_wash: Color32,
    pub failed: Color32,
    pub failed_wash: Color32,
}

/// The palette for a given theme.
pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: Color32::from_rgb(0x0C, 0x12, 0x14),
            surface: Color32::from_rgb(0x13, 0x1B, 0x1E),
            surface_2: Color32::from_rgb(0x1A, 0x24, 0x27),
            ink: Color32::from_rgb(0xE4, 0xEB, 0xEC),
            ink_2: Color32::from_rgb(0xAF, 0xBE, 0xC1),
            ink_3: Color32::from_rgb(0x7D, 0x8E, 0x92),
            hairline: Color32::from_rgb(0x26, 0x32, 0x36),
            accent: Color32::from_rgb(0x5B, 0xC6, 0xD6),
            accent_hover: Color32::from_rgb(0x8F, 0xDC, 0xE7),
            accent_wash: Color32::from_rgb(0x12, 0x2A, 0x30),
            on_accent: Color32::from_rgb(0x0C, 0x12, 0x14),
            connected: Color32::from_rgb(0x6F, 0xD9, 0xA0),
            connected_wash: Color32::from_rgb(0x12, 0x30, 0x1F),
            reconnecting: Color32::from_rgb(0xD9, 0xAE, 0x52),
            reconnecting_wash: Color32::from_rgb(0x2B, 0x23, 0x14),
            failed: Color32::from_rgb(0xE0, 0x86, 0x78),
            failed_wash: Color32::from_rgb(0x2E, 0x1C, 0x18),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(0xF2, 0xF5, 0xF5),
            surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            surface_2: Color32::from_rgb(0xE9, 0xEE, 0xEE),
            ink: Color32::from_rgb(0x10, 0x17, 0x19),
            ink_2: Color32::from_rgb(0x3D, 0x4A, 0x4D),
            ink_3: Color32::from_rgb(0x6B, 0x7A, 0x7E),
            hairline: Color32::from_rgb(0xD6, 0xDE, 0xDE),
            accent: Color32::from_rgb(0x16, 0x70, 0x7F),
            accent_hover: Color32::from_rgb(0x0E, 0x57, 0x63),
            accent_wash: Color32::from_rgb(0xDC, 0xED, 0xEF),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            connected: Color32::from_rgb(0x1E, 0x7F, 0x52),
            connected_wash: Color32::from_rgb(0xDF, 0xF3, 0xE7),
            reconnecting: Color32::from_rgb(0x96, 0x70, 0x0F),
            reconnecting_wash: Color32::from_rgb(0xF6, 0xED, 0xDA),
            failed: Color32::from_rgb(0xAE, 0x40, 0x34),
            failed_wash: Color32::from_rgb(0xF6, 0xE3, 0xE0),
        }
    }
}

/// The palette matching whatever theme `ui` is currently drawing in
/// (light/dark, possibly following the OS live).
pub fn palette_for(ui: &egui::Ui) -> Palette {
    palette(ui.ctx().theme() == egui::Theme::Dark)
}

/// Install the theme: fonts, colours and spacing, for both light and dark,
/// then follow the OS preference between them. Call once, from `App::new`.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());

    ctx.set_style_of(egui::Theme::Light, build_style(false));
    ctx.set_style_of(egui::Theme::Dark, build_style(true));
    ctx.set_theme(egui::ThemePreference::System);
}

/// Embeds one variable font (Inter) and registers it three times under
/// different variation-axis coordinates, so "three weights" is three
/// `wght` instances of the same file rather than three separate downloads.
fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    let base = FontData::from_static(include_bytes!("../assets/fonts/Inter.ttf"));
    let at_weight = |wght: f32| {
        base.clone().tweak(FontTweak {
            coords: VariationCoords::new([("wght", wght)]),
            ..Default::default()
        })
    };

    fonts
        .font_data
        .insert("inter-regular".to_owned(), Arc::new(at_weight(430.0)));
    fonts
        .font_data
        .insert(MEDIUM.to_owned(), Arc::new(at_weight(560.0)));
    fonts
        .font_data
        .insert(SEMIBOLD.to_owned(), Arc::new(at_weight(680.0)));

    // Body copy: Inter Regular first, keeping egui's bundled emoji glyphs as
    // fallback so status icons and the like keep working.
    let emoji_fallback: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let mut proportional = vec!["inter-regular".to_owned()];
    proportional.extend(emoji_fallback.iter().cloned());
    fonts
        .families
        .insert(FontFamily::Proportional, proportional);

    for (family_name, font_key) in [(MEDIUM, MEDIUM), (SEMIBOLD, SEMIBOLD)] {
        let mut list = vec![font_key.to_owned()];
        list.extend(emoji_fallback.iter().cloned());
        fonts
            .families
            .insert(FontFamily::Name(family_name.into()), list);
    }

    fonts
}

fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    [
        (TextStyle::Small, FontId::new(size::SM, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(size::BODY, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(size::BODY, FontFamily::Name(MEDIUM.into())),
        ),
        (
            TextStyle::Heading,
            FontId::new(size::LG, FontFamily::Name(SEMIBOLD.into())),
        ),
        (TextStyle::Monospace, FontId::new(size::BODY, FontFamily::Monospace)),
    ]
    .into()
}

fn build_style(dark: bool) -> egui::Style {
    let mut style = egui::Style {
        text_styles: text_styles(),
        ..Default::default()
    };

    style.spacing.item_spacing = egui::vec2(space::SM, space::SM);
    style.spacing.button_padding = egui::vec2(space::LG, space::SM);
    style.spacing.window_margin = space::LG.into();
    style.spacing.menu_margin = space::SM.into();
    style.spacing.indent = space::LG;
    style.spacing.interact_size.y = 34.0;

    style.visuals = build_visuals(dark);
    style
}

fn build_visuals(dark: bool) -> egui::Visuals {
    let p = palette(dark);
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.override_text_color = Some(p.ink);
    v.hyperlink_color = p.accent;
    v.faint_bg_color = p.surface_2;
    v.extreme_bg_color = p.surface_2;
    v.text_edit_bg_color = Some(p.surface);
    v.code_bg_color = p.surface_2;
    v.warn_fg_color = p.reconnecting;
    v.error_fg_color = p.failed;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.hairline);
    v.panel_fill = p.bg;
    v.selection.bg_fill = p.accent_wash;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    v.widgets.noninteractive.bg_fill = p.bg;
    v.widgets.noninteractive.weak_bg_fill = p.bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.hairline);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.ink_2);
    v.widgets.noninteractive.corner_radius = 6.into();

    v.widgets.inactive.bg_fill = p.surface_2;
    v.widgets.inactive.weak_bg_fill = p.surface_2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.hairline);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.inactive.corner_radius = 6.into();

    v.widgets.hovered.bg_fill = p.surface_2;
    v.widgets.hovered.weak_bg_fill = p.surface_2;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.hovered.corner_radius = 6.into();

    v.widgets.active.bg_fill = p.accent_wash;
    v.widgets.active.weak_bg_fill = p.accent_wash;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.active.corner_radius = 6.into();

    v.widgets.open.bg_fill = p.surface_2;
    v.widgets.open.weak_bg_fill = p.surface_2;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.open.corner_radius = 6.into();

    v
}
