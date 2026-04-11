use egui::TextureHandle;
use std::sync::{Arc, Mutex};

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct ViewerApp {
    frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
    texture: Option<TextureHandle>,
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl ViewerApp {
    pub fn new(
        frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
        connected: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            frame_buffer,
            texture: None,
            connected,
        }
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check for new frame
        let new_frame = {
            let mut guard = self.frame_buffer.lock().unwrap();
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
                    self.texture = Some(ctx.load_texture("remote_screen", image, egui::TextureOptions::LINEAR));
                }
            }
        }

        let connected = self
            .connected
            .load(std::sync::atomic::Ordering::Relaxed);

        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (color, text) = if connected {
                    (egui::Color32::GREEN, "Connected")
                } else {
                    (egui::Color32::YELLOW, "Connecting...")
                };
                ui.colored_label(color, text);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = &self.texture {
                let available = ui.available_size();
                let tex_size = tex.size_vec2();

                // Scale to fit while preserving aspect ratio
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

        // Request repaint at ~60fps
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}
