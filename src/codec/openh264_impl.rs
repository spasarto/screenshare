use super::{EncoderInput, VideoEncoder, bitrate_for};
use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{Complexity, Encoder, EncoderConfig, UsageType};
use openh264::formats::YUVSource;

/// Adapts NV12 planes (as produced by the GPU video processor, phase 2 of
/// `docs/hardware-encoding-plan.md`) to the encoder's planar `YUVSource`.
/// The Y plane is used as-is; U/V are de-interleaved once here, which is far
/// cheaper than the BGRA→I420 conversion it replaces.
struct Nv12Yuv<'a> {
    width: usize,
    height: usize,
    y: &'a [u8],
    u: Vec<u8>,
    v: Vec<u8>,
}

impl<'a> Nv12Yuv<'a> {
    fn new(y: &'a [u8], uv: &[u8], width: usize, height: usize) -> Self {
        let chroma_len = (width / 2) * (height / 2);
        let mut u = vec![0u8; chroma_len];
        let mut v = vec![0u8; chroma_len];
        for i in 0..chroma_len {
            u[i] = uv[i * 2];
            v[i] = uv[i * 2 + 1];
        }
        Self { width, height, y, u, v }
    }
}

impl YUVSource for Nv12Yuv<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

pub struct H264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

/// Encoder threads. OpenH264 only runs slices of one frame in parallel, and
/// caps its thread pool at 4.
const MAX_ENCODER_THREADS: usize = 4;
/// Upper bound on a slice, in bytes. The crate only offers single-slice or
/// size-limited slicing, and one slice leaves every thread but one idle; this
/// splits a typical 20-40 KB frame into a few slices and a keyframe into ~10.
const MAX_SLICE_BYTES: u32 = 10_000;

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
        Ok(Self { encoder, width, height })
    }

    /// Encode an NV12 frame (Y plane, interleaved UV plane) into H264 NAL units.
    fn encode_planes(&mut self, y: &[u8], uv: &[u8]) -> Result<Vec<u8>> {
        let yuv = Nv12Yuv::new(y, uv, self.width as usize, self.height as usize);
        let bitstream = self.encoder.encode(&yuv).context("H264 encode failed")?;
        let nals: usize = (0..bitstream.num_layers())
            .filter_map(|i| bitstream.layer(i))
            .map(|l| l.nal_count())
            .sum();
        crate::diag::ENC_NALS.add(nals as u64);
        Ok(bitstream.to_vec())
    }
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, frame: &EncoderInput) -> Result<Vec<u8>> {
        let (y, uv) = frame.read_back().context("NV12 readback failed")?;
        self.encode_planes(&y, &uv)
    }

    fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    fn name(&self) -> &str {
        "OpenH264"
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
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
