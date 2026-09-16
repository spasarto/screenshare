//! The setup screen's "what to share" picker: a searchable grid of tiles —
//! one per shareable window plus one "entire screen" tile per monitor —
//! replacing the old bare title dropdown.
//!
//! Each window tile shows a live still of that window (`PrintWindow`,
//! refreshed on a throttled timer) with the window's own icon as an instant
//! fallback when the capture fails. See [`crate::capture::thumbnail`] for
//! why `PrintWindow` rather than a WGC session per tile.

use crate::capture::picker::{self, MonitorInfo, WindowInfo};
use crate::capture::thumbnail;
use crate::viewer::{theme, widgets};
use egui::{Color32, ColorImage, Rect, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const TILE_WIDTH: f32 = 156.0;
const ART_HEIGHT: f32 = 72.0;
/// Art size for the smaller [`PickerGrid::show_selected_preview`] card.
const PREVIEW_WIDTH: f32 = 72.0;
const PREVIEW_HEIGHT: f32 = 44.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    Window(usize),
    Monitor(usize),
}

#[derive(Default)]
struct TileArt {
    thumb: Option<TextureHandle>,
    icon: Option<TextureHandle>,
}

pub struct PickerGrid {
    windows: Vec<WindowInfo>,
    monitors: Vec<MonitorInfo>,
    /// Keyed by raw `HWND` rather than by index into `windows`, so art
    /// survives the re-enumeration each refresh does — indices shift as
    /// windows open and close, and rebuilding this per cycle is what used to
    /// throw every texture away (and re-pay the icon lookup) every 2 s.
    window_art: HashMap<usize, TileArt>,
    thumbs: thumbnail::Service,
    selection: Option<Selection>,
    search: String,
    last_refresh: Option<Instant>,
}

impl PickerGrid {
    pub fn new() -> Self {
        let mut grid = Self {
            windows: Vec::new(),
            monitors: picker::enumerate_monitors(),
            window_art: HashMap::new(),
            thumbs: thumbnail::Service::new(),
            selection: None,
            search: String::new(),
            last_refresh: None,
        };
        // Populate the list (so the first frame has tiles and a selection to
        // draw) but leave `last_refresh` clear, so that first frame also
        // fires the capture request instead of sitting on an empty grid for
        // a full `REFRESH_INTERVAL` before asking for any art.
        grid.refresh_window_list();
        grid.last_refresh = None;
        grid
    }

    pub fn has_targets(&self) -> bool {
        !self.windows.is_empty() || !self.monitors.is_empty()
    }

    pub fn selected_capture_target(&self) -> Option<crate::capture::CaptureTarget> {
        match self.selection? {
            Selection::Window(i) => self
                .windows
                .get(i)
                .map(|w| crate::capture::CaptureTarget::Window(w.window)),
            Selection::Monitor(i) => self
                .monitors
                .get(i)
                .map(|m| crate::capture::CaptureTarget::Monitor(m.monitor)),
        }
    }

    /// The title of the selected window (or the label of the selected
    /// monitor), for naming what's about to be shared.
    pub fn selected_label(&self) -> Option<String> {
        match self.selection? {
            Selection::Window(i) => self.windows.get(i).map(|w| w.title.clone()),
            Selection::Monitor(i) => self.monitors.get(i).map(|m| m.label.clone()),
        }
    }

    /// Re-enumerates windows, preserving the current selection by matching
    /// the underlying `HWND` (so a same-second refresh doesn't yank the
    /// picker out from under a user who's about to click Start), and falls
    /// back to the first available target if the selected window closed.
    fn refresh_window_list(&mut self) {
        let prev_hwnd = match self.selection {
            Some(Selection::Window(i)) => self.windows.get(i).map(|w| w.window),
            _ => None,
        };

        self.windows = picker::enumerate_windows().unwrap_or_default();

        // Keep the art we already have and drop only what closed, so a
        // window that's still open keeps showing its last still (and its
        // one-shot icon) instead of blanking out once every interval.
        let live: HashSet<usize> = self.windows.iter().map(window_key).collect();
        self.window_art.retain(|hwnd, _| live.contains(hwnd));

        if let Some(prev) = prev_hwnd
            && let Some(i) = self.windows.iter().position(|w| w.window == prev)
        {
            self.selection = Some(Selection::Window(i));
        }

        let selection_valid = match self.selection {
            Some(Selection::Window(i)) => i < self.windows.len(),
            Some(Selection::Monitor(i)) => i < self.monitors.len(),
            None => false,
        };
        if !selection_valid {
            self.selection = if !self.windows.is_empty() {
                Some(Selection::Window(0))
            } else if !self.monitors.is_empty() {
                Some(Selection::Monitor(0))
            } else {
                None
            };
        }

        self.last_refresh = Some(Instant::now());
    }

    /// Point the selection at a specific window, re-enumerating once if it
    /// isn't in the list yet. The `--process` auto-start finds its own
    /// target without going through the grid, and the "you're sharing X"
    /// preview would otherwise name whichever tile happened to be selected
    /// by default. Clears the selection (so the preview shows nothing rather
    /// than something false) if the window can't be found at all.
    pub fn select_window(&mut self, window: &windows_capture::window::Window) {
        if !self.windows.iter().any(|w| w.window == *window) {
            self.refresh_window_list();
        }
        self.selection = self
            .windows
            .iter()
            .position(|w| w.window == *window)
            .map(Selection::Window);
    }

    /// Re-enumerate windows without touching any textures. Cheap enough to
    /// call from anywhere (e.g. on returning to the picker from a cancelled
    /// session) without needing an `egui::Context` on hand.
    pub fn refresh_list(&mut self) {
        self.refresh_window_list();
        // Returning to the picker should show current stills, not whatever
        // was on screen when the session started.
        self.force_refresh();
    }

    /// Make the next frame re-enumerate and re-request art instead of
    /// waiting out the interval. Used by the explicit Refresh button, which
    /// can't do the work itself: which tiles are on screen isn't known until
    /// the grid below it has been laid out.
    fn force_refresh(&mut self) {
        self.last_refresh = None;
    }

    /// Upload whatever the worker finished since the last frame. Cheap
    /// enough to call every frame: a non-blocking drain plus an in-place
    /// texture update per finished tile.
    fn pump(&mut self, ctx: &egui::Context) {
        for ready in self.thumbs.poll() {
            let art = self.window_art.entry(ready.hwnd).or_default();
            let (slot, prefix, img) = match ready.art {
                thumbnail::Art::Thumb(img) => (&mut art.thumb, "picker-thumb", img),
                thumbnail::Art::Icon(img) => (&mut art.icon, "picker-icon", img),
            };
            let color = ColorImage::from_rgba_unmultiplied(
                [img.width as usize, img.height as usize],
                &img.rgba,
            );
            match slot {
                // `load_texture` allocates a fresh GPU texture and drops the
                // old one; on a 2 s timer that's a free/alloc/upload cycle
                // per tile for pixels that could have gone into the texture
                // already sitting there.
                Some(tex) => tex.set(color, TextureOptions::LINEAR),
                None => {
                    *slot = Some(ctx.load_texture(
                        format!("{prefix}-{:x}", ready.hwnd),
                        color,
                        TextureOptions::LINEAR,
                    ))
                }
            }
        }
    }

    /// Queue art for exactly the tiles that are on screen.
    ///
    /// Capturing the whole enumeration was most of the old cost: the grid
    /// draws at most `max_tiles`, so on a busy desktop the majority of every
    /// refresh was `PrintWindow` calls for windows filtered out by search or
    /// past the tile budget — work nobody could see.
    fn request_captures(&mut self, visible: &[usize]) {
        let batch: Vec<thumbnail::Request> = visible
            .iter()
            .filter_map(|&i| self.windows.get(i))
            .map(|w| {
                let hwnd = window_key(w);
                thumbnail::Request {
                    hwnd,
                    need_icon: self
                        .window_art
                        .get(&hwnd)
                        .is_none_or(|art| art.icon.is_none()),
                }
            })
            .collect();
        self.thumbs.request(batch);
    }

    /// `max_tiles` bounds how many tiles (monitors + windows) get drawn, so
    /// the caller can trade grid rows for whatever else it needs to fit in
    /// the window's fixed height (e.g. the Join code-entry row).
    pub fn show(&mut self, ui: &mut egui::Ui, max_tiles: usize) {
        self.pump(ui.ctx());

        ui.horizontal(|ui| {
            let avail = ui.available_width();
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search windows...")
                    .desired_width(avail - 96.0),
            );
            if widgets::secondary_button(ui, "Refresh").clicked() {
                self.force_refresh();
            }
        });

        ui.add_space(theme::space::SM);

        // Holding off while the worker is still busy is what bounds the cost
        // on a desktop slow enough that a batch outlasts the interval: the
        // refresh rate degrades instead of the queue growing without limit.
        let due = !self.thumbs.is_busy()
            && self
                .last_refresh
                .is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
        if due {
            self.refresh_window_list();
        }

        if !self.has_targets() {
            self.show_empty_state(ui);
            return;
        }

        // No ScrollArea here: wrapping this grid in one measurably breaks
        // layout in this egui version — a second wrapped row leaves a large
        // dead gap below it and pushes everything after the grid (including
        // the Start button) down by roughly a row's height, reproduced even
        // with a completely bare `ScrollArea::vertical()`. `egui::Grid`
        // doesn't have that problem, so it's used instead with `max_tiles`
        // (rather than an unbounded scroll) keeping the grid within the
        // window's fixed height; search narrows a longer list.
        let monitor_count = self.monitors.len().min(max_tiles);
        let window_budget = max_tiles.saturating_sub(monitor_count);

        let search = self.search.to_lowercase();
        let matches: Vec<usize> = (0..self.windows.len())
            .filter(|&i| search.is_empty() || self.windows[i].title.to_lowercase().contains(&search))
            .collect();

        if due {
            let visible: Vec<usize> = matches.iter().take(window_budget).copied().collect();
            self.request_captures(&visible);
        }

        if self.windows.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for i in 0..monitor_count {
                    self.monitor_tile(ui, i);
                }
                ui.vertical(|ui| {
                    ui.set_width(TILE_WIDTH * 2.0);
                    ui.add_space(theme::space::SM);
                    let p = theme::palette_for(ui);
                    ui.colored_label(
                        p.ink_3,
                        "No shareable windows found — open one, or share your entire screen above.",
                    );
                });
            });
        } else if matches.is_empty() && monitor_count == 0 {
            let p = theme::palette_for(ui);
            ui.colored_label(p.ink_3, "No windows match your search.");
        } else {
            egui::Grid::new("picker_grid")
                .num_columns(4)
                .spacing(egui::vec2(theme::space::SM, theme::space::SM))
                .show(ui, |ui| {
                    let mut col = 0usize;
                    for i in 0..monitor_count {
                        self.monitor_tile(ui, i);
                        col += 1;
                        if col == 4 {
                            ui.end_row();
                            col = 0;
                        }
                    }
                    for &i in matches.iter().take(window_budget) {
                        self.window_tile(ui, i);
                        col += 1;
                        if col == 4 {
                            ui.end_row();
                            col = 0;
                        }
                    }
                    if col != 0 {
                        ui.end_row();
                    }
                });
        }

        if matches.len() > window_budget {
            ui.add_space(theme::space::XS);
            let p = theme::palette_for(ui);
            ui.colored_label(
                p.ink_3,
                format!("+{} more — narrow with search", matches.len() - window_budget),
            );
        }
    }

    fn show_empty_state(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette_for(ui);
        egui::Frame::NONE
            .fill(p.surface_2)
            .corner_radius(8)
            .inner_margin(theme::space::LG)
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.colored_label(p.ink_2, "Nothing to share yet.");
                    ui.add_space(theme::space::XS);
                    ui.colored_label(
                        p.ink_3,
                        "No shareable windows or displays were found. Open a window, then refresh.",
                    );
                    ui.add_space(theme::space::SM);
                    if widgets::secondary_button(ui, "Refresh").clicked() {
                        self.force_refresh();
                    }
                });
            });
    }

    fn monitor_tile(&mut self, ui: &mut egui::Ui, idx: usize) {
        let selected = self.selection == Some(Selection::Monitor(idx));
        let (label, sub) = {
            let m = &self.monitors[idx];
            (m.label.clone(), format!("{}×{}", m.width, m.height))
        };

        let clicked = tile_frame(ui, selected, |ui| {
            let p = theme::palette_for(ui);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(TILE_WIDTH, ART_HEIGHT), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 6.0, p.surface_2);
            draw_monitor_glyph(ui, rect, if selected { p.accent } else { p.ink_3 });

            ui.add_space(theme::space::XS);
            ui.label(egui::RichText::new(label).size(theme::size::SM).strong());
            ui.colored_label(p.ink_3, egui::RichText::new(sub).size(theme::size::SM));
        });

        if clicked {
            self.selection = Some(Selection::Monitor(idx));
        }
    }

    fn window_tile(&mut self, ui: &mut egui::Ui, idx: usize) {
        let selected = self.selection == Some(Selection::Window(idx));
        let title = self.windows[idx].title.clone();
        let (thumb, icon) = match self.window_art.get(&window_key(&self.windows[idx])) {
            Some(art) => (art.thumb.clone(), art.icon.clone()),
            None => (None, None),
        };

        let clicked = tile_frame(ui, selected, |ui| {
            let p = theme::palette_for(ui);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(TILE_WIDTH, ART_HEIGHT), egui::Sense::hover());
            ui.painter().rect_filled(rect, 6.0, p.surface_2);

            if let Some(tex) = &thumb {
                paint_fit(ui, rect, tex);
            } else if let Some(tex) = &icon {
                paint_centered_icon(ui, rect, tex);
            } else {
                ui.painter().rect_stroke(
                    rect.shrink(1.0),
                    6.0,
                    Stroke::new(1.0, p.hairline),
                    StrokeKind::Inside,
                );
            }

            ui.add_space(theme::space::XS);
            ui.add(
                egui::Label::new(egui::RichText::new(title).size(theme::size::SM).strong())
                    .truncate(),
            );
        });

        if clicked {
            self.selection = Some(Selection::Window(idx));
        }
    }

    /// A compact "this is what you're sharing" card for the progress
    /// screens: the selected window's still (or its icon, or the monitor
    /// glyph) next to its title, plus a Change control. Returns true when
    /// Change was clicked.
    ///
    /// Keeps asking for art on the same throttle the grid uses, rather than
    /// only drawing what the grid happened to capture: the `--process`
    /// auto-start never shows the grid at all, and an empty box is a worse
    /// answer to "what am I sharing?" than a stale one.
    pub fn show_selected_preview(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        self.pump(ui.ctx());
        self.request_preview_art(selection);

        let p = theme::palette_for(ui);
        let label = self.selected_label().unwrap_or_default();
        let mut change = false;

        egui::Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.hairline))
            .corner_radius(8)
            .inner_margin(theme::space::SM)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(PREVIEW_WIDTH, PREVIEW_HEIGHT),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 4.0, p.surface_2);
                    match selection {
                        Selection::Monitor(_) => draw_monitor_glyph(ui, rect, p.ink_3),
                        Selection::Window(i) => {
                            let art = self
                                .windows
                                .get(i)
                                .and_then(|w| self.window_art.get(&window_key(w)));
                            if let Some(tex) = art.and_then(|a| a.thumb.as_ref()) {
                                paint_fit(ui, rect, tex);
                            } else if let Some(tex) = art.and_then(|a| a.icon.as_ref()) {
                                paint_centered_icon(ui, rect, tex);
                            }
                        }
                    }

                    ui.add_space(theme::space::SM);
                    // Change is laid out first, from the right: the title
                    // truncates to whatever is left, instead of claiming the
                    // full width and having the button drawn over it.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        change = widgets::secondary_button(ui, "Change")
                            .on_hover_text("Ends this session and returns to the picker")
                            .clicked();
                        ui.add_space(theme::space::SM);
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                            widgets::section_header(ui, "You're sharing");
                            ui.add(
                                egui::Label::new(egui::RichText::new(&label).strong()).truncate(),
                            )
                            // Window titles are long and the card is narrow;
                            // the truncated half is often the half that
                            // identifies it.
                            .on_hover_text(&label);
                        });
                    });
                });
            });

        change
    }

    /// Ask for a still of the one window the preview is showing, on the same
    /// interval the grid refreshes at. A no-op for a monitor selection,
    /// which draws a glyph rather than a capture.
    fn request_preview_art(&mut self, selection: Selection) {
        let Selection::Window(idx) = selection else {
            return;
        };
        let due = !self.thumbs.is_busy()
            && self
                .last_refresh
                .is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
        if !due {
            return;
        }
        // Reuse the grid's timer so returning to the picker doesn't
        // immediately fire a second batch for the tiles as well.
        self.last_refresh = Some(Instant::now());
        self.request_captures(&[idx]);
    }
}

/// Identity for the art cache. The raw `HWND` as a `usize` — the same key
/// the capture worker echoes back, and stable across re-enumeration in a way
/// a position in `windows` is not.
fn window_key(w: &WindowInfo) -> usize {
    w.window.as_raw_hwnd() as usize
}

/// A clickable, selectable card. Returns whether it was clicked this frame.
fn tile_frame<R>(ui: &mut egui::Ui, selected: bool, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> bool {
    let p = theme::palette_for(ui);
    let (stroke, fill) = if selected {
        (Stroke::new(2.0, p.accent), p.accent_wash)
    } else {
        (Stroke::new(1.0, p.hairline), p.surface)
    };

    let resp = egui::Frame::NONE
        .fill(fill)
        .stroke(stroke)
        .corner_radius(8)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_width(TILE_WIDTH);
            // Force top-down stacking for the art/label regardless of the
            // caller's own layout direction — inside a `Grid` cell that
            // direction is left-to-right, which `Frame::show`'s child `Ui`
            // otherwise inherits.
            ui.vertical(|ui| add_contents(ui))
        })
        .response;

    // No manual gap here: `theme::build_style`'s `item_spacing` already
    // separates cells in both the `Grid` and `horizontal_wrapped` callers,
    // and an explicit `add_space` panics inside a `Grid` layout.
    let resp = resp.interact(egui::Sense::click());
    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn paint_fit(ui: &mut egui::Ui, rect: Rect, tex: &TextureHandle) {
    let tex_size = tex.size_vec2();
    let scale = (rect.width() / tex_size.x).min(rect.height() / tex_size.y);
    let draw_size = tex_size * scale;
    let draw_rect = Rect::from_center_size(rect.center(), draw_size);
    ui.painter().image(
        tex.id(),
        draw_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn paint_centered_icon(ui: &mut egui::Ui, rect: Rect, tex: &TextureHandle) {
    let size = Vec2::splat((rect.height() * 0.45).min(tex.size_vec2().x));
    let draw_rect = Rect::from_center_size(rect.center(), size);
    ui.painter().image(
        tex.id(),
        draw_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn draw_monitor_glyph(ui: &mut egui::Ui, rect: Rect, color: Color32) {
    let w = rect.width().min(rect.height()) * 0.9;
    let h = w * 0.68;
    let screen = Rect::from_center_size(rect.center() - Vec2::new(0.0, h * 0.12), Vec2::new(w, h));
    let stroke = Stroke::new(2.0, color);
    ui.painter()
        .rect_stroke(screen, 4.0, stroke, StrokeKind::Inside);

    let stand_top = screen.max.y;
    let cx = rect.center().x;
    let stand_bottom = stand_top + h * 0.16;
    ui.painter()
        .line_segment([egui::pos2(cx, stand_top), egui::pos2(cx, stand_bottom)], stroke);
    let base_w = w * 0.32;
    ui.painter().line_segment(
        [
            egui::pos2(cx - base_w / 2.0, stand_bottom),
            egui::pos2(cx + base_w / 2.0, stand_bottom),
        ],
        stroke,
    );
}
