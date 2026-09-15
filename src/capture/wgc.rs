use super::Frame;
use anyhow::Result;
use std::sync::mpsc::SyncSender;
use std::time::Instant;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

struct CaptureHandler {
    tx: SyncSender<Frame>,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = SyncSender<Frame>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut frame::Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();

        let buffer = frame.buffer()?;
        let data = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();

        let frame = Frame {
            width,
            height,
            data,
            timestamp: Instant::now(),
        };

        // Non-blocking send; drop frame if the channel is full
        let _ = self.tx.try_send(frame);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("Capture source closed");
        Ok(())
    }
}

pub fn start_capture(
    window: Window,
    frame_tx: SyncSender<Frame>,
) -> Result<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let settings = Settings::new(
                window,
                CursorCaptureSettings::WithCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                frame_tx,
            );

            if let Err(e) = CaptureHandler::start(settings) {
                tracing::error!("Capture error: {e}");
            }
        })?;

    Ok(handle)
}
