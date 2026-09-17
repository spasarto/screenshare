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

/// A secondary-weight button that stays visibly "on" while `selected` —
/// for view toggles like Fit / 1:1 and Stats, where the current state
/// matters as much as the action.
pub fn toggle_button(ui: &mut Ui, selected: bool, text: impl Into<String>) -> Response {
    if !selected {
        return secondary_button(ui, text);
    }
    let p = theme::palette_for(ui);
    let text = text.into();
    ui.scope(|ui| {
        style_button(ui, p.accent_wash, p.accent_wash, p.ink, Stroke::new(1.0, p.accent));
        ui.add(
            egui::Button::new(text)
                .corner_radius(6)
                .min_size(Vec2::new(0.0, BUTTON_HEIGHT)),
        )
    })
    .inner
}

/// An outline button in the failure colour, for the one action that ends
/// something (e.g. "Disconnect"). Not filled: it should be findable, not the
/// loudest thing on screen.
pub fn danger_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = theme::palette_for(ui);
    let text = text.into();
    ui.scope(|ui| {
        style_button(ui, p.surface_2, p.failed_wash, p.failed, Stroke::new(1.0, p.failed));
        ui.add(
            egui::Button::new(RichText::new(text).family(FontFamily::Name(theme::MEDIUM.into())))
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
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let p = theme::palette_for(ui);
    egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.hairline))
        .corner_radius(8)
        .inner_margin(theme::space::LG)
        .show(ui, add_contents)
}

/// A large clickable card for a top-level choice (e.g. "Start a session" vs.
/// "Join a session"). Unlike [`primary_button`]/[`secondary_button`], both
/// options here carry equal visual weight — this is a fork in the journey,
/// not a primary/secondary action pair.
pub fn choice_card(ui: &mut Ui, title: &str, description: &str) -> Response {
    let p = theme::palette_for(ui);
    let resp = egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.hairline))
        .corner_radius(10)
        .inner_margin(theme::space::LG)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .size(theme::size::LG)
                        .family(FontFamily::Name(theme::SEMIBOLD.into())),
                );
                ui.add_space(theme::space::XS);
                ui.colored_label(p.ink_2, description);
            });
        })
        .response;

    let resp = resp.interact(egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_stroke(
            resp.rect,
            10.0,
            Stroke::new(1.5, p.accent),
            egui::StrokeKind::Inside,
        );
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    resp
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
        // Glyphs Inter itself has. ◐ and ✕ fell through to egui's fallback
        // fonts and drew as empty boxes.
        StatusKind::Warning => (p.reconnecting, p.reconnecting_wash, "\u{26A0}"), // ⚠
        StatusKind::Negative => (p.failed, p.failed_wash, "\u{00D7}"),       // ×
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

/// Where one step of a multi-step progress list has got to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Done,
    Current,
    Pending,
}

/// A vertical checklist of the steps a session goes through, with the
/// current one spinning. Replaces the single unlabelled spinner that made
/// "creating", "waiting" and "joining" look like the same screen: the point
/// is to show how far along the handshake is, not just that it is busy.
pub fn progress_steps(ui: &mut Ui, steps: &[(&str, StepState)]) {
    let p = theme::palette_for(ui);
    ui.scope(|ui| {
        let spacing = ui.spacing_mut();
        // One list, not four unrelated labels.
        spacing.item_spacing.y = theme::space::XS;
        // The rows are text, not controls. Without this they each inherit
        // the 34px minimum height meant for clickable widgets, which is
        // most of a second list's worth of empty space over four steps.
        spacing.interact_size.y = 0.0;

        for (label, state) in steps {
            ui.horizontal(|ui| {
                // Fixed-width gutter so the labels line up regardless of
                // which marker (glyph or spinner) each row happens to draw.
                ui.allocate_ui(Vec2::new(MARKER_WIDTH, MARKER_WIDTH), |ui| match state {
                    StepState::Done => {
                        ui.label(RichText::new("\u{2713}").color(p.connected)); // ✓
                    }
                    StepState::Current => {
                        ui.add(egui::Spinner::new().size(theme::size::BODY));
                    }
                    StepState::Pending => {
                        ui.label(
                            RichText::new("\u{25CB}") // ○
                                .size(theme::size::SM)
                                .color(p.ink_3),
                        );
                    }
                });
                let text = RichText::new(*label);
                ui.label(match state {
                    StepState::Done => text.color(p.ink_2),
                    StepState::Current => text
                        .color(p.ink)
                        .family(FontFamily::Name(theme::MEDIUM.into())),
                    StepState::Pending => text.color(p.ink_3),
                });
            });
        }
    });
}

const MARKER_WIDTH: f32 = 18.0;

/// A QR code, rasterised once per distinct payload and kept as a texture.
///
/// Held by the caller rather than rebuilt per frame: encoding is real work,
/// and `load_texture` on every repaint would churn a GPU texture for pixels
/// that only change when the session code does.
#[derive(Default)]
pub struct QrCache {
    key: String,
    texture: Option<egui::TextureHandle>,
}

impl QrCache {
    /// Paints `data` as a QR code `size` px on a side (quiet zone included),
    /// re-encoding only when `data` changes. A payload too long to encode
    /// draws nothing rather than failing the frame.
    pub fn show(&mut self, ui: &mut Ui, data: &str, size: f32) {
        if self.key != data || self.texture.is_none() {
            self.texture = qr_image(data).map(|img| {
                ui.ctx()
                    .load_texture("session-qr", img, egui::TextureOptions::NEAREST)
            });
            self.key = data.to_owned();
        }

        let Some(tex) = &self.texture else { return };
        ui.add(egui::Image::new(egui::load::SizedTexture::new(
            tex.id(),
            Vec2::splat(size),
        )));
    }
}

/// One image pixel per QR module would be scaled up by the GPU; this many
/// keeps the edges crisp under `TextureOptions::NEAREST` at any display size.
const QR_SCALE: usize = 4;
/// The four-module quiet zone the QR spec requires around the symbol —
/// without it, scanners struggle to find the finder patterns.
const QR_QUIET: usize = 4;

/// Renders `data` to a black-on-white QR bitmap.
///
/// Deliberately not theme-aware: scanners expect dark modules on a light
/// ground, and inverting one for a dark UI is a coin flip on whether a phone
/// camera reads it at all.
fn qr_image(data: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + QR_QUIET * 2) * QR_SCALE;

    let mut pixels = vec![egui::Color32::WHITE; side * side];
    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] != qrcode::Color::Dark {
                continue;
            }
            let (px, py) = ((x + QR_QUIET) * QR_SCALE, (y + QR_QUIET) * QR_SCALE);
            for row in py..py + QR_SCALE {
                pixels[row * side + px..row * side + px + QR_SCALE]
                    .fill(egui::Color32::BLACK);
            }
        }
    }

    Some(egui::ColorImage {
        size: [side, side],
        source_size: Vec2::new(side as f32, side as f32),
        pixels,
    })
}
