//! The viewing screen's stats panel: frame rate, bitrate and resolution in
//! each direction, plus round-trip time, session length and whether the
//! connection is direct or relayed.
//!
//! The media threads only keep running totals (see
//! [`crate::transport::SessionStats`]); [`Sampler`] turns those into rates
//! here, at a pace the panel can actually be read at.

use crate::transport::{SessionStats, SharedConnState};
use crate::viewer::{theme, widgets};
use egui::{RichText, Ui};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often the rates are recomputed. Faster than this and the numbers
/// flicker between frames' worth of noise instead of saying anything.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub const PANEL_WIDTH: f32 = 232.0;

#[derive(Clone, Copy, Default)]
pub struct Rates {
    pub rx_fps: f64,
    pub rx_bits_per_sec: f64,
    pub tx_fps: f64,
    pub tx_bits_per_sec: f64,
}

#[derive(Clone, Copy)]
struct Totals {
    at: Instant,
    frames_rx: u64,
    bytes_rx: u64,
    frames_tx: u64,
    bytes_tx: u64,
}

impl Totals {
    fn read(stats: &SessionStats) -> Self {
        Self {
            at: Instant::now(),
            frames_rx: stats.frames_received.load(Ordering::Relaxed),
            bytes_rx: stats.bytes_received.load(Ordering::Relaxed),
            frames_tx: stats.frames_sent.load(Ordering::Relaxed),
            bytes_tx: stats.bytes_sent.load(Ordering::Relaxed),
        }
    }
}

/// Turns [`SessionStats`] totals into per-second rates.
#[derive(Default)]
pub struct Sampler {
    last: Option<Totals>,
    rates: Rates,
}

impl Sampler {
    /// The current rates, re-sampled if the last sample is old enough.
    pub fn update(&mut self, stats: &SessionStats) -> Rates {
        let now = Totals::read(stats);
        match self.last {
            None => self.last = Some(now),
            Some(last) => {
                let secs = now.at.duration_since(last.at).as_secs_f64();
                if secs >= SAMPLE_INTERVAL.as_secs_f64() {
                    let per_sec = |a: u64, b: u64| a.saturating_sub(b) as f64 / secs;
                    self.rates = Rates {
                        rx_fps: per_sec(now.frames_rx, last.frames_rx),
                        rx_bits_per_sec: per_sec(now.bytes_rx, last.bytes_rx) * 8.0,
                        tx_fps: per_sec(now.frames_tx, last.frames_tx),
                        tx_bits_per_sec: per_sec(now.bytes_tx, last.bytes_tx) * 8.0,
                    };
                    self.last = Some(now);
                }
            }
        }
        self.rates
    }
}

/// Everything the panel shows that doesn't live in [`SessionStats`].
pub struct PanelContext<'a> {
    pub stats: &'a SessionStats,
    pub conn_state: &'a SharedConnState,
    pub rates: Rates,
    pub duration: Duration,
    pub paused: bool,
}

pub fn show_panel(ui: &mut Ui, cx: &PanelContext) {
    let p = theme::palette_for(ui);
    let unknown = "\u{2014}"; // —

    let path = match cx.conn_state.get_ice_info() {
        Some(ice) if ice.relay => "Relayed (TURN)".to_string(),
        Some(ice) => format!("Direct ({})", ice.local_type),
        None => unknown.to_string(),
    };
    let rtt = cx
        .stats
        .rtt()
        .map_or_else(|| unknown.to_string(), |d| format!("{} ms", d.as_millis()));

    section(ui, "Session", &[
        ("Duration", format_duration(cx.duration)),
        ("Path", path),
        ("Round trip", rtt),
    ]);

    ui.add_space(theme::space::LG);
    section(ui, "Receiving", &[
        ("Resolution", format_size(cx.stats.remote_size())),
        ("Frame rate", format_fps(cx.rates.rx_fps)),
        ("Bitrate", format_bitrate(cx.rates.rx_bits_per_sec)),
    ]);

    ui.add_space(theme::space::LG);
    if cx.paused {
        widgets::section_header(ui, "Sending");
        ui.add_space(theme::space::XS);
        ui.colored_label(p.ink_3, "Paused \u{2014} nothing is being sent.");
    } else {
        section(ui, "Sending", &[
            ("Resolution", format_size(cx.stats.local_size())),
            ("Frame rate", format_fps(cx.rates.tx_fps)),
            ("Bitrate", format_bitrate(cx.rates.tx_bits_per_sec)),
        ]);
    }
}

fn section(ui: &mut Ui, title: &str, rows: &[(&str, String)]) {
    let p = theme::palette_for(ui);
    widgets::section_header(ui, title);
    ui.add_space(theme::space::XS);
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([theme::space::MD, theme::space::XS])
        .show(ui, |ui| {
            for (label, value) in rows {
                ui.colored_label(p.ink_3, *label);
                ui.label(RichText::new(value).color(p.ink));
                ui.end_row();
            }
        });
}

/// `0:07`, `4:12`, `1:04:12`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn format_bitrate(bits_per_sec: f64) -> String {
    if bits_per_sec >= 1_000_000.0 {
        format!("{:.1} Mbps", bits_per_sec / 1_000_000.0)
    } else {
        format!("{:.0} kbps", bits_per_sec / 1_000.0)
    }
}

fn format_fps(fps: f64) -> String {
    format!("{fps:.0} fps")
}

fn format_size(size: Option<(u32, u32)>) -> String {
    size.map_or_else(|| "\u{2014}".to_string(), |(w, h)| format!("{w}\u{00D7}{h}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_grow_an_hours_field_only_when_needed() {
        assert_eq!(format_duration(Duration::from_secs(7)), "0:07");
        assert_eq!(format_duration(Duration::from_secs(252)), "4:12");
        assert_eq!(format_duration(Duration::from_secs(3852)), "1:04:12");
    }

    #[test]
    fn bitrates_switch_units_at_one_megabit() {
        assert_eq!(format_bitrate(820_000.0), "820 kbps");
        assert_eq!(format_bitrate(2_430_000.0), "2.4 Mbps");
    }

    #[test]
    fn sizes_use_a_multiplication_sign() {
        assert_eq!(format_size(Some((1280, 720))), "1280\u{00D7}720");
        assert_eq!(format_size(None), "\u{2014}");
    }
}
