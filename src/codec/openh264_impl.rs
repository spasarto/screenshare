use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;

pub struct H264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

impl H264Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let api = OpenH264API::from_source();
        let config = EncoderConfig::new()
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(30.0))
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .bitrate(openh264::encoder::BitRate::from_bps(2_000_000))
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(60));
        let encoder =
            Encoder::with_api_config(api, config).context("Failed to create H264 encoder")?;
        Ok(Self {
            encoder,
            width,
            height,
        })
    }

    /// Encode a BGRA frame into H264 NAL units.
    pub fn encode(&mut self, bgra_data: &[u8]) -> Result<Vec<u8>> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bgra_src = BgraSliceU8::new(bgra_data, (w, h));
        let yuv = YUVBuffer::from_rgb_source(bgra_src);
        let bitstream = self.encoder.encode(&yuv).context("H264 encode failed")?;
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
