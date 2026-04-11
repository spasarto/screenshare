use crate::app::SessionEvent;
use crate::capture::picker::{self, WindowInfo};
use crate::transport::{ConnState, SharedConnState};
use egui::TextureHandle;
use rand::Rng;
use std::sync::{Arc, Mutex};

fn generate_uuid_v4() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ]),
    )
}

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
    GeneratingOffer,
    OfferReady,
    AwaitingOffer,
    GeneratingAnswer,
    AnswerReady,
    Connecting,
    Viewing,
    Error,
}

pub struct App {
    phase: Phase,

    // Setup
    passphrase: String,
    windows: Vec<WindowInfo>,
    selected_window: Option<usize>,
    role: Role,
    setup_error: Option<String>,

    // Signaling
    local_code: String,
    remote_code_input: String,
    copied: bool,

    // Held for answerer (moved to background thread on connect)
    held_window: Option<windows_capture::window::Window>,
    held_passphrase: Option<String>,

    // Background communication
    event_rx: Option<std::sync::mpsc::Receiver<SessionEvent>>,
    answer_tx: Option<std::sync::mpsc::Sender<String>>,

    // Viewing
    frame_buffer: Option<Arc<Mutex<Option<DecodedFrame>>>>,
    texture: Option<TextureHandle>,
    conn_state: Option<SharedConnState>,

    // Error
    error_message: String,
}

impl App {
    pub fn new() -> Self {
        let windows = picker::enumerate_windows().unwrap_or_default();
        let has_windows = !windows.is_empty();
        Self {
            phase: Phase::Setup,
            passphrase: generate_uuid_v4(),
            windows,
            selected_window: if has_windows { Some(0) } else { None },
            role: Role::Create,
            setup_error: None,
            local_code: String::new(),
            remote_code_input: String::new(),
            copied: false,
            held_window: None,
            held_passphrase: None,
            event_rx: None,
            answer_tx: None,
            frame_buffer: None,
            texture: None,
            conn_state: None,
            error_message: String::new(),
        }
    }

    fn process_events(&mut self) {
        let Some(rx) = &self.event_rx else { return };

        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::OfferReady(offer) => {
                    self.local_code = offer;
                    self.phase = Phase::OfferReady;
                }
                SessionEvent::AnswerReady(answer) => {
                    self.local_code = answer;
                    self.phase = Phase::AnswerReady;
                }
                SessionEvent::SessionReady {
                    frame_buffer,
                    conn_state,
                } => {
                    self.frame_buffer = Some(frame_buffer);
                    self.conn_state = Some(conn_state);
                    // If answerer is still showing their response code, stay there
                    if self.phase != Phase::AnswerReady {
                        self.phase = Phase::Viewing;
                    }
                }
                SessionEvent::Error(msg) => {
                    self.error_message = msg;
                    self.phase = Phase::Error;
                }
            }
        }
    }

    fn show_setup(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.heading("Screen Share");
                ui.add_space(20.0);

                ui.set_max_width(450.0);

                ui.horizontal(|ui| {
                    ui.label("Passphrase:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.passphrase)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                });

                ui.add_space(8.0);

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

                if let Some(err) = &self.setup_error {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::RED, err.clone());
                }

                ui.add_space(12.0);

                if ui.button("Start").clicked() {
                    self.handle_start();
                }
            });
        });
    }

    fn handle_start(&mut self) {
        if self.passphrase.is_empty() {
            self.setup_error = Some("Passphrase is required".into());
            return;
        }

        let Some(idx) = self.selected_window else {
            self.setup_error = Some("Select a window to share".into());
            return;
        };

        if idx >= self.windows.len() {
            self.setup_error = Some("Invalid window selection".into());
            return;
        }

        let passphrase = self.passphrase.clone();
        let windows = std::mem::take(&mut self.windows);
        let window = windows.into_iter().nth(idx).unwrap().window;

        match self.role {
            Role::Create => {
                let (event_tx, event_rx) = std::sync::mpsc::channel();
                let (answer_tx, answer_rx) = std::sync::mpsc::channel();

                self.event_rx = Some(event_rx);
                self.answer_tx = Some(answer_tx);

                crate::app::spawn_offerer(passphrase, window, event_tx, answer_rx);
                self.phase = Phase::GeneratingOffer;
            }
            Role::Join => {
                self.held_window = Some(window);
                self.held_passphrase = Some(passphrase);
                self.phase = Phase::AwaitingOffer;
            }
        }
    }

    fn show_generating(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label("Generating connection code...");
            });
        });
    }

    fn show_offer_ready(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.heading("Your Connection Code");
                    ui.add_space(10.0);

                    ui.set_max_width(550.0);

                    ui.label("Send this code to your peer:");
                    ui.add_space(4.0);

                    let mut code = self.local_code.clone();
                    ui.add(
                        egui::TextEdit::multiline(&mut code)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );

                    ui.horizontal(|ui| {
                        if ui.button("📋 Copy").clicked() {
                            ui.ctx().copy_text(self.local_code.clone());
                            self.copied = true;
                        }
                        if self.copied {
                            ui.colored_label(egui::Color32::GREEN, "✓ Copied!");
                        }
                    });

                    ui.add_space(16.0);
                    ui.label("Paste their response code below:");
                    ui.add_space(4.0);

                    ui.add(
                        egui::TextEdit::multiline(&mut self.remote_code_input)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Paste response code here..."),
                    );

                    ui.add_space(8.0);
                    let can_connect = !self.remote_code_input.trim().is_empty();
                    ui.add_enabled_ui(can_connect, |ui| {
                        if ui.button("Connect").clicked() {
                            if let Some(tx) = &self.answer_tx {
                                let _ = tx.send(self.remote_code_input.trim().to_string());
                            }
                            self.answer_tx = None;
                            self.phase = Phase::Connecting;
                        }
                    });
                });
            });
        });
    }

    fn show_awaiting_offer(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.heading("Join Session");
                ui.add_space(16.0);

                ui.set_max_width(550.0);

                ui.label("Paste the connection code from your peer:");
                ui.add_space(4.0);

                ui.add(
                    egui::TextEdit::multiline(&mut self.remote_code_input)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Paste connection code here..."),
                );

                ui.add_space(8.0);
                let can_connect = !self.remote_code_input.trim().is_empty();
                ui.add_enabled_ui(can_connect, |ui| {
                    if ui.button("Connect").clicked() {
                        let window = self.held_window.take().expect("window should be held");
                        let passphrase = self
                            .held_passphrase
                            .take()
                            .expect("passphrase should be held");
                        let offer = self.remote_code_input.trim().to_string();

                        let (event_tx, event_rx) = std::sync::mpsc::channel();
                        self.event_rx = Some(event_rx);

                        crate::app::spawn_answerer(passphrase, window, offer, event_tx);
                        self.phase = Phase::GeneratingAnswer;
                    }
                });
            });
        });
    }

    fn show_answer_ready(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.heading("Your Response Code");
                ui.add_space(10.0);

                ui.set_max_width(550.0);

                ui.label("Send this code back to your peer:");
                ui.add_space(4.0);

                let mut code = self.local_code.clone();
                ui.add(
                    egui::TextEdit::multiline(&mut code)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );

                ui.horizontal(|ui| {
                    if ui.button("📋 Copy").clicked() {
                        ui.ctx().copy_text(self.local_code.clone());
                        self.copied = true;
                    }
                    if self.copied {
                        ui.colored_label(egui::Color32::GREEN, "✓ Copied!");
                    }
                });

                ui.add_space(16.0);

                let session_ready = self.frame_buffer.is_some();
                if session_ready {
                    if ui.button("Continue to Viewer").clicked() {
                        self.phase = Phase::Viewing;
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Setting up session...");
                    });
                }
            });
        });
    }

    fn show_connecting(&self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.spinner();
                ui.add_space(10.0);
                ui.label("Connecting...");
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
                ui.add_space(100.0);
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

        match self.phase {
            Phase::Setup => self.show_setup(ctx),
            Phase::GeneratingOffer | Phase::GeneratingAnswer => self.show_generating(ctx),
            Phase::OfferReady => self.show_offer_ready(ctx),
            Phase::AwaitingOffer => self.show_awaiting_offer(ctx),
            Phase::AnswerReady => self.show_answer_ready(ctx),
            Phase::Connecting => self.show_connecting(ctx),
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
}
