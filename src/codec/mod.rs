pub mod mf_encoder;
pub mod openh264_impl;

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
