use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{Complexity, Encoder, EncoderConfig, FrameType, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use std::time::{Duration, Instant};

pub struct H264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    /// When the last keyframe went out; `None` until the first one.
    last_keyframe: Option<Instant>,
}

/// How long a viewer that joins or loses data mid-stream waits, at most, for
/// a picture it can decode from. Timed rather than counted in frames: the
/// capture only delivers frames when the screen changes, so a frame count
/// meant for 60 fps could stretch to tens of seconds on a quiet screen.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

/// Bits per pixel per frame the encoder is budgeted. 0.07 is what the old
/// fixed 2 Mbps worked out to at 1280x720 and 30 fps, which looked right for
/// desktop content; scaling from it keeps larger or faster settings looking
/// the same rather than starving them.
const BITS_PER_PIXEL: f64 = 0.07;
const MIN_BITRATE: u32 = 1_000_000;
/// Past this the data channel, not the encoder, is the bottleneck.
const MAX_BITRATE: u32 = 10_000_000;

/// Encoder threads. OpenH264 only runs slices of one frame in parallel, and
/// caps its thread pool at 4.
const MAX_ENCODER_THREADS: usize = 4;
/// Upper bound on a slice, in bytes. The crate only offers single-slice or
/// size-limited slicing, and one slice leaves every thread but one idle; this
/// splits a typical 20-40 KB frame into a few slices and a keyframe into ~10.
const MAX_SLICE_BYTES: u32 = 10_000;

fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * BITS_PER_PIXEL;
    (bps as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        let api = OpenH264API::from_source();
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_ENCODER_THREADS) as u16;
        let config = EncoderConfig::new()
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .bitrate(openh264::encoder::BitRate::from_bps(bitrate_for(width, height, fps)))
            // Tools tuned for desktop content rather than camera footage.
            .usage_type(UsageType::ScreenContentRealTime)
            // Less motion search per frame: at 1440p the encoder, not the
            // link, is what limits the frame rate.
            .complexity(Complexity::Low)
            // Rate control budgets for `fps`, but a slow encoder sees far
            // fewer frames than that, overshoots on each, and then skips
            // frames it has already paid to encode. Let quality give instead.
            .skip_frames(false)
            .num_threads(threads)
            .max_slice_len(MAX_SLICE_BYTES);
        tracing::info!("Encoder {width}x{height} @ {fps} fps: {threads} threads");
        let encoder =
            Encoder::with_api_config(api, config).context("Failed to create H264 encoder")?;
        Ok(Self {
            encoder,
            width,
            height,
            last_keyframe: None,
        })
    }

    /// Encode a BGRA frame into H264 NAL units.
    pub fn encode(&mut self, bgra_data: &[u8]) -> Result<Vec<u8>> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bgra_src = BgraSliceU8::new(bgra_data, (w, h));
        let yuv = YUVBuffer::from_rgb_source(bgra_src);
        if self
            .last_keyframe
            .is_some_and(|t| t.elapsed() >= KEYFRAME_INTERVAL)
        {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&yuv).context("H264 encode failed")?;
        let nals: usize = (0..bitstream.num_layers())
            .filter_map(|i| bitstream.layer(i))
            .map(|l| l.nal_count())
            .sum();
        crate::diag::ENC_NALS.add(nals as u64);
        if matches!(bitstream.frame_type(), FrameType::IDR) {
            self.last_keyframe = Some(Instant::now());
        }
        Ok(bitstream.to_vec())
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().context("Failed to create H264 decoder")?;
        Ok(Self { decoder })
    }

    /// Decode H264 NAL units into an RGBA frame. Returns (width, height, rgba_data).
    pub fn decode(&mut self, h264_data: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>> {
        let yuv = match self.decoder.decode(h264_data) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("H264 decode failed: {e}")),
        };

        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);

        Ok(Some((width as u32, height as u32, rgba)))
    }
}
