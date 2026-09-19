//! The in-app log: a resizable drawer along the bottom of the window,
//! replacing the F10 console. Fed by [`crate::logging`], so it works in
//! release builds, and everything in it can be copied out in one click.

use crate::logging::{self, LogBuffer, LogLine};
use crate::viewer::{theme, widgets};
use egui::{RichText, TextStyle};
use tracing::Level;

const DEFAULT_HEIGHT: f32 = 220.0;
const MIN_HEIGHT: f32 = 120.0;

/// Levels the drawer can filter down to, most severe first.
const FILTERS: [(Level, &str); 4] = [
    (Level::ERROR, "Errors"),
    (Level::WARN, "Warnings"),
    (Level::INFO, "Info"),
    (Level::DEBUG, "Debug"),
];

pub struct LogDrawer {
    open: bool,
    /// Show lines at this level or more severe.
    min_level: Level,
}

impl Default for LogDrawer {
    fn default() -> Self {
        Self {
            open: false,
            min_level: Level::INFO,
        }
    }
}

impl LogDrawer {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    /// Adds the drawer as a bottom panel. Call before the screen's own
    /// panels, so it sits outermost and every screen gives up the space.
    pub fn show(&mut self, ui: &mut egui::Ui, buffer: &LogBuffer) {
        if !self.open {
            return;
        }
        egui::Panel::bottom("log_drawer")
            .resizable(true)
            .default_size(DEFAULT_HEIGHT)
            .min_size(MIN_HEIGHT)
            .show(ui, |ui| {
                ui.add_space(theme::space::XS);
                self.show_header(ui, buffer);
                ui.add_space(theme::space::XS);
                self.show_lines(ui, buffer);
            });
    }

    fn show_header(&mut self, ui: &mut egui::Ui, buffer: &LogBuffer) {
        ui.horizontal(|ui| {
            widgets::section_header(ui, "Log");
            ui.add_space(theme::space::SM);
            for (level, label) in FILTERS {
                if widgets::toggle_button(ui, self.min_level == level, label).clicked() {
                    self.min_level = level;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::secondary_button(ui, "Close")
                    .on_hover_text("F10")
                    .clicked()
                {
                    self.open = false;
                }
                if widgets::secondary_button(ui, "Clear").clicked() {
                    buffer.clear();
                }
                if widgets::secondary_button(ui, "Copy")
                    .on_hover_text("Copy the lines shown, as plain text")
                    .clicked()
                {
                    let text = buffer.with_lines(|lines| {
                        lines
                            .iter()
                            .filter(|l| l.level <= self.min_level)
                            .map(LogLine::to_text)
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                    ui.ctx().copy_text(text);
                }
            });
        });
    }

    fn show_lines(&self, ui: &mut egui::Ui, buffer: &LogBuffer) {
        let p = theme::palette_for(ui);
        let row_height = ui.text_style_height(&TextStyle::Monospace);

        buffer.with_lines(|lines| {
            let shown: Vec<&LogLine> = lines.iter().filter(|l| l.level <= self.min_level).collect();
            if shown.is_empty() {
                ui.colored_label(p.ink_3, "Nothing logged at this level yet.");
                return;
            }

            egui::ScrollArea::both()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show_rows(ui, row_height, shown.len(), |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    // Rows are text: without these, each would take the
                    // 34px minimum meant for controls and the spacing
                    // between them, and `row_height` would be wrong.
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().interact_size.y = 0.0;
                    for line in &shown[range] {
                        let level_color = match line.level {
                            Level::ERROR => p.failed,
                            Level::WARN => p.reconnecting,
                            _ => p.ink_3,
                        };
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = theme::space::SM;
                            ui.label(
                                RichText::new(logging::format_offset(line.at))
                                    .monospace()
                                    .color(p.ink_3),
                            );
                            ui.label(
                                RichText::new(format!("{:>5}", line.level))
                                    .monospace()
                                    .color(level_color),
                            );
                            ui.label(RichText::new(&line.target).monospace().color(p.ink_3));
                            ui.label(RichText::new(&line.message).monospace().color(p.ink));
                        });
                    }
                });
        });
    }
}
