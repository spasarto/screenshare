//! Small building blocks shared by every screen in the viewer, all drawing
//! from the tokens in [`crate::viewer::theme`]. Nothing here is
//! screen-specific — that stays in `viewer/mod.rs`.

use crate::viewer::theme;
use egui::{FontFamily, Response, RichText, Stroke, Ui, Vec2};

/// A fixed-width column, horizontally centered within the available space.
///
/// Every screen should wrap its content in this instead of centring each
/// child individually — that mismatch (widgets centred one by one vs. a
/// nested `horizontal` claiming the full width) is what put the setup
/// screen's logo and its form on two different axes.
pub fn centered_column<R>(ui: &mut Ui, width: f32, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let margin = ((ui.available_width() - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(width);
            add_contents(ui)
        })
        .inner
    })
    .inner
}

const BUTTON_HEIGHT: f32 = 36.0;

/// The filled, high-emphasis button for a screen's one primary action
/// (e.g. "Start"). `enabled` routes through `Ui::add_enabled` so a disabled
/// primary button still renders (dimmed) rather than disappearing.
pub fn primary_button(ui: &mut Ui, enabled: bool, text: impl Into<String>) -> Response {
    let p = theme::palette_for(ui);
    let text = text.into();
    ui.scope(|ui| {
        style_button(ui, p.accent, p.accent_hover, p.on_accent, Stroke::NONE);
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(text).family(FontFamily::Name(theme::MEDIUM.into())))
                .corner_radius(6)
                .min_size(Vec2::new(0.0, BUTTON_HEIGHT)),
        )
    })
    .inner
}

/// The outline, low-emphasis button for secondary actions (e.g. "Cancel").
pub fn secondary_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = theme::palette_for(ui);
    let text = text.into();
    ui.scope(|ui| {
        style_button(
            ui,
            p.surface_2,
            p.surface_2,
            p.ink,
            Stroke::new(1.0, p.hairline),
        );
        ui.add(
            egui::Button::new(text)
                .corner_radius(6)
                .min_size(Vec2::new(0.0, BUTTON_HEIGHT)),
        )
    })
    .inner
}

/// Overrides the inactive/hovered/active widget visuals for the buttons
/// added inside this `ui.scope`, so we get egui's normal hover/press
/// feedback for free instead of a single flat fill (which `Button::fill`
/// would otherwise lock in for every interaction state).
fn style_button(ui: &mut Ui, fill: egui::Color32, hover_fill: egui::Color32, fg: egui::Color32, stroke: Stroke) {
    let widgets = &mut ui.style_mut().visuals.widgets;
    for visuals in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
    ] {
        visuals.bg_stroke = stroke;
        visuals.fg_stroke = Stroke::new(1.0, fg);
    }
    widgets.inactive.bg_fill = fill;
    widgets.inactive.weak_bg_fill = fill;
    widgets.hovered.bg_fill = hover_fill;
    widgets.hovered.weak_bg_fill = hover_fill;
    widgets.active.bg_fill = hover_fill;
    widgets.active.weak_bg_fill = hover_fill;
}

/// A card: a bordered, rounded surface used to visually group content.
/// Not yet used by phase 1's screens — reserved for the settings/stats
/// panels planned for later phases.
#[allow(dead_code)]
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let p = theme::palette_for(ui);
    egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.hairline))
        .corner_radius(8)
        .inner_margin(theme::space::LG)
        .show(ui, add_contents)
}

/// A small uppercase label used to introduce a section (e.g. "Window:" ->
/// "SELECT A WINDOW").
pub fn section_header(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = theme::palette_for(ui);
    ui.label(
        RichText::new(text.into().to_uppercase())
            .size(theme::size::SM)
            .color(p.ink_3)
            .family(FontFamily::Name(theme::MEDIUM.into())),
    )
}

/// What a [`status_pill`] is reporting, mapped to a semantic colour (never a
/// raw `Color32` literal) and a distinct glyph so status doesn't rely on
/// hue alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Positive,
    Warning,
    Negative,
    Neutral,
}

/// A small rounded status badge, replacing bare `colored_label(Color32::…)`
/// calls throughout the viewing screen.
pub fn status_pill(ui: &mut Ui, kind: StatusKind, text: impl Into<String>) -> Response {
    let p = theme::palette_for(ui);
    let (fg, bg, glyph) = match kind {
        StatusKind::Positive => (p.connected, p.connected_wash, "\u{25CF}"), // ●
        StatusKind::Warning => (p.reconnecting, p.reconnecting_wash, "\u{25D0}"), // ◐
        StatusKind::Negative => (p.failed, p.failed_wash, "\u{2715}"),       // ✕
        StatusKind::Neutral => (p.ink_3, p.surface_2, "\u{25CB}"),           // ○
    };
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{glyph} {}", text.into()))
                    .size(theme::size::SM)
                    .color(fg)
                    .family(FontFamily::Name(theme::MEDIUM.into())),
            )
        })
        .response
}
