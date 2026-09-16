pub mod picker;
pub mod thumbnail;
pub mod wgc;

use std::time::Instant;

/// What a capture session grabs: a single window, or an entire monitor.
/// `Window` is the common case; `Monitor` backs the setup screen's "entire
/// screen" tiles.
#[derive(Clone, Copy)]
pub enum CaptureTarget {
    Window(windows_capture::window::Window),
    Monitor(windows_capture::monitor::Monitor),
}

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // BGRA8 pixel data
    #[allow(dead_code)] // Reserved for future A/V sync and pacing decisions.
    pub timestamp: Instant,
}

/// Scale a tightly-packed BGRA8 buffer to fit within `max_w`x`max_h`, preserving
/// aspect ratio, and repack it into a tightly-packed buffer with even dimensions
/// (required by the H264 encoder). Never upscales.
///
/// Building the output row by row from the source's real stride (`src_w`) rather
/// than truncating the source buffer avoids shearing when `src_w` is odd.
pub fn scale_bgra(
    data: &[u8],
    src_w: u32,
    src_h: u32,
    max_w: u32,
    max_h: u32,
) -> (u32, u32, Vec<u8>) {
    let scale = (max_w as f64 / src_w as f64)
        .min(max_h as f64 / src_h as f64)
        .min(1.0);

    let dst_w = (((src_w as f64 * scale).round() as u32).max(2)) & !1;
    let dst_h = (((src_h as f64 * scale).round() as u32).max(2)) & !1;

    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let src_y = (y as u64 * src_h as u64 / dst_h as u64) as u32;
        let src_row = (src_y as usize) * (src_w as usize) * 4;
        let dst_row = (y as usize) * (dst_w as usize) * 4;
        for x in 0..dst_w {
            let src_x = (x as u64 * src_w as u64 / dst_w as u64) as u32;
            let src_i = src_row + (src_x as usize) * 4;
            let dst_i = dst_row + (x as usize) * 4;
            out[dst_i..dst_i + 4].copy_from_slice(&data[src_i..src_i + 4]);
        }
    }

    (dst_w, dst_h, out)
}
