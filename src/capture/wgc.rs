use super::{CaptureTarget, Frame};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

/// How much earlier than the frame interval a frame may arrive and still be
/// kept. WGC delivers on the display's refresh, so at 30 fps on a 60 Hz
/// screen every other frame lands a hair either side of the interval; a
/// strict cut-off would drop most of them and deliver 20.
const FRAME_INTERVAL_SLACK: f64 = 0.85;

struct CaptureHandler {
    tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    scratch: Vec<u8>,
    min_interval: Duration,
    last_kept: Option<Instant>,
}

struct Flags {
    tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    min_interval: Duration,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags.tx,
            paused: ctx.flags.paused,
            scratch: Vec::new(),
            min_interval: ctx.flags.min_interval,
            last_kept: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut frame::Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Paused: drop the frame before paying for the GPU readback, so
        // nothing reaches the encoder and it goes idle.
        if self.paused.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Frame-rate cap, applied here rather than through WGC's own
        // MinUpdateInterval, which older Windows 10 builds don't have. Also
        // before the readback, so a dropped frame costs nothing.
        let now = Instant::now();
        if self
            .last_kept
            .is_some_and(|t| now.duration_since(t) < self.min_interval.mul_f64(FRAME_INTERVAL_SLACK))
        {
            return Ok(());
        }
        self.last_kept = Some(now);

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

/// A running capture. Dropping it without calling [`CaptureSession::stop`]
/// leaves the capture thread running, which is what every session did
/// before Disconnect existed — so the one owner is expected to stop it.
pub struct CaptureSession {
    control: CaptureControl<CaptureHandler, <CaptureHandler as GraphicsCaptureApiHandler>::Error>,
}

impl CaptureSession {
    /// Stops capturing and waits for the capture thread to exit. The frame
    /// sender drops with the handler, which in turn ends the encoder thread.
    pub fn stop(self) {
        if let Err(e) = self.control.stop() {
            tracing::warn!("Failed to stop capture cleanly: {e}");
        }
    }
}

/// Starts capturing `target` on its own thread, at up to `fps` frames a
/// second. While `paused` is set, frames are dropped at the source.
pub fn start_capture(
    target: CaptureTarget,
    frame_tx: SyncSender<Frame>,
    paused: Arc<AtomicBool>,
    fps: u32,
) -> Result<CaptureSession> {
    let flags = Flags {
        tx: frame_tx,
        paused,
        min_interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64),
    };
    let control = match target {
        CaptureTarget::Window(window) => CaptureHandler::start_free_threaded(Settings::new(
            window,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        )),
        CaptureTarget::Monitor(monitor) => CaptureHandler::start_free_threaded(Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        )),
    }
    .map_err(|e| anyhow::anyhow!("Failed to start capture: {e}"))?;

    Ok(CaptureSession { control })
}
