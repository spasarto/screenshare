mod log_drawer;
mod picker_grid;
pub mod settings;
mod stats;
mod theme;
mod widgets;

use crate::CliArgs;
use crate::app::{Repaint, SessionEvent, SessionHandle, Stage};
use crate::capture::{CaptureTarget, picker};
use crate::failure::{Recovery, SessionFailure};
use crate::logging::LogBuffer;
use crate::signaling_server::SignalingServer;
use crate::transport::ConnState;
use crate::transport::peer::MAX_RECONNECT_ATTEMPTS;
use egui::{Rect, TextureHandle, Vec2};
use log_drawer::LogDrawer;
use picker_grid::{PickerGrid, SavedTarget};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsPanel};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use widgets::{QrCache, StatusKind, StepState};

/// Fixed width of the centred column every screen lays its content out in,
/// so the logo, fields and buttons all share one axis.
const COLUMN_WIDTH: f32 = 420.0;

/// Wider column used only by the two setup steps that need room for a
/// two-card choice or a tile grid.
const WIDE_COLUMN_WIDTH: f32 = 640.0;

/// How long a "Copied" confirmation stays up after a copy.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// Side length of the QR code on the waiting screen. Big enough that a phone
/// held in front of the monitor resolves the modules; small enough to sit
/// beside the link and the share preview without pushing Cancel off screen.
const QR_SIZE: f32 = 112.0;

/// In fullscreen, how long the pointer has to sit still before it's hidden.
const CURSOR_IDLE_HIDE: Duration = Duration::from_millis(2500);

/// In fullscreen, how close to the top or bottom edge the pointer has to get
/// to bring that edge's bar back.
const EDGE_REVEAL: f32 = 6.0;

/// Once a fullscreen bar is out, how far past it the pointer can stray
/// before it tucks away again, so a slightly loose hand doesn't flicker it.
const EDGE_HOLD_SLACK: f32 = 24.0;

/// How long "Waiting for your peer's screen" waits before offering reasons.
const NO_FRAMES_HINT_AFTER: Duration = Duration::from_secs(15);

/// Repaint cadence while viewing with nothing arriving: enough for the
/// duration counter, stats and reconnect timer. Frames request their own
/// repaints as they land.
const VIEWING_TICK: Duration = Duration::from_millis(500);

/// Size of the outgoing-frame still in the sharing bar.
const SHARING_THUMB: Vec2 = Vec2::new(64.0, 36.0);

/// Width of the cards laid over the remote screen (session ended, waiting).
const OVERLAY_WIDTH: f32 = 360.0;

/// Height the error screen's Details text scrolls within.
const DETAILS_MAX_HEIGHT: f32 = 96.0;

/// Upper bound on how long closing the window waits for the peer to be told.
/// Slightly over the session thread's own goodbye timeout.
const EXIT_GOODBYE_WAIT: Duration = Duration::from_millis(600);

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Role {
    Create,
    Join,
}

/// What survives a restart, beyond the window's size and position (which
/// eframe saves itself). Stored as one value so adding a field later reads
/// old saves with that field at its default.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    settings: Settings,
    last_role: Option<Role>,
    last_target: Option<SavedTarget>,
    /// Set once a session has connected, which means the Windows Firewall
    /// prompt (if any) has been and gone.
    network_notice_seen: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SetupStep {
    /// "Start a session" vs. "Join a session" — the fork the old UI never
    /// named, even though both sides always end up sharing a window.
    ChooseIntent,
    /// Pick what to share (and, when joining, enter the peer's code).
    PickTarget,
}

/// Which of the two copyable things on the waiting screen was last copied,
/// so the confirmation appears next to the button that was actually pressed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Copied {
    Code,
    Link,
}

/// What a session is capturing, fixed when it starts. Kept apart from the
/// picker, whose selection can move when its list refreshes — Reconnect has
/// to share the same thing again, not whatever tile is selected by then.
#[derive(Clone)]
struct ActiveShare {
    target: CaptureTarget,
    label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Zoom {
    /// Scale the remote screen to fit the window.
    Fit,
    /// One remote pixel per physical screen pixel, scrolling if it's bigger.
    Actual,
}

/// View state for the Viewing phase. Zoom and the stats toggle carry over
/// between sessions; the rest restarts with each one.
struct ViewerState {
    zoom: Zoom,
    show_stats: bool,
    fullscreen: bool,
    connected_at: Instant,
    /// Last pointer or key input, for hiding the cursor in fullscreen.
    last_activity: Instant,
    /// The toolbar and sharing bar while fullscreen, where each floats over
    /// the screen only while the pointer is at its edge.
    top_bar: EdgeBar,
    bottom_bar: EdgeBar,
    sampler: stats::Sampler,
    outgoing_preview: Option<TextureHandle>,
}

/// A fullscreen bar that slides in when the pointer reaches its edge of the
/// screen and tucks away once the pointer leaves it.
#[derive(Clone, Copy)]
struct EdgeBar {
    shown: bool,
    /// Height as last drawn, so the hold zone matches the bar.
    height: f32,
}

impl EdgeBar {
    const HIDDEN: Self = Self { shown: false, height: 44.0 };

    /// `from_edge` is how far the pointer is from this bar's screen edge, or
    /// `None` when it's off the window.
    fn update(&mut self, from_edge: Option<f32>) {
        self.shown = from_edge.is_some_and(|d| {
            d <= EDGE_REVEAL || (self.shown && d <= self.height + EDGE_HOLD_SLACK)
        });
    }
}

impl ViewerState {
    fn new() -> Self {
        Self {
            zoom: Zoom::Fit,
            show_stats: false,
            fullscreen: false,
            connected_at: Instant::now(),
            last_activity: Instant::now(),
            top_bar: EdgeBar::HIDDEN,
            bottom_bar: EdgeBar::HIDDEN,
            sampler: stats::Sampler::default(),
            outgoing_preview: None,
        }
    }

    fn start_session(&mut self) {
        self.connected_at = Instant::now();
        self.last_activity = Instant::now();
        self.sampler = stats::Sampler::default();
        self.outgoing_preview = None;
    }
}

/// Everything the viewing screen's controls can ask for, applied after the
/// frame is drawn so the drawing code only ever needs to read `App`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewerAction {
    Disconnect,
    Reconnect,
    SetFullscreen(bool),
    SetZoom(Zoom),
    ToggleStats,
    SetPaused(bool),
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Setup,
    WaitingForProcess,
    CreatingSession,
    WaitingForPeer,
    Joining,
    Viewing,
    Error,
}

pub struct App {
    phase: Phase,

    // Setup
    setup_step: SetupStep,
    /// True when the role came from `--server`/`--client` rather than the
    /// ChooseIntent screen — in that case there's no intent screen to go
    /// "back" to.
    role_fixed_by_cli: bool,
    picker: PickerGrid,
    role: Role,
    code_digits: String,
    code_letters: String,
    /// The offerer's own generated code (for display on WaitingForPeer) or a
    /// CLI-supplied `--pin`. Not the same as the digits/letters the user is
    /// actively typing into the Join fields.
    session_code: String,

    // CLI auto-start
    cli_process: Option<String>,
    cli_pin: Option<String>,
    last_process_poll: Option<std::time::Instant>,

    // Background communication
    event_rx: Option<std::sync::mpsc::Receiver<SessionEvent>>,
    /// How far the in-flight session has got through the handshake. Drives
    /// the step list on the three "please wait" screens.
    stage: Stage,

    // Handoff (WaitingForPeer)
    qr: QrCache,
    copied: Option<(Copied, Instant)>,

    /// What the in-flight or live session is capturing.
    active_share: Option<ActiveShare>,

    // Viewing
    session: Option<SessionHandle>,
    texture: Option<TextureHandle>,
    viewer: ViewerState,

    /// For waking the GUI from the decoder thread.
    egui_ctx: egui::Context,

    // Logo
    logo_texture: Option<TextureHandle>,

    // Cleanup: set when we are the offerer and have a session code
    hosted_session_code: Option<String>,

    // Set while a background session thread is running (CreatingSession,
    // WaitingForPeer, Joining, Viewing). Sending `true` tells it to give up,
    // or hang up if already connected.
    session_cancel: Option<watch::Sender<bool>>,

    // Error
    failure: Option<SessionFailure>,

    // Settings, persistence and the log
    settings: Settings,
    settings_panel: SettingsPanel,
    last_role: Option<Role>,
    last_target: Option<SavedTarget>,
    network_notice_seen: bool,
    log: LogBuffer,
    log_drawer: LogDrawer,
    /// The server the in-flight or live session is using, fixed when it
    /// starts: settings can change under a running session, and cleanup and
    /// the share link have to go to the server the code actually lives on.
    session_server_url: String,
    /// The code the last hosted session asked for (`--pin`, or the previous
    /// code on Reconnect), so Try again asks for the same one.
    last_create_pin: Option<String>,
    /// Closing while fullscreen leaves fullscreen first, so the window size
    /// eframe saves on exit is the windowed one.
    close_when_windowed: bool,
}

impl App {
    pub fn new(args: CliArgs, log: LogBuffer, cc: &eframe::CreationContext) -> Self {
        let persisted: Persisted = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, eframe::APP_KEY))
            .unwrap_or_default();

        theme::install(&cc.egui_ctx);
        cc.egui_ctx.set_theme(persisted.settings.theme.preference());
        if persisted.settings.start_minimized {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        // Determine role from CLI flags, defaulting to Create
        let role_fixed_by_cli = args.server || args.client;
        let role = if args.client {
            Role::Join
        } else {
            Role::Create
        };

        let mut picker = PickerGrid::new();
        if let Some(target) = &persisted.last_target {
            picker.restore(target);
        }

        // Pre-fill the code fields from --pin for client mode, split into
        // the same digits/letters groups the Join screen edits.
        let (code_digits, code_letters) = match (args.client, &args.pin) {
            (true, Some(pin)) if pin.len() >= 6 => {
                (pin[..6].to_string(), pin[6..].to_string())
            }
            (true, Some(pin)) => (pin.clone(), String::new()),
            _ => (String::new(), String::new()),
        };

        // Decide starting phase/step: if we have a role flag + process, wait
        // for the process; if we just have a role flag, skip straight past
        // the "what do you want to do" screen since the CLI already answered it.
        let has_process = args.process.is_some();
        let phase = if role_fixed_by_cli && has_process {
            Phase::WaitingForProcess
        } else {
            Phase::Setup
        };
        let setup_step = if role_fixed_by_cli {
            SetupStep::PickTarget
        } else {
            SetupStep::ChooseIntent
        };

        let logo_texture = {
            let bytes = include_bytes!("../logo.png");
            let img = image::load_from_memory(bytes)
                .expect("Failed to load logo.png")
                .to_rgba8();
            let (w, h) = img.dimensions();
            let color_image =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
            Some(
                cc.egui_ctx
                    .load_texture("logo", color_image, egui::TextureOptions::LINEAR),
            )
        };

        Self {
            phase,
            setup_step,
            role_fixed_by_cli,
            picker,
            role,
            code_digits,
            code_letters,
            session_code: String::new(),
            cli_process: args.process,
            cli_pin: if args.server { args.pin } else { None },
            last_process_poll: None,
            event_rx: None,
            stage: Stage::ContactingServer,
            qr: QrCache::default(),
            copied: None,
            active_share: None,
            session: None,
            texture: None,
            viewer: ViewerState::new(),
            egui_ctx: cc.egui_ctx.clone(),
            logo_texture,
            hosted_session_code: None,
            session_cancel: None,
            failure: None,
            session_server_url: persisted.settings.server_url(),
            settings: persisted.settings,
            settings_panel: SettingsPanel::default(),
            last_role: persisted.last_role,
            last_target: persisted.last_target,
            network_notice_seen: persisted.network_notice_seen,
            log,
            log_drawer: LogDrawer::default(),
            last_create_pin: None,
            close_when_windowed: false,
        }
    }

    /// The code the user has typed into the Join fields, normalised and
    /// combined (digits then letters, matching the wire format).
    fn join_code(&self) -> String {
        format!("{}{}", self.code_digits, self.code_letters)
    }

    /// Runs [`normalize_code`] over the two code-entry fields, every frame.
    fn normalize_code_fields(&mut self) {
        let (digits, letters) = normalize_code(&self.code_digits, &self.code_letters);
        self.code_digits = digits;
        self.code_letters = letters;
    }

    fn can_start(&self) -> bool {
        self.picker.selected_capture_target().is_some()
            && (self.role == Role::Create || crate::is_valid_pin(&self.join_code()))
    }

    /// Explains why Start is disabled (or nothing, once it isn't) instead of
    /// leaving a greyed-out button to speak for itself.
    fn start_hint(&self) -> Option<String> {
        if self.picker.selected_capture_target().is_none() {
            return Some("Select something to share".to_string());
        }
        if self.role == Role::Join {
            let typed = self.code_digits.chars().count() + self.code_letters.chars().count();
            let remaining = 10usize.saturating_sub(typed);
            if remaining > 0 {
                let plural = if remaining == 1 { "" } else { "s" };
                return Some(format!("{remaining} more character{plural} needed"));
            }
        }
        None
    }

    /// Abandon whatever session is in flight or live (if any) and return to
    /// the setup screen. Safe to call from any phase.
    fn cancel_session(&mut self) {
        self.end_session();
        self.active_share = None;
        self.set_fullscreen(false);
        self.picker.refresh_list();
        self.phase = Phase::Setup;
        self.setup_step = SetupStep::PickTarget;
    }

    /// Tells the session thread to hang up (sending the peer a goodbye if
    /// connected) and drops everything the GUI held for it. Leaves the phase
    /// alone, so Reconnect can go straight into a new session.
    fn end_session(&mut self) {
        if let Some(tx) = self.session_cancel.take() {
            let _ = tx.send(true);
        }
        if let Some(code) = self.hosted_session_code.take() {
            let server = SignalingServer::new(&self.session_server_url);
            std::thread::spawn(move || server.delete_session_blocking(&code));
        }
        self.event_rx = None;
        self.failure = None;
        self.stage = Stage::ContactingServer;
        self.copied = None;
        self.session = None;
        self.texture = None;
    }

    /// Starts a new session the same way round as the one that just ended:
    /// the host re-creates its code, the joiner looks it up again, so each
    /// side pressing Reconnect meets the other without anyone retyping.
    /// Falls back to the picker if the shared window has since closed.
    fn reconnect(&mut self) {
        let Some(share) = self.active_share.clone() else {
            self.cancel_session();
            return;
        };
        if let CaptureTarget::Window(window) = share.target
            && !window.is_valid()
        {
            self.cancel_session();
            return;
        }
        let code = self.session_code.clone();
        self.end_session();
        let create_pin = (self.role == Role::Create && crate::is_valid_pin(&code)).then_some(code);
        self.spawn_session(share, create_pin);
    }

    /// Runs the failed session again, the same way round and sharing the
    /// same thing. `fresh_code` drops the code a host asked for, for the
    /// failures where that code is the problem.
    fn retry(&mut self, fresh_code: bool) {
        let Some(share) = self.active_share.clone() else {
            self.cancel_session();
            return;
        };
        if let CaptureTarget::Window(window) = share.target
            && !window.is_valid()
        {
            self.cancel_session();
            return;
        }
        let pin = if fresh_code { None } else { self.last_create_pin.clone() };
        // A code still held on the server has to go before it can be
        // created again; `end_session`'s cleanup runs in the background and
        // would race the new session's create.
        if let (Some(pin), Some(hosted)) = (&pin, &self.hosted_session_code)
            && pin == hosted
        {
            SignalingServer::new(&self.session_server_url).delete_session_blocking(hosted);
            self.hosted_session_code = None;
        }
        self.end_session();
        self.spawn_session(share, pin);
    }

    fn apply_recovery(&mut self, recovery: Recovery) {
        match recovery {
            Recovery::Retry => self.retry(false),
            Recovery::NewCode => self.retry(true),
            // The code fields are left as they were, so going back to the
            // Join screen is checking the code.
            Recovery::CheckCode | Recovery::PickAnother => self.cancel_session(),
            Recovery::OpenSettings => self.settings_panel.open(&self.settings),
        }
    }

    fn apply_settings(&mut self, settings: Settings) {
        self.egui_ctx.set_theme(settings.theme.preference());
        self.settings = settings;
    }

    fn set_fullscreen(&mut self, fullscreen: bool) {
        if self.viewer.fullscreen != fullscreen {
            self.viewer.fullscreen = fullscreen;
            self.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
        }
    }

    fn process_events(&mut self) {
        let Some(rx) = &self.event_rx else { return };

        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::Progress(stage) => {
                    self.stage = stage;
                }
                SessionEvent::SessionCode(code) => {
                    self.hosted_session_code = Some(code.clone());
                    self.session_code = code;
                    self.phase = Phase::WaitingForPeer;
                }
                SessionEvent::SessionReady(session) => {
                    // The session thread deletes the server-side session
                    // once connected; nothing left for us to clean up.
                    self.hosted_session_code = None;
                    self.session = Some(session);
                    self.texture = None;
                    self.viewer.start_session();
                    self.network_notice_seen = true;
                    self.phase = Phase::Viewing;
                }
                SessionEvent::Error(failure) => {
                    self.failure = Some(failure);
                    self.phase = Phase::Error;
                }
            }
        }
    }

    fn show_logo(&self, ui: &mut egui::Ui) {
        if let Some(tex) = &self.logo_texture {
            let size = egui::vec2(72.0, 72.0);
            ui.image(egui::load::SizedTexture::new(tex.id(), size));
        }
    }

    /// The logo, at the same vertical offset on every screen that shows it.
    fn show_logo_header(&self, ui: &mut egui::Ui) {
        ui.add_space(theme::space::XXL);
        ui.vertical_centered(|ui| self.show_logo(ui));
        ui.add_space(theme::space::XL);
    }

    fn show_setup(&mut self, ui: &mut egui::Ui) {
        match self.setup_step {
            SetupStep::ChooseIntent => self.show_choose_intent(ui),
            SetupStep::PickTarget => self.show_pick_target(ui),
        }
    }

    fn show_choose_intent(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, WIDE_COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);

                ui.vertical_centered(|ui| {
                    let p = theme::palette_for(ui);
                    ui.colored_label(
                        p.ink_2,
                        "Both sides share their screen — pick where you're starting.",
                    );
                });
                ui.add_space(theme::space::LG);

                ui.columns(2, |columns| {
                    if widgets::choice_card(
                        &mut columns[0],
                        "Start a session",
                        "Pick something to share and get a code to send your peer. \
                         They'll share their screen with you too.",
                        self.last_role == Some(Role::Create),
                    )
                    .clicked()
                    {
                        self.role = Role::Create;
                        self.setup_step = SetupStep::PickTarget;
                    }

                    if widgets::choice_card(
                        &mut columns[1],
                        "Join a session",
                        "Enter a code you were given, then pick what you'll share back.",
                        self.last_role == Some(Role::Join),
                    )
                    .clicked()
                    {
                        self.role = Role::Join;
                        self.setup_step = SetupStep::PickTarget;
                    }
                });
            });
        });
    }

    fn show_pick_target(&mut self, ui: &mut egui::Ui) {
        self.normalize_code_fields();

        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, WIDE_COLUMN_WIDTH, |ui| {
                ui.add_space(theme::space::SM);

                ui.horizontal(|ui| {
                    if !self.role_fixed_by_cli && widgets::secondary_button(ui, "< Back").clicked() {
                        self.setup_step = SetupStep::ChooseIntent;
                    }
                    ui.add_space(theme::space::SM);
                    ui.heading(match self.role {
                        Role::Create => "Start a session",
                        Role::Join => "Join a session",
                    });
                });
                ui.add_space(theme::space::SM);

                widgets::section_header(ui, "What do you want to share?");
                ui.add_space(theme::space::XS);
                // Join also needs room for the code-entry row below the
                // grid, so it gets one fewer tile row than Create.
                let max_tiles = if self.role == Role::Join { 4 } else { 8 };
                self.picker.show(ui, max_tiles);

                if self.role == Role::Join {
                    ui.add_space(theme::space::SM);
                    widgets::section_header(ui, "Session code");
                    ui.add_space(theme::space::XS);
                    self.show_code_entry(ui);
                }

                ui.add_space(theme::space::MD);

                let can_start = self.can_start();
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, can_start, "Start").clicked() {
                        self.handle_start();
                    }
                    if let Some(hint) = self.start_hint() {
                        ui.add_space(theme::space::SM);
                        let p = theme::palette_for(ui);
                        ui.colored_label(p.ink_3, hint);
                    }
                });
            });
        });
    }

    fn show_code_entry(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette_for(ui);
        let letters_id = egui::Id::new("code_letters_field");
        let mut submit = false;
        ui.horizontal(|ui| {
            let digits_before = self.code_digits.chars().count();
            let digits_response = ui.add(
                egui::TextEdit::singleline(&mut self.code_digits)
                    .desired_width(104.0)
                    .font(egui::FontId::monospace(theme::size::LG))
                    .hint_text("000000"),
            );
            // Once all 6 digits are in, jump straight to the letters field
            // rather than making the user reach for Tab or the mouse.
            if digits_response.changed()
                && digits_before < 6
                && self.code_digits.chars().count() >= 6
            {
                ui.memory_mut(|mem| mem.request_focus(letters_id));
            }
            if digits_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.label(egui::RichText::new("\u{00B7}").size(theme::size::LG).color(p.ink_3));
            let letters_response = ui.add(
                egui::TextEdit::singleline(&mut self.code_letters)
                    .id(letters_id)
                    .desired_width(72.0)
                    .font(egui::FontId::monospace(theme::size::LG))
                    .hint_text("aaaa"),
            );
            if letters_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
        });
        if submit && self.can_start() {
            self.handle_start();
        }
    }

    fn handle_start(&mut self) {
        // The Start button is disabled unless `can_start()` holds, so
        // reaching here with no target or an invalid code shouldn't happen;
        // treat it as a no-op rather than surfacing an error for a state
        // the UI already prevents.
        let Some(target) = self.picker.selected_capture_target() else {
            return;
        };
        if self.role == Role::Join && !crate::is_valid_pin(&self.join_code()) {
            return;
        }
        let label = self.picker.selected_label().unwrap_or_default();
        if !self.role_fixed_by_cli {
            self.last_role = Some(self.role);
        }
        self.last_target = self.picker.saved_selection();
        self.spawn_session(ActiveShare { target, label }, self.cli_pin.clone());
    }

    /// Auto-start a session directly with a found window (used by WaitingForProcess phase).
    fn start_with_window(&mut self, info: picker::WindowInfo) {
        // Keep the picker's selection in step, so the progress screens' share
        // preview names the window actually being captured.
        self.picker.select_window(&info.window);
        let share = ActiveShare {
            target: CaptureTarget::Window(info.window),
            label: info.title,
        };
        self.spawn_session(share, self.cli_pin.clone());
    }

    /// Spawns the session thread for the current role. `create_pin` is the
    /// code to host under (from `--pin`, or the previous session's code on
    /// Reconnect); a joiner uses the code in the Join fields.
    fn spawn_session(&mut self, share: ActiveShare, create_pin: Option<String>) {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        self.event_rx = Some(event_rx);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.session_cancel = Some(cancel_tx);
        let ctx = self.egui_ctx.clone();
        let repaint: Repaint = Arc::new(move || ctx.request_repaint());

        let target = share.target;
        self.active_share = Some(share);
        self.stage = Stage::ContactingServer;
        self.failure = None;
        self.session_server_url = self.settings.server_url();
        self.last_create_pin = create_pin.clone();
        let quality = self.settings.quality();
        tracing::info!(
            "Starting session via {} at up to {}x{}, {} fps",
            self.session_server_url,
            quality.max_width,
            quality.max_height,
            quality.fps
        );

        match self.role {
            Role::Create => {
                crate::app::spawn_offerer(
                    self.session_server_url.clone(),
                    target,
                    quality,
                    create_pin,
                    event_tx,
                    cancel_rx,
                    repaint,
                );
                self.phase = Phase::CreatingSession;
            }
            Role::Join => {
                crate::app::spawn_answerer(
                    self.session_server_url.clone(),
                    target,
                    quality,
                    self.join_code(),
                    event_tx,
                    cancel_rx,
                    repaint,
                );
                self.phase = Phase::Joining;
            }
        }
    }

    fn show_creating_session(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);
                ui.heading("Creating session");
                ui.add_space(theme::space::MD);
                widgets::progress_steps(ui, &self.progress_list());
                self.show_network_notice(ui);
                ui.add_space(theme::space::LG);
                self.show_share_preview(ui);
                ui.add_space(theme::space::LG);
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    self.cancel_session();
                }
            });
        });
    }

    /// The step list for the current role and stage, with everything before
    /// the stage in flight marked done.
    fn progress_list(&self) -> [(&'static str, StepState); 4] {
        let labels = match self.role {
            Role::Create => [
                "Contacting the signaling server",
                "Waiting for your peer to join",
                "Negotiating the connection",
                "Connected",
            ],
            Role::Join => [
                "Looking up the session",
                "Waiting for the host",
                "Negotiating the connection",
                "Connected",
            ],
        };
        let current = match self.stage {
            Stage::ContactingServer => 0,
            Stage::WaitingForPeer => 1,
            Stage::Negotiating => 2,
            Stage::Connected => 3,
        };
        std::array::from_fn(|i| {
            let state = match i.cmp(&current) {
                std::cmp::Ordering::Less => StepState::Done,
                std::cmp::Ordering::Equal => StepState::Current,
                std::cmp::Ordering::Greater => StepState::Pending,
            };
            (labels[i], state)
        })
    }

    /// "You're sharing X", on every screen between pressing Start and seeing
    /// the remote screen — this app transmits from both ends, and nothing
    /// used to say so.
    fn show_share_preview(&mut self, ui: &mut egui::Ui) {
        if self.picker.show_selected_preview(ui) {
            // The capture target is baked into the session thread when it
            // spawns, so picking a different one means starting over — which
            // is exactly what cancelling does.
            self.cancel_session();
        }
    }

    /// True while the "Copied" confirmation for `what` should still be up.
    fn copied_recently(&self, what: Copied) -> bool {
        self.copied
            .is_some_and(|(which, at)| which == what && at.elapsed() < COPIED_FOR)
    }

    /// The web link carrying the session code, so the peer can be handed a
    /// URL instead of ten characters to type.
    fn share_link(&self) -> String {
        format!("{}/j/{}", self.session_server_url, self.session_code)
    }

    fn show_waiting_for_process(&mut self, ui: &mut egui::Ui) {
        let process_name = self.cli_process.clone().unwrap_or_default();

        // Poll for the process every ~1 second
        let should_poll = self
            .last_process_poll
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
            .unwrap_or(true);

        if should_poll {
            self.last_process_poll = Some(std::time::Instant::now());
            if let Some(info) = picker::find_window_by_process(&process_name) {
                tracing::info!("Found process '{}': window '{}'", process_name, info.title);
                self.start_with_window(info);
                return;
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.add_space(theme::space::SM);
                    ui.label(format!("Waiting for '{}' to start...", process_name));
                });
                ui.add_space(theme::space::XL);
                ui.vertical_centered(|ui| {
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        self.cancel_session();
                    }
                });
            });
        });
    }

    /// The handoff screen: everything needed to get the session to someone
    /// else — the code, a link, a QR of that link — plus where the handshake
    /// has got to and what is about to be shared.
    fn show_waiting_for_peer(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, WIDE_COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);
                self.show_session_code(ui);
                ui.add_space(theme::space::SM);

                ui.columns(2, |columns| {
                    // Left: the other way to hand the session over, then
                    // where the handshake has got to.
                    self.show_share_link(&mut columns[0]);
                    columns[0].add_space(theme::space::LG);
                    widgets::progress_steps(&mut columns[0], &self.progress_list());

                    // Right: the same link as a QR for a phone, and what is
                    // going out once the peer arrives.
                    let ui = &mut columns[1];
                    let link = self.share_link();
                    self.qr.show(ui, &link, QR_SIZE);
                    let p = theme::palette_for(ui);
                    ui.colored_label(p.ink_3, "Scan to open the link");
                    ui.add_space(theme::space::MD);
                    self.show_share_preview(ui);
                });

                ui.add_space(theme::space::MD);
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    self.cancel_session();
                }
                self.show_network_notice(ui);
            });
        });
    }

    /// Until a session has connected once, warns that Windows is likely to
    /// ask about network access — the prompt used to arrive unannounced,
    /// from an unsigned binary, halfway through setting up a session.
    fn show_network_notice(&self, ui: &mut egui::Ui) {
        if self.network_notice_seen {
            return;
        }
        let p = theme::palette_for(ui);
        ui.add_space(theme::space::SM);
        ui.label(
            egui::RichText::new(
                "The first time you connect, Windows may ask whether Screen Share can use \
                 the network. Allow it, or connecting to your peer may fail or fall back \
                 to a slower relay.",
            )
            .size(theme::size::SM)
            .color(p.ink_2),
        );
    }

    /// Settings and the log, in the top-right corner of every screen but
    /// the viewer (where F10 still opens the log).
    fn show_utility_buttons(&mut self, ctx: &egui::Context) {
        egui::Area::new(egui::Id::new("utility_buttons"))
            .anchor(
                egui::Align2::RIGHT_TOP,
                egui::vec2(-theme::space::SM, theme::space::SM),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let log_label = if self.log_drawer.is_open() { "Hide log" } else { "Log" };
                    if widgets::quiet_button(ui, log_label).on_hover_text("F10").clicked() {
                        self.log_drawer.toggle();
                    }
                    if widgets::quiet_button(ui, "Settings").clicked() {
                        self.settings_panel.open(&self.settings);
                    }
                });
            });
    }

    /// Closing while fullscreen: leave fullscreen first and close on the
    /// next frame, so the size eframe saves for next launch is the
    /// windowed one rather than the whole screen.
    fn handle_close(&mut self, ctx: &egui::Context) {
        let (close_requested, fullscreen) =
            ctx.input(|i| (i.viewport().close_requested(), i.viewport().fullscreen));
        if close_requested && fullscreen == Some(true) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            self.viewer.fullscreen = false;
            self.close_when_windowed = true;
            ctx.request_repaint();
        } else if self.close_when_windowed && fullscreen != Some(true) {
            self.close_when_windowed = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The code itself, grouped so it survives being read across a desk,
    /// with the one-click copy the old screen left the user to do by hand.
    fn show_session_code(&mut self, ui: &mut egui::Ui) {
        widgets::section_header(ui, "Share this code with your peer");
        ui.add_space(theme::space::XS);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(grouped_code(&self.session_code))
                    .size(theme::size::HERO)
                    .monospace()
                    .strong(),
            );
            ui.add_space(theme::space::MD);
            ui.vertical(|ui| {
                // Sit the button against the code's middle rather than its
                // cap height.
                ui.add_space(theme::space::MD);
                if widgets::secondary_button(ui, "Copy code").clicked() {
                    // The separator is a reading aid, not part of the code.
                    ui.ctx().copy_text(self.session_code.clone());
                    self.copied = Some((Copied::Code, Instant::now()));
                }
                if self.copied_recently(Copied::Code) {
                    let p = theme::palette_for(ui);
                    ui.colored_label(p.connected, "Copied");
                }
            });
        });
    }

    fn show_share_link(&mut self, ui: &mut egui::Ui) {
        let link = self.share_link();
        let p = theme::palette_for(ui);

        widgets::section_header(ui, "Or send a link");
        ui.add_space(theme::space::XS);
        ui.add(
            egui::Label::new(egui::RichText::new(display_link(&link)).color(p.ink_2)).truncate(),
        );
        ui.add_space(theme::space::XS);
        ui.horizontal(|ui| {
            if widgets::secondary_button(ui, "Copy link").clicked() {
                ui.ctx().copy_text(link.clone());
                self.copied = Some((Copied::Link, Instant::now()));
            }
            if self.copied_recently(Copied::Link) {
                ui.colored_label(p.connected, "Copied");
            }
        });
    }

    fn show_joining(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);
                ui.heading(format!("Joining {}", grouped_code(&self.join_code())));
                ui.add_space(theme::space::MD);
                widgets::progress_steps(ui, &self.progress_list());
                self.show_network_notice(ui);
                ui.add_space(theme::space::LG);
                self.show_share_preview(ui);
                ui.add_space(theme::space::LG);
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    self.cancel_session();
                }
            });
        });
    }

    fn show_viewing(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(session) = self.session.clone() else {
            return;
        };

        self.upload_frames(&ctx, &session);
        self.track_viewer_input(&ctx);

        let conn = &session.conn_state;
        let state = conn.get();
        let paused = session.is_paused();
        let fullscreen = self.viewer.fullscreen;
        let has_frame = self.texture.is_some();
        // Stops counting once the session has ended.
        let duration = if state == ConnState::Failed {
            conn.since().saturating_duration_since(self.viewer.connected_at)
        } else {
            self.viewer.connected_at.elapsed()
        };

        // In fullscreen the bars float over the screen, each only while the
        // pointer is at its edge — but both stay up while something needs
        // the user's attention.
        let needs_attention = state != ConnState::Connected || !has_frame;
        let (show_top, show_bottom) = if fullscreen {
            let screen = ctx.content_rect();
            let pointer = ctx.input(|i| i.pointer.hover_pos());
            self.viewer.top_bar.update(pointer.map(|p| p.y - screen.top()));
            self.viewer.bottom_bar.update(pointer.map(|p| screen.bottom() - p.y));
            (
                needs_attention || self.viewer.top_bar.shown,
                needs_attention || self.viewer.bottom_bar.shown,
            )
        } else {
            (true, true)
        };

        let idle_for = self.viewer.last_activity.elapsed();
        if fullscreen && !show_top && !show_bottom {
            if idle_for >= CURSOR_IDLE_HIDE {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            } else {
                ctx.request_repaint_after(CURSOR_IDLE_HIDE - idle_for);
            }
        }

        let mut actions: Vec<ViewerAction> = Vec::new();
        let p = theme::palette_for(ui);

        // Docked, the bars push the remote screen aside; that's also how
        // they come back in fullscreen when they're held up, so the banners
        // pinned to the top of the screen stay clear of them.
        if !fullscreen || needs_attention {
            egui::Panel::top("viewer_toolbar").show(ui, |ui| {
                self.show_toolbar(ui, state, duration, &mut actions);
            });
            egui::Panel::bottom("sharing_bar").show(ui, |ui| {
                ui.add_space(theme::space::XS);
                self.show_sharing_bar(ui, state, paused, &mut actions);
                ui.add_space(theme::space::XS);
            });
        } else {
            if show_top {
                self.viewer.top_bar.height =
                    edge_overlay(&ctx, "viewer_toolbar_overlay", egui::Align2::LEFT_TOP, |ui| {
                        self.show_toolbar(ui, state, duration, &mut actions);
                    });
            }
            if show_bottom {
                self.viewer.bottom_bar.height =
                    edge_overlay(&ctx, "sharing_bar_overlay", egui::Align2::LEFT_BOTTOM, |ui| {
                        ui.add_space(theme::space::XS);
                        self.show_sharing_bar(ui, state, paused, &mut actions);
                        ui.add_space(theme::space::XS);
                    });
            } else {
                // The bar is tucked away, but what's going out never is.
                let label = self.sharing_label();
                egui::Area::new(egui::Id::new("sharing_reminder"))
                    .anchor(
                        egui::Align2::LEFT_BOTTOM,
                        egui::vec2(theme::space::MD, -theme::space::MD),
                    )
                    .interactable(false)
                    .show(&ctx, |ui| {
                        if paused {
                            widgets::status_pill(ui, StatusKind::Warning, "Sharing paused");
                        } else {
                            widgets::status_pill(
                                ui,
                                StatusKind::Positive,
                                format!("Sharing {label}"),
                            );
                        }
                    });
            }
        }

        if self.viewer.show_stats {
            let rates = self.viewer.sampler.update(&session.stats);
            egui::Panel::right("stats_panel")
                .resizable(false)
                .exact_size(stats::PANEL_WIDTH)
                .show(ui, |ui| {
                    ui.add_space(theme::space::LG);
                    stats::show_panel(
                        ui,
                        &stats::PanelContext {
                            stats: &session.stats,
                            conn_state: conn,
                            rates,
                            duration,
                            paused,
                        },
                    );
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                let area = ui.max_rect();
                match &self.texture {
                    Some(tex) => self.paint_remote_screen(ui, tex, area),
                    None if state != ConnState::Failed => {
                        self.show_waiting_for_frames(&ctx, area, conn.peer_paused());
                    }
                    None => {}
                }

                match state {
                    ConnState::Failed => {
                        ui.painter().rect_filled(area, 0.0, p.bg.gamma_multiply(0.85));
                        self.show_session_ended(&ctx, area, conn.peer_left(), &mut actions);
                    }
                    ConnState::Reconnecting => {
                        ui.painter().rect_filled(area, 0.0, p.bg.gamma_multiply(0.5));
                        let elapsed = conn.since().elapsed().as_secs();
                        let attempt = conn.reconnect_attempt();
                        let text = if attempt > 0 {
                            format!(
                                "Connection lost \u{2014} reconnecting, attempt {attempt} of \
                                 {MAX_RECONNECT_ATTEMPTS} \u{00B7} {elapsed}s"
                            )
                        } else {
                            format!("Connection lost \u{2014} reconnecting \u{00B7} {elapsed}s")
                        };
                        banner(&ctx, area, StatusKind::Warning, text);
                    }
                    ConnState::Connected if has_frame && conn.peer_paused() => {
                        banner(
                            &ctx,
                            area,
                            StatusKind::Neutral,
                            "Your peer paused sharing \u{2014} this is their last frame",
                        );
                    }
                    _ => {}
                }
            });

        for action in actions {
            match action {
                ViewerAction::Disconnect => self.cancel_session(),
                ViewerAction::Reconnect => self.reconnect(),
                ViewerAction::SetFullscreen(on) => self.set_fullscreen(on),
                ViewerAction::SetZoom(zoom) => self.viewer.zoom = zoom,
                ViewerAction::ToggleStats => self.viewer.show_stats = !self.viewer.show_stats,
                ViewerAction::SetPaused(on) => session.set_paused(on),
            }
        }
    }

    /// Moves the newest remote frame and outgoing still (if either changed)
    /// into their textures.
    fn upload_frames(&mut self, ctx: &egui::Context, session: &SessionHandle) {
        crate::diag::GUI_PAINTS.inc();
        let remote = session.frame_buffer.lock().unwrap().take();
        if let Some(decoded) = remote {
            let upload_start = std::time::Instant::now();
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [decoded.width as usize, decoded.height as usize],
                &decoded.rgba,
            );
            match &mut self.texture {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("remote_screen", image, egui::TextureOptions::LINEAR));
                }
            }
            crate::diag::GUI_UPLOADS.inc();
            crate::diag::GUI_UPLOAD_US.add_time(upload_start.elapsed());
        }

        let outgoing = session.outgoing_preview.lock().unwrap().take();
        if let Some(still) = outgoing {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [still.width as usize, still.height as usize],
                &still.rgba,
            );
            match &mut self.viewer.outgoing_preview {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.viewer.outgoing_preview = Some(ctx.load_texture(
                        "outgoing_preview",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }
    }

    /// Keyboard shortcuts, plus the activity clock that decides when
    /// fullscreen hides the bars.
    fn track_viewer_input(&mut self, ctx: &egui::Context) {
        let (active, f11, escape, os_fullscreen) = ctx.input(|i| {
            let active = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                        | egui::Event::Key { .. }
                )
            });
            (
                active,
                i.key_pressed(egui::Key::F11),
                i.key_pressed(egui::Key::Escape),
                i.viewport().fullscreen,
            )
        });

        // Keep in step if fullscreen changed some way other than our
        // buttons (e.g. the OS), so the next toggle isn't a no-op.
        if let Some(on) = os_fullscreen {
            self.viewer.fullscreen = on;
        }
        if active {
            self.viewer.last_activity = Instant::now();
        }
        if f11 {
            self.set_fullscreen(!self.viewer.fullscreen);
        } else if escape {
            self.set_fullscreen(false);
        }
    }

    fn paint_remote_screen(&self, ui: &mut egui::Ui, tex: &TextureHandle, area: Rect) {
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let tex_size = tex.size_vec2();
        match self.viewer.zoom {
            Zoom::Fit => {
                let scale = (area.width() / tex_size.x).min(area.height() / tex_size.y);
                let rect = Rect::from_center_size(area.center(), tex_size * scale);
                ui.painter().image(tex.id(), rect, uv, egui::Color32::WHITE);
            }
            Zoom::Actual => {
                // Texture pixels are physical; the layout is in points.
                let size = tex_size / ui.ctx().pixels_per_point();
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let (rect, _) = ui.allocate_exact_size(
                            size.max(ui.available_size()),
                            egui::Sense::hover(),
                        );
                        let rect = Rect::from_center_size(rect.center(), size);
                        ui.painter().image(tex.id(), rect, uv, egui::Color32::WHITE);
                    });
            }
        }
    }

    /// Connected, but nothing to draw yet.
    fn show_waiting_for_frames(&self, ctx: &egui::Context, area: Rect, peer_paused: bool) {
        let waited = self.viewer.connected_at.elapsed();
        centered_overlay(ctx, "waiting_for_frames", area, |ui| {
            let p = theme::palette_for(ui);
            if peer_paused {
                ui.label("Your peer has paused sharing.");
                ui.colored_label(p.ink_3, "Their screen will appear here when they resume.");
                return;
            }
            ui.spinner();
            ui.add_space(theme::space::SM);
            ui.label("Waiting for your peer's screen\u{2026}");
            if waited >= NO_FRAMES_HINT_AFTER {
                ui.add_space(theme::space::SM);
                ui.colored_label(
                    p.ink_3,
                    "Nothing has arrived yet. If the window they're sharing is \
                     minimised, Windows sends nothing for it until it's restored.",
                );
            } else {
                ctx.request_repaint_after(NO_FRAMES_HINT_AFTER - waited);
            }
        });
    }

    /// The card over a dimmed last frame once the connection is gone for good.
    fn show_session_ended(
        &self,
        ctx: &egui::Context,
        area: Rect,
        peer_left: bool,
        actions: &mut Vec<ViewerAction>,
    ) {
        let (title, body) = if peer_left {
            ("Your peer ended the session", "They disconnected on their side.")
        } else {
            (
                "The connection was lost",
                "Reconnecting on its own didn't work, so the session has ended.",
            )
        };
        let how = match self.role {
            Role::Create => "Reconnect reopens the same code \u{2014} your peer presses Reconnect too.",
            Role::Join => "Reconnect joins the same code again once your peer reopens it.",
        };

        centered_overlay(ctx, "session_ended", area, |ui| {
            widgets::card(ui, |ui| {
                ui.set_width(OVERLAY_WIDTH);
                let p = theme::palette_for(ui);
                ui.heading(title);
                ui.add_space(theme::space::XS);
                ui.colored_label(p.ink_2, body);
                ui.colored_label(p.ink_3, how);
                ui.add_space(theme::space::MD);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, true, "Reconnect").clicked() {
                        actions.push(ViewerAction::Reconnect);
                    }
                    if widgets::secondary_button(ui, "Back to setup").clicked() {
                        actions.push(ViewerAction::Disconnect);
                    }
                });
            });
        });
    }

    /// The top bar: connection status, session length, and the view and
    /// session controls.
    fn show_toolbar(
        &self,
        ui: &mut egui::Ui,
        state: ConnState,
        duration: Duration,
        actions: &mut Vec<ViewerAction>,
    ) {
        let Some(session) = &self.session else { return };
        let conn = &session.conn_state;
        let fullscreen = self.viewer.fullscreen;
        let p = theme::palette_for(ui);
        ui.add_space(theme::space::XS);
        ui.horizontal(|ui| {
            let (kind, text) = connection_status(conn);
            widgets::status_pill(ui, kind, text);
            if state == ConnState::Connected
                && let Some(ice) = conn.get_ice_info()
            {
                if ice.relay {
                    widgets::status_pill(ui, StatusKind::Neutral, "Relayed (TURN)")
                        .on_hover_text("Traffic goes through a relay server");
                } else {
                    widgets::status_pill(ui, StatusKind::Positive, "Direct")
                        .on_hover_text(format!("Peer-to-peer ({})", ice.local_type));
                }
            }
            ui.colored_label(
                p.ink_3,
                stats::format_duration(duration),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::danger_button(ui, "Disconnect")
                    .on_hover_text("End the session for both of you")
                    .clicked()
                {
                    actions.push(ViewerAction::Disconnect);
                }
                let (label, hint) = if fullscreen {
                    ("Exit fullscreen", "F11 or Esc")
                } else {
                    ("Fullscreen", "F11")
                };
                if widgets::secondary_button(ui, label).on_hover_text(hint).clicked() {
                    actions.push(ViewerAction::SetFullscreen(!fullscreen));
                }
                if widgets::toggle_button(ui, self.viewer.show_stats, "Stats").clicked() {
                    actions.push(ViewerAction::ToggleStats);
                }
                ui.add_space(theme::space::SM);
                // Right-to-left, so "1:1" first reads as "Fit  1:1".
                let zoom = self.viewer.zoom;
                if widgets::toggle_button(ui, zoom == Zoom::Actual, "1:1")
                    .on_hover_text("Actual size: one remote pixel per screen pixel")
                    .clicked()
                {
                    actions.push(ViewerAction::SetZoom(Zoom::Actual));
                }
                if widgets::toggle_button(ui, zoom == Zoom::Fit, "Fit")
                    .on_hover_text("Scale to fit the window")
                    .clicked()
                {
                    actions.push(ViewerAction::SetZoom(Zoom::Fit));
                }
            });
        });
        ui.add_space(theme::space::XS);
    }

    /// The bottom bar: what is going out, as a live still, and the control
    /// to stop it.
    fn show_sharing_bar(
        &self,
        ui: &mut egui::Ui,
        state: ConnState,
        paused: bool,
        actions: &mut Vec<ViewerAction>,
    ) {
        let p = theme::palette_for(ui);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(SHARING_THUMB, egui::Sense::hover());
            ui.painter().rect_filled(rect, 4.0, p.surface_2);
            if let Some(tex) = &self.viewer.outgoing_preview {
                let scale = (rect.width() / tex.size_vec2().x).min(rect.height() / tex.size_vec2().y);
                let draw = Rect::from_center_size(rect.center(), tex.size_vec2() * scale);
                let tint = if paused || state == ConnState::Failed {
                    egui::Color32::from_gray(110)
                } else {
                    egui::Color32::WHITE
                };
                let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(tex.id(), draw, uv, tint);
            }
            ui.add_space(theme::space::XS);

            let header = match (state, paused) {
                (ConnState::Failed, _) => "Stopped sharing",
                (_, true) => "Sharing paused",
                _ => "You're sharing",
            };
            let label = self.sharing_label();

            // Button first, from the right, so the title truncates into
            // whatever is left rather than running under it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if state != ConnState::Failed {
                    if paused {
                        if widgets::primary_button(ui, true, "Resume sharing").clicked() {
                            actions.push(ViewerAction::SetPaused(false));
                        }
                    } else if widgets::secondary_button(ui, "Pause sharing")
                        .on_hover_text("Stop sending your screen without ending the session")
                        .clicked()
                    {
                        actions.push(ViewerAction::SetPaused(true));
                    }
                }
                ui.add_space(theme::space::SM);
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    widgets::section_header(ui, header);
                    ui.add(egui::Label::new(egui::RichText::new(&label).strong()).truncate())
                        .on_hover_text(&label);
                });
            });
        });
    }

    fn sharing_label(&self) -> String {
        self.active_share
            .as_ref()
            .map(|s| s.label.clone())
            .unwrap_or_default()
    }

    /// What went wrong in words, what to do about it as buttons, and the
    /// raw error behind a Details disclosure for when the words aren't
    /// enough.
    fn show_error(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<Recovery> = None;
        let mut back = false;
        let mut open_log = false;

        egui::CentralPanel::default().show(ui, |ui| {
            widgets::centered_column(ui, COLUMN_WIDTH, |ui| {
                self.show_logo_header(ui);
                let p = theme::palette_for(ui);
                let Some(failure) = &self.failure else {
                    ui.heading("Something went wrong");
                    ui.add_space(theme::space::LG);
                    back = widgets::primary_button(ui, true, "Back to setup").clicked();
                    return;
                };

                widgets::status_pill(ui, StatusKind::Negative, "Session failed");
                ui.add_space(theme::space::SM);
                ui.heading(failure.title());
                ui.add_space(theme::space::XS);
                ui.colored_label(p.ink_2, failure.explanation());
                ui.add_space(theme::space::LG);

                let (first, second) = failure.recoveries();
                // Both of these already are "back to setup", just aimed.
                let goes_back =
                    |r: Recovery| matches!(r, Recovery::CheckCode | Recovery::PickAnother);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, true, first.label()).clicked() {
                        chosen = Some(first);
                    }
                    if let Some(second) = second
                        && widgets::secondary_button(ui, second.label()).clicked()
                    {
                        chosen = Some(second);
                    }
                    if !goes_back(first)
                        && !second.is_some_and(goes_back)
                        && widgets::secondary_button(ui, "Back to setup").clicked()
                    {
                        back = true;
                    }
                });

                ui.add_space(theme::space::MD);
                egui::CollapsingHeader::new("Details")
                    .id_salt("error_details")
                    .show(ui, |ui| {
                        // Chains with URLs in them run long; the screen
                        // doesn't scroll, so this part does.
                        egui::ScrollArea::vertical()
                            .max_height(DETAILS_MAX_HEIGHT)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(&failure.details)
                                        .monospace()
                                        .size(theme::size::SM)
                                        .color(p.ink_2),
                                );
                            });
                        ui.add_space(theme::space::XS);
                        ui.horizontal(|ui| {
                            if widgets::secondary_button(ui, "Copy details").clicked() {
                                ui.ctx().copy_text(failure.details.clone());
                            }
                            if widgets::secondary_button(ui, "Open log").clicked() {
                                open_log = true;
                            }
                        });
                    });
            });
        });

        if open_log {
            self.log_drawer.open();
        }
        if let Some(recovery) = chosen {
            self.apply_recovery(recovery);
        } else if back {
            self.cancel_session();
        }
    }
}

/// Keeps the two code-entry fields honest as the user types or pastes:
/// strips anything that isn't alphanumeric, forces lowercase, and re-splits
/// into a digit run followed by a letter run, so pasting the whole code into
/// either box (or typing a stray character into the wrong one) still lands
/// in the right place.
///
/// A paste that *contains* a code — the share link the host sends, most
/// obviously — is reduced to just that code, so the peer can paste the link
/// they were given and never type a character.
fn normalize_code(digits: &str, letters: &str) -> (String, String) {
    let raw: String = format!("{digits}{letters}")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();

    if let Some(code) = extract_code(&raw) {
        return (code[..6].to_string(), code[6..].to_string());
    }

    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).take(6).collect();
    let letters: String = raw
        .chars()
        .skip(digits.len())
        .filter(|c| c.is_ascii_alphabetic())
        .take(4)
        .collect();
    (digits, letters)
}

/// `482917bcdq` -> `482917 \u{00B7} bcdq`. The digits and the letters are two
/// different alphabets; grouping them is what makes the code readable across
/// a desk and repeatable over a phone.
fn grouped_code(code: &str) -> String {
    if crate::is_valid_pin(code) {
        format!("{} \u{00B7} {}", &code[..6], &code[6..])
    } else {
        code.to_string()
    }
}

/// The toolbar's connection pill: what state the link is in, and — while
/// reconnecting — how far through its retries.
fn connection_status(conn: &crate::transport::SharedConnState) -> (StatusKind, String) {
    match conn.get() {
        ConnState::Connecting => (StatusKind::Warning, "Connecting\u{2026}".into()),
        ConnState::Connected => (StatusKind::Positive, "Connected".into()),
        ConnState::Reconnecting => match conn.reconnect_attempt() {
            0 => (StatusKind::Warning, "Reconnecting\u{2026}".into()),
            n => (
                StatusKind::Warning,
                format!("Reconnecting ({n}/{MAX_RECONNECT_ATTEMPTS})"),
            ),
        },
        ConnState::Failed if conn.peer_left() => (StatusKind::Negative, "Peer left".into()),
        ConnState::Failed => (StatusKind::Negative, "Disconnected".into()),
    }
}

/// Lays `add_contents` out centred over `area` (the remote screen), sized
/// to its content. An `Area` rather than a child `Ui`, because egui can't
/// vertically centre a column of widgets inside a panel without knowing
/// their height first.
fn centered_overlay(
    ctx: &egui::Context,
    id: &str,
    area: Rect,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Area::new(egui::Id::new(id))
        .pivot(egui::Align2::CENTER_CENTER)
        .fixed_pos(area.center())
        .show(ctx, |ui| {
            ui.set_max_width(OVERLAY_WIDTH);
            ui.vertical_centered(add_contents);
        });
}

/// A full-width bar floating over one edge of the screen, drawn like a panel
/// but without pushing the remote screen aside. Returns its height.
fn edge_overlay(
    ctx: &egui::Context,
    id: &str,
    edge: egui::Align2,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> f32 {
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .anchor(edge, Vec2::ZERO)
        .show(ctx, |ui| {
            let frame = egui::Frame::side_top_panel(ui.style());
            frame.show(ui, |ui| {
                ui.set_width(screen.width() - frame.total_margin().sum().x);
                add_contents(ui);
            });
            // The divider a docked panel would draw on its inner edge.
            let rect = ui.min_rect();
            let y = if edge.y() == egui::Align::Min { rect.bottom() } else { rect.top() };
            ui.painter().hline(
                rect.x_range(),
                y,
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
        })
        .response
        .rect
        .height()
}

/// A status line pinned to the top of the remote screen.
fn banner(ctx: &egui::Context, area: Rect, kind: StatusKind, text: impl Into<String>) {
    egui::Area::new(egui::Id::new("viewer_banner"))
        .pivot(egui::Align2::CENTER_TOP)
        .fixed_pos(area.center_top() + egui::vec2(0.0, theme::space::MD))
        .interactable(false)
        .show(ctx, |ui| {
            // An Area starts out narrow; without this the one-line status
            // wraps inside its pill.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            widgets::status_pill(ui, kind, text);
        });
}

/// The share link without its scheme — the part worth reading.
fn display_link(link: &str) -> String {
    link.strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"))
        .unwrap_or(link)
        .to_string()
}

/// Finds a whole session code (6 digits then 4 letters) inside an already
/// lowercased, alphanumeric-only string — which is what a pasted share link
/// reduces to. Returns `None` for a partially typed code, leaving the
/// digits-then-letters split below to handle it.
fn extract_code(alnum: &str) -> Option<String> {
    let chars: Vec<char> = alnum.chars().collect();
    let last_start = chars.len().checked_sub(10)?;
    (0..=last_start)
        .find(|&i| {
            chars[i..i + 6].iter().all(char::is_ascii_digit)
                && chars[i + 6..i + 10].iter().all(char::is_ascii_alphabetic)
        })
        .map(|i| chars[i..i + 10].iter().collect())
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.process_events();

        let ctx = ui.ctx().clone();
        self.handle_close(&ctx);

        if ctx.input(|i| i.key_pressed(egui::Key::F10)) {
            self.log_drawer.toggle();
        }
        // Before the screen, so the drawer is the outermost bottom panel.
        self.log_drawer.show(ui, &self.log);

        match self.phase {
            Phase::Setup => self.show_setup(ui),
            Phase::WaitingForProcess => self.show_waiting_for_process(ui),
            Phase::CreatingSession => self.show_creating_session(ui),
            Phase::WaitingForPeer => self.show_waiting_for_peer(ui),
            Phase::Joining => self.show_joining(ui),
            Phase::Viewing => self.show_viewing(ui),
            Phase::Error => self.show_error(ui),
        }

        if self.phase != Phase::Viewing {
            self.show_utility_buttons(&ctx);
        }
        let session_in_flight = matches!(
            self.phase,
            Phase::CreatingSession | Phase::WaitingForPeer | Phase::Joining
        );
        if let Some(settings) = self.settings_panel.show(&ctx, session_in_flight) {
            self.apply_settings(settings);
        }
        if self.log_drawer.is_open() {
            // New lines arrive from other threads with no repaint of their own.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        match self.phase {
            // The picker's thumbnail grid refreshes on its own timer even
            // with no input, so it needs a repaint nudge; ChooseIntent and
            // Error are fully static and don't.
            Phase::Setup if self.setup_step == SetupStep::PickTarget => {
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
            Phase::Setup | Phase::Error => {}
            Phase::Viewing => {
                ctx.request_repaint_after(VIEWING_TICK);
            }
            _ => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }

    fn on_exit(&mut self) {
        // Closing the window mid-session is the most common way to hang up.
        // Give the session thread a moment to say goodbye, so the peer sees
        // "ended the session" now instead of minutes of reconnect attempts.
        if let (Some(session), Some(cancel)) = (&self.session, self.session_cancel.take()) {
            let _ = cancel.send(true);
            let deadline = Instant::now() + EXIT_GOODBYE_WAIT;
            while session.conn_state.get() != ConnState::Failed && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if let Some(code) = &self.hosted_session_code {
            let server = SignalingServer::new(&self.session_server_url);
            server.delete_session_blocking(code);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let persisted = Persisted {
            settings: self.settings.clone(),
            last_role: self.last_role,
            last_target: self.last_target.clone(),
            network_notice_seen: self.network_notice_seen,
        };
        eframe::set_value(storage, eframe::APP_KEY, &persisted);
    }

    /// egui's own memory (collapsed headers, scroll offsets, and so on) is
    /// nothing worth restoring, and restoring it could bring back style or
    /// theme state that `App::new` has just set.
    fn persist_egui_memory(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(pasted: &str) -> (String, String) {
        // As if the whole thing landed in the digits field.
        normalize_code(pasted, "")
    }

    #[test]
    fn pasting_a_share_link_fills_both_fields() {
        assert_eq!(
            normalized("https://screenshare.aghj.dev/j/482917bcdq"),
            ("482917".to_string(), "bcdq".to_string())
        );
    }

    #[test]
    fn pasting_a_bare_code_fills_both_fields() {
        assert_eq!(
            normalized("482917bcdq"),
            ("482917".to_string(), "bcdq".to_string())
        );
    }

    #[test]
    fn a_half_typed_code_still_splits_digits_then_letters() {
        assert_eq!(
            normalized("482917bc"),
            ("482917".to_string(), "bc".to_string())
        );
    }

    #[test]
    fn a_stray_character_in_the_letters_field_moves_to_the_digits() {
        assert_eq!(
            normalize_code("48291", "7bcdq"),
            ("482917".to_string(), "bcdq".to_string())
        );
    }

    #[test]
    fn extraction_finds_the_code_after_a_leading_digit() {
        assert_eq!(
            extract_code("1482917bcdq"),
            Some("482917bcdq".to_string())
        );
    }

    #[test]
    fn extraction_gives_up_on_a_partial_code() {
        assert_eq!(extract_code("482917bc"), None);
    }

    #[test]
    fn grouping_leaves_anything_that_is_not_a_code_alone() {
        assert_eq!(grouped_code("482917bcdq"), "482917 \u{00B7} bcdq");
        assert_eq!(grouped_code("482917"), "482917");
        assert_eq!(grouped_code(""), "");
    }

    #[test]
    fn display_link_drops_the_scheme() {
        assert_eq!(
            display_link("https://screenshare.aghj.dev/j/482917bcdq"),
            "screenshare.aghj.dev/j/482917bcdq"
        );
    }
}
