pub mod picker;
pub mod session;
pub mod thumbnail;
pub mod video_processor;

use std::time::Instant;

/// What a capture session grabs: a single window, or an entire monitor.
/// `Window` is the common case; `Monitor` backs the setup screen's "entire
/// screen" tiles.
#[derive(Clone, Copy)]
pub enum CaptureTarget {
    Window(windows_capture::window::Window),
    Monitor(windows_capture::monitor::Monitor),
}

/// A frame ready for the encoder: NV12 (BT.709, studio range), already
/// scaled and colour-converted on the GPU by [`video_processor::VideoProcessor`]
/// (see `docs/hardware-encoding-plan.md` phase 2). `width`/`height` are the
/// encode target's dimensions, not the captured surface's.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, `width * height` bytes.
    pub y: Vec<u8>,
    /// Tightly packed, interleaved U/V, `width * height / 2` bytes.
    pub uv: Vec<u8>,
    #[allow(dead_code)] // Reserved for future A/V sync and pacing decisions.
    pub timestamp: Instant,
}

/// Rounds `v` up to the nearest multiple of 16, never below 16. Hardware
/// video encoders (phase 3) commonly require 16-aligned surfaces.
fn align_up_16(v: u32) -> u32 {
    v.div_ceil(16).max(1) * 16
}

/// Largest size that fits within `max_w`x`max_h` while preserving aspect
/// ratio, rounded up to multiples of 16. Never upscales. This is the encode
/// target the GPU video processor scales into; a source that doesn't divide
/// evenly by 16 is stretched by at most 15px to fill the padding rather than
/// cropped, since nothing downstream (yet — see phase 3's
/// `MF_MT_MINIMUM_DISPLAY_APERTURE`) removes it again.
pub fn aspect_fit_16(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / src_w as f64)
        .min(max_h as f64 / src_h as f64)
        .min(1.0);

    let dst_w = align_up_16((src_w as f64 * scale).round() as u32);
    let dst_h = align_up_16((src_h as f64 * scale).round() as u32);
    (dst_w, dst_h)
}

/// Downscales an NV12 frame (BT.709, studio range — matching the GPU video
/// processor's output) to fit within `max_w`x`max_h` and converts it to
/// RGBA, for the small "you are sharing" preview still. Never upscales.
/// Cheap enough to do per-pixel: it only runs once a second, on an
/// already-small frame.
pub fn nv12_preview_rgba(y: &[u8], uv: &[u8], src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
    let scale = (max_w as f64 / src_w as f64)
        .min(max_h as f64 / src_h as f64)
        .min(1.0);
    let dst_w = ((src_w as f64 * scale).round() as u32).max(1);
    let dst_h = ((src_h as f64 * scale).round() as u32).max(1);

    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for dy in 0..dst_h {
        let src_y = (dy as u64 * src_h as u64 / dst_h as u64) as u32;
        let uv_row = (src_y / 2) as usize;
        for dx in 0..dst_w {
            let src_x = (dx as u64 * src_w as u64 / dst_w as u64) as u32;
            let y_val = y[(src_y as usize) * (src_w as usize) + src_x as usize];
            let uv_col = ((src_x / 2) * 2) as usize;
            let uv_i = uv_row * (src_w as usize) + uv_col;
            let (u_val, v_val) = (uv[uv_i], uv[uv_i + 1]);

            let i = ((dy * dst_w + dx) * 4) as usize;
            let (r, g, b) = yuv709_studio_to_rgb(y_val, u_val, v_val);
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            out[i + 3] = 255;
        }
    }

    (dst_w, dst_h, out)
}

/// BT.709, studio range (16-235 luma, 16-240 chroma) to full-range RGB.
fn yuv709_studio_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = (y as f32 - 16.0).max(0.0) * (255.0 / 219.0);
    let u = u as f32 - 128.0;
    let v = v as f32 - 128.0;
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    (r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8)
}
