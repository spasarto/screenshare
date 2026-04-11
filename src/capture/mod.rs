pub mod picker;
pub mod wgc;

use std::time::Instant;

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // BGRA8 pixel data
    pub timestamp: Instant,
}
