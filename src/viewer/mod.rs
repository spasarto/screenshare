use crate::app::SessionEvent;
use crate::capture::picker::{self, WindowInfo};
use crate::transport::{ConnState, SharedConnState};
use crate::CliArgs;
use egui::TextureHandle;
use std::sync::{Arc, Mutex};

const SERVER_URL: &str = "https://screenshare.aghj.dev";

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Create,
    Join,
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
    windows: Vec<WindowInfo>,
    selected_window: Option<usize>,
    role: Role,
    session_code: String,
    setup_error: Option<String>,

    // CLI auto-start
    cli_process: Option<String>,
    cli_pin: Option<String>,
    last_process_poll: Option<std::time::Instant>,

    // Background communication
    event_rx: Option<std::sync::mpsc::Receiver<SessionEvent>>,

    // Viewing
    frame_buffer: Option<Arc<Mutex<Option<DecodedFrame>>>>,
    texture: Option<TextureHandle>,
    conn_state: Option<SharedConnState>,

    // Logo
    logo_texture: Option<TextureHandle>,

    // Cleanup: set when we are the offerer and have a session code
    hosted_session_code: Option<String>,

    // Error
    error_message: String,
}

impl App {
    pub fn new(args: CliArgs, cc: &eframe::CreationContext) -> Self {
        let windows = picker::enumerate_windows().unwrap_or_default();
        let has_windows = !windows.is_empty();

        // Determine role from CLI flags, defaulting to Create
        let role = if args.client {
            Role::Join
        } else {
            Role::Create
        };

        // Pre-fill session code from --pin for client mode
        let session_code = if args.client {
            args.pin.clone().unwrap_or_default()
        } else {
            String::new()
        };

        // Decide starting phase: if we have a role flag + process, wait for process
        let has_role = args.server || args.client;
        let phase = if has_role && args.process.is_some() {
            Phase::WaitingForProcess
        } else {
            Phase::Setup
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
            windows,
            selected_window: if has_windows { Some(0) } else { None },
            role,
            session_code,
            setup_error: None,
            cli_process: args.process,
            cli_pin: if args.server { args.pin } else { None },
            last_process_poll: None,
            event_rx: None,
            frame_buffer: None,
            texture: None,
            conn_state: None,
            logo_texture,
            hosted_session_code: None,
            error_message: String::new(),
        }
    }

    fn process_events(&mut self) {
        let Some(rx) = &self.event_rx else { return };

        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::SessionCode(code) => {
                    self.hosted_session_code = Some(code.clone());
                    self.session_code = code;
                    self.phase = Phase::WaitingForPeer;
                }
                SessionEvent::SessionReady {
                    frame_buffer,
                    conn_state,
                } => {
                    self.frame_buffer = Some(frame_buffer);
                    self.conn_state = Some(conn_state);
                    self.phase = Phase::Viewing;
                }
                SessionEvent::Error(msg) => {
                    self.error_message = msg;
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

    fn show_setup(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                self.show_logo(ui);
                ui.add_space(12.0);

                ui.set_max_width(450.0);

                ui.horizontal(|ui| {
                    ui.label("Window:");
                    let selected_text = self
                        .selected_window
                        .and_then(|i| self.windows.get(i))
                        .map(|w| w.title.as_str())
                        .unwrap_or("Select a window...");
                    egui::ComboBox::from_id_salt("window_picker")
                        .selected_text(selected_text)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for (i, w) in self.windows.iter().enumerate() {
                                ui.selectable_value(&mut self.selected_window, Some(i), &w.title);
                            }
                        });
                    if ui
                        .button("↻")
                        .on_hover_text("Refresh window list")
                        .clicked()
                    {
                        self.windows = picker::enumerate_windows().unwrap_or_default();
                        if self
                            .selected_window
                            .map(|i| i >= self.windows.len())
                            .unwrap_or(true)
                        {
                            self.selected_window = if self.windows.is_empty() {
                                None
                            } else {
                                Some(0)
                            };
                        }
                    }
                });

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.role, Role::Create, "Create session");
                    ui.radio_value(&mut self.role, Role::Join, "Join session");
                });

                if self.role == Role::Join {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Session code:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.session_code)
                                .desired_width(120.0)
                                .hint_text("e.g. 482917bcdq"),
                        );
                    });
                }

                if let Some(err) = &self.setup_error {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::RED, err.clone());
                }

                ui.add_space(12.0);

                if ui
                    .scope(|ui| {
                        ui.spacing_mut().button_padding = egui::vec2(16.0, 8.0);
                        ui.button("Start")
                    })
                    .inner
                    .clicked()
                {
                    self.handle_start();
                }
            });
        });
    }

    fn handle_start(&mut self) {
        let Some(idx) = self.selected_window else {
            self.setup_error = Some("Select a window to share".into());
            return;
        };

        if idx >= self.windows.len() {
            self.setup_error = Some("Invalid window selection".into());
            return;
        }

        let windows = std::mem::take(&mut self.windows);
        let window = windows.into_iter().nth(idx).unwrap().window;

        match self.role {
            Role::Create => {
                let (event_tx, event_rx) = std::sync::mpsc::channel();
                self.event_rx = Some(event_rx);

                crate::app::spawn_offerer(
                    SERVER_URL.to_string(),
                    window,
                    self.cli_pin.clone(),
                    event_tx,
                );
                self.phase = Phase::CreatingSession;
            }
            Role::Join => {
                let code = self.session_code.trim().to_lowercase();
                if code.is_empty() {
                    self.setup_error = Some("Enter a session code".into());
                    // Put windows back
                    self.windows = picker::enumerate_windows().unwrap_or_default();
                    return;
                }

                let (event_tx, event_rx) = std::sync::mpsc::channel();
                self.event_rx = Some(event_rx);

                crate::app::spawn_answerer(SERVER_URL.to_string(), window, code, event_tx);
                self.phase = Phase::Joining;
            }
        }
    }

    /// Auto-start a session directly with a found window (used by WaitingForProcess phase).
    fn start_with_window(&mut self, window: windows_capture::window::Window) {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        self.event_rx = Some(event_rx);

        match self.role {
            Role::Create => {
                crate::app::spawn_offerer(
                    SERVER_URL.to_string(),
                    window,
                    self.cli_pin.clone(),
                    event_tx,
                );
                self.phase = Phase::CreatingSession;
            }
            Role::Join => {
                let code = self.session_code.trim().to_lowercase();
                crate::app::spawn_answerer(SERVER_URL.to_string(), window, code, event_tx);
                self.phase = Phase::Joining;
            }
        }
    }

    fn show_creating_session(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                self.show_logo(ui);
                ui.add_space(40.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label("Creating session...");
            });
        });
    }

    fn show_waiting_for_process(&mut self, ctx: &egui::Context) {
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
                self.start_with_window(info.window);
                return;
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                self.show_logo(ui);
                ui.add_space(40.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label(format!("Waiting for '{}' to start...", process_name));
            });
        });
    }

    fn show_waiting_for_peer(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                self.show_logo(ui);
                ui.add_space(20.0);
                ui.heading("Session Created");
                ui.add_space(20.0);

                ui.label("Share this code with your peer:");
                ui.add_space(12.0);

                ui.label(
                    egui::RichText::new(&self.session_code)
                        .size(48.0)
                        .monospace()
                        .strong(),
                );

                ui.add_space(20.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label("Waiting for peer to join...");
            });
        });
    }

    fn show_joining(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                self.show_logo(ui);
                ui.add_space(40.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label("Joining session...");
            });
        });
    }

    fn show_viewing(&mut self, ctx: &egui::Context) {
        // Check for new frame
        if let Some(fb) = &self.frame_buffer {
            let new_frame = {
                let mut guard = fb.lock().unwrap();
                guard.take()
            };

            if let Some(decoded) = new_frame {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [decoded.width as usize, decoded.height as usize],
                    &decoded.rgba,
                );

                match &mut self.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ctx.load_texture(
                            "remote_screen",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
        }

        let state = self
            .conn_state
            .as_ref()
            .map(|cs| cs.get())
            .unwrap_or(ConnState::Connecting);

        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (color, text) = match state {
                    ConnState::Connected => (egui::Color32::GREEN, "Connected"),
                    ConnState::Reconnecting => {
                        (egui::Color32::from_rgb(255, 165, 0), "Reconnecting...")
                    }
                    ConnState::Failed => (egui::Color32::RED, "Disconnected"),
                    ConnState::Connecting => (egui::Color32::YELLOW, "Connecting..."),
                };
                ui.colored_label(color, text);

                if let Some(ice) = self.conn_state.as_ref().and_then(|cs| cs.get_ice_info()) {
                    ui.separator();
                    if ice.relay {
                        ui.colored_label(egui::Color32::from_rgb(255, 165, 0), "Relayed (TURN)");
                    } else {
                        ui.colored_label(
                            egui::Color32::GREEN,
                            format!("Direct ({})", ice.local_type),
                        );
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = &self.texture {
                let available = ui.available_size();
                let tex_size = tex.size_vec2();

                let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                let display_size = egui::vec2(tex_size.x * scale, tex_size.y * scale);

                ui.centered_and_justified(|ui| {
                    ui.image(egui::load::SizedTexture::new(tex.id(), display_size));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Waiting for remote screen...");
                });
            }
        });
    }

    fn show_error(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                self.show_logo(ui);
                ui.add_space(30.0);
                ui.colored_label(egui::Color32::RED, "Error");
                ui.add_space(10.0);
                ui.label(&self.error_message);
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();

        if ctx.input(|i| i.key_pressed(egui::Key::F10)) {
            crate::console::toggle();
        }

        match self.phase {
            Phase::Setup => self.show_setup(ctx),
            Phase::WaitingForProcess => self.show_waiting_for_process(ctx),
            Phase::CreatingSession => self.show_creating_session(ctx),
            Phase::WaitingForPeer => self.show_waiting_for_peer(ctx),
            Phase::Joining => self.show_joining(ctx),
            Phase::Viewing => self.show_viewing(ctx),
            Phase::Error => self.show_error(ctx),
        }

        match self.phase {
            Phase::Setup | Phase::Error => {}
            Phase::Viewing => {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
            _ => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(code) = &self.hosted_session_code {
            let server = crate::signaling_server::SignalingServer::new(SERVER_URL);
            server.delete_session_blocking(code);
        }
    }
}
