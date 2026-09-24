//! User settings — what used to be hard-coded (1280x720 capture, a `const`
//! signaling server) or simply missing (theme, start minimised) — and the
//! modal that edits them. Persisted with the rest of the app's state in
//! `App::save`.

use crate::app::Quality;
use crate::viewer::{theme, widgets};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER_URL: &str = "https://screenshare.aghj.dev";

const PANEL_WIDTH: f32 = 440.0;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    P720,
    P1080,
    P1440,
}

impl Resolution {
    const ALL: [Resolution; 3] = [Resolution::P720, Resolution::P1080, Resolution::P1440];

    fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
        }
    }

    fn max_size(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
            Resolution::P1440 => (2560, 1440),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameRate {
    Fps15,
    Fps30,
    Fps60,
}

impl FrameRate {
    const ALL: [FrameRate; 3] = [FrameRate::Fps15, FrameRate::Fps30, FrameRate::Fps60];

    fn fps(self) -> u32 {
        match self {
            FrameRate::Fps15 => 15,
            FrameRate::Fps30 => 30,
            FrameRate::Fps60 => 60,
        }
    }
}

/// Which H.264 encoder and decoder a session uses. Automatic tries the GPU
/// (the hardware encoder; Media Foundation's decoder, on DXVA where it can)
/// and falls back to OpenH264 if they won't start or fail; see phases 4 and
/// 5 of `docs/hardware-encoding-plan.md`. Stored as `encoder`, its name from
/// before it covered decoding too.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncoderChoice {
    Automatic,
    SoftwareOnly,
}

impl EncoderChoice {
    const ALL: [EncoderChoice; 2] = [EncoderChoice::Automatic, EncoderChoice::SoftwareOnly];

    fn label(self) -> &'static str {
        match self {
            EncoderChoice::Automatic => "Automatic",
            EncoderChoice::SoftwareOnly => "Software only",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "Match Windows",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }

    pub fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub resolution: Resolution,
    pub frame_rate: FrameRate,
    pub encoder: EncoderChoice,
    pub theme: ThemeChoice,
    pub start_minimized: bool,
    /// Empty means [`DEFAULT_SERVER_URL`], so a user who never touches it
    /// follows the default if it ever moves.
    pub server_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution: Resolution::P720,
            frame_rate: FrameRate::Fps30,
            encoder: EncoderChoice::Automatic,
            theme: ThemeChoice::System,
            start_minimized: false,
            server_url: String::new(),
        }
    }
}

impl Settings {
    pub fn server_url(&self) -> String {
        let custom = self.server_url.trim().trim_end_matches('/');
        if custom.is_empty() {
            DEFAULT_SERVER_URL.to_string()
        } else {
            custom.to_string()
        }
    }

    pub fn quality(&self) -> Quality {
        let (max_width, max_height) = self.resolution.max_size();
        Quality {
            max_width,
            max_height,
            fps: self.frame_rate.fps(),
            hardware_video: self.encoder == EncoderChoice::Automatic,
        }
    }
}

/// Why a server URL can't be used, or `None` if it can. Blank is fine — it
/// means the default.
fn server_url_problem(url: &str) -> Option<&'static str> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return Some("Start with https:// (or http:// for a local server)");
    };
    if rest.trim_end_matches('/').is_empty() || rest.contains(char::is_whitespace) {
        return Some("That doesn't look like a server address");
    }
    None
}

/// The settings modal. Edits a draft, so Cancel (or Escape, or a click
/// outside) really does leave everything as it was.
#[derive(Default)]
pub struct SettingsPanel {
    draft: Option<Settings>,
}

impl SettingsPanel {
    pub fn open(&mut self, current: &Settings) {
        self.draft = Some(current.clone());
    }

    /// Draws the modal while open. Returns the new settings on Save.
    /// `session_active` adds a note that some changes wait for the next
    /// session, since a running one has its server and quality baked in.
    pub fn show(&mut self, ctx: &egui::Context, session_active: bool) -> Option<Settings> {
        let draft = self.draft.as_mut()?;
        let mut saved = None;
        let mut close = false;

        let modal = egui::Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
            ui.set_width(PANEL_WIDTH);
            let p = theme::palette_for(ui);
            ui.heading("Settings");
            ui.add_space(theme::space::MD);

            widgets::section_header(ui, "What you send");
            ui.colored_label(
                p.ink_3,
                "Higher settings look sharper but need a faster connection at both ends.",
            );
            ui.add_space(theme::space::XS);
            egui::Grid::new("settings_quality")
                .num_columns(2)
                .spacing([theme::space::LG, theme::space::SM])
                .show(ui, |ui| {
                    ui.label("Resolution");
                    ui.horizontal(|ui| {
                        for r in Resolution::ALL {
                            if widgets::toggle_button(ui, draft.resolution == r, r.label()).clicked() {
                                draft.resolution = r;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Frame rate");
                    ui.horizontal(|ui| {
                        for f in FrameRate::ALL {
                            let label = format!("{} fps", f.fps());
                            if widgets::toggle_button(ui, draft.frame_rate == f, label).clicked() {
                                draft.frame_rate = f;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Video codec");
                    ui.horizontal(|ui| {
                        for e in EncoderChoice::ALL {
                            if widgets::toggle_button(ui, draft.encoder == e, e.label()).clicked() {
                                draft.encoder = e;
                            }
                        }
                    });
                    ui.end_row();
                });
            ui.colored_label(
                p.ink_3,
                "Automatic encodes and decodes on your graphics card when it can, and falls back to software if it can't.",
            );

            ui.add_space(theme::space::LG);
            widgets::section_header(ui, "Appearance");
            ui.add_space(theme::space::XS);
            ui.horizontal(|ui| {
                for t in ThemeChoice::ALL {
                    if widgets::toggle_button(ui, draft.theme == t, t.label()).clicked() {
                        draft.theme = t;
                    }
                }
            });
            ui.checkbox(&mut draft.start_minimized, "Start minimised");

            ui.add_space(theme::space::LG);
            widgets::section_header(ui, "Signaling server");
            ui.add_space(theme::space::XS);
            ui.add(
                egui::TextEdit::singleline(&mut draft.server_url)
                    .hint_text(DEFAULT_SERVER_URL)
                    .desired_width(f32::INFINITY),
            );
            let problem = server_url_problem(&draft.server_url);
            match problem {
                Some(problem) => {
                    ui.colored_label(p.failed, problem);
                }
                None => {
                    ui.colored_label(
                        p.ink_3,
                        "You and your peer must use the same server. Leave blank for the default.",
                    );
                }
            }

            if session_active {
                ui.add_space(theme::space::SM);
                widgets::status_pill(
                    ui,
                    widgets::StatusKind::Neutral,
                    "Quality and server changes apply from your next session",
                );
            }

            ui.add_space(theme::space::LG);
            ui.horizontal(|ui| {
                if widgets::primary_button(ui, problem.is_none(), "Save").clicked() {
                    saved = Some(draft.clone());
                }
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    close = true;
                }
            });
        });

        if saved.is_some() || close || modal.should_close() {
            self.draft = None;
        }
        saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_server_means_the_default() {
        let s = Settings::default();
        assert_eq!(s.server_url(), DEFAULT_SERVER_URL);
        assert_eq!(server_url_problem("  "), None);
    }

    #[test]
    fn custom_server_loses_its_trailing_slash() {
        let s = Settings {
            server_url: " http://localhost:8787/ ".into(),
            ..Settings::default()
        };
        assert_eq!(s.server_url(), "http://localhost:8787");
    }

    #[test]
    fn server_urls_need_a_scheme_and_a_host() {
        assert!(server_url_problem("screenshare.example.com").is_some());
        assert!(server_url_problem("https://").is_some());
        assert!(server_url_problem("https://my server").is_some());
        assert_eq!(server_url_problem("https://screenshare.example.com"), None);
    }

    #[test]
    fn default_quality_matches_the_old_hard_coded_capture() {
        let q = Settings::default().quality();
        assert_eq!((q.max_width, q.max_height, q.fps), (1280, 720, 30));
    }

    #[test]
    fn hardware_video_is_on_unless_software_only() {
        assert!(Settings::default().quality().hardware_video);
        let s = Settings {
            encoder: EncoderChoice::SoftwareOnly,
            ..Settings::default()
        };
        assert!(!s.quality().hardware_video);
    }

    #[test]
    fn settings_saved_before_the_encoder_choice_default_to_automatic() {
        let s: Settings = serde_json::from_str(r#"{"resolution":"P1080","frame_rate":"Fps60"}"#).unwrap();
        assert!(s.encoder == EncoderChoice::Automatic);
    }
}
