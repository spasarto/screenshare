pub mod mf_decoder;
pub mod mf_encoder;
pub mod openh264_impl;

pub use mf_decoder::MfDecoder;
pub use mf_encoder::MfEncoder;
pub use openh264_impl::{H264Decoder, H264Encoder};

use anyhow::Result;

/// What the encoders take: a captured NV12 frame, still on the GPU.
pub type EncoderInput = crate::capture::Frame;

/// An H.264 encoder: Media Foundation's hardware one ([`MfEncoder`]) or the
/// OpenH264 software fallback ([`H264Encoder`]). See phase 3 of
/// `docs/hardware-encoding-plan.md`. Not `Send`: an encoder is created and
/// used on the encoder thread only, and the Media Foundation one holds COM
/// objects that shouldn't wander.
pub trait VideoEncoder {
    /// Encodes one frame to Annex-B H.264. Can come back empty when the
    /// encoder skipped the frame or is holding it back for later output.
    fn encode(&mut self, frame: &EncoderInput) -> Result<Vec<u8>>;
    /// Makes the next encoded frame an IDR, with SPS/PPS in front of it.
    fn force_keyframe(&mut self);
    /// For logs: "NVIDIA H.264 Encoder MFT", "OpenH264", ...
    fn name(&self) -> &str;
    /// The frame size this encoder was created for; a frame of any other
    /// size needs a new encoder.
    fn size(&self) -> (u32, u32);
}

/// A decoded picture in NV12: a full-size Y plane, then a half-size plane
/// of interleaved U/V pairs, both tightly packed (row stride = `width`
/// bytes). `width` and `height` are even, as 4:2:0 H.264 guarantees. This
/// is what the viewer uploads and converts to RGB on the GPU (phase 5 of
/// `docs/hardware-encoding-plan.md`).
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes.
    pub y: Vec<u8>,
    /// `width * height / 2` bytes: `height / 2` rows of `width / 2` U/V pairs.
    pub uv: Vec<u8>,
}

/// An H.264 decoder: Media Foundation's ([`MfDecoder`], hardware-accelerated
/// where the GPU allows) or OpenH264 ([`H264Decoder`]), the fallback. Like
/// [`VideoEncoder`], created and used on one thread only.
pub trait VideoDecoder {
    /// Decodes one access unit (Annex-B). `None` when it produced no picture
    /// (parameter sets only, or the decoder is still waiting for an IDR).
    fn decode(&mut self, h264: &[u8]) -> Result<Option<Nv12Frame>>;
    /// For logs: "Microsoft H264 Video Decoder MFT (DXVA)", "OpenH264", ...
    fn name(&self) -> &str;
}

/// Bits per pixel per frame the encoder is budgeted. 0.07 is what the old
/// fixed 2 Mbps worked out to at 1280x720 and 30 fps, which looked right for
/// desktop content; scaling from it keeps larger or faster settings looking
/// the same rather than starving them.
const BITS_PER_PIXEL: f64 = 0.07;
const MIN_BITRATE: u32 = 1_000_000;
/// Past this the data channel, not the encoder, is the bottleneck.
const MAX_BITRATE: u32 = 10_000_000;

fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * BITS_PER_PIXEL;
    (bps as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}
