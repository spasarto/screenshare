//! Per-stage frame counters for the media pipeline, logged once a second.
//!
//! Each stage (capture → encode → send → receive → decode → display) bumps
//! its own counters; a reporter thread logs the last second's worth and
//! resets them. A stage that stalls outright still shows up — as zeros —
//! because the reporter, not the stage, does the logging.
//!
//! Counters are process-global: one process runs one session at a time.

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Records a duration, in microseconds.
    pub fn add_time(&self, d: Duration) {
        self.add(d.as_micros() as u64);
    }

    pub fn max(&self, n: u64) {
        self.0.fetch_max(n, Ordering::Relaxed);
    }

    pub fn max_time(&self, d: Duration) {
        self.max(d.as_micros() as u64);
    }

    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

// Capture (WGC callback thread).
pub static CAP_ARRIVED: Counter = Counter::new();
pub static CAP_SKIP_PAUSED: Counter = Counter::new();
pub static CAP_SKIP_RATE: Counter = Counter::new();
pub static CAP_KEPT: Counter = Counter::new();
pub static CAP_DROP_FULL: Counter = Counter::new();
/// Time spent in the GPU video processor's scale + BGRA→NV12 Blt (phase 2 of
/// docs/hardware-encoding-plan.md). The frame stays on the GPU afterwards;
/// any CPU readback is counted on the encoder side (`ENC_READBACK_*`).
pub static CAP_GPU_US: Counter = Counter::new();
pub static CAP_GPU_MAX_US: Counter = Counter::new();

// Encoder thread.
pub static ENC_IN: Counter = Counter::new();
pub static ENC_SCALE_US: Counter = Counter::new();
pub static ENC_ENCODE_US: Counter = Counter::new();
pub static ENC_ENCODE_MAX_US: Counter = Counter::new();
pub static ENC_OUT: Counter = Counter::new();
pub static ENC_EMPTY: Counter = Counter::new();
pub static ENC_ERR: Counter = Counter::new();
pub static ENC_BYTES: Counter = Counter::new();
pub static ENC_MAX_BYTES: Counter = Counter::new();
pub static ENC_IDR: Counter = Counter::new();
/// NAL units produced, SPS/PPS included: well above ~1 per frame means the
/// frame was split into slices the encoder could spread over threads.
pub static ENC_NALS: Counter = Counter::new();
pub static ENC_BLOCKED_US: Counter = Counter::new();
/// NV12 GPU → CPU copies: every frame for the software encoder, once a
/// second for the preview, never for the hardware encoder's frames.
pub static ENC_READBACKS: Counter = Counter::new();
pub static ENC_READBACK_US: Counter = Counter::new();
pub static ENC_READBACK_MAX_US: Counter = Counter::new();

/// Which encoder the encoder thread is using, named in the `encode` line.
static ENCODER_NAME: Mutex<String> = Mutex::new(String::new());

pub fn set_encoder(name: &str) {
    *ENCODER_NAME.lock().unwrap() = name.to_string();
}

/// Which decoder the decoder thread is using, named in the `decode` part of
/// the `receive` line.
static DECODER_NAME: Mutex<String> = Mutex::new(String::new());

pub fn set_decoder(name: &str) {
    *DECODER_NAME.lock().unwrap() = name.to_string();
}

// Sender task.
pub static SND_IN: Counter = Counter::new();
pub static SND_DROP_STATE: Counter = Counter::new();
pub static SND_DROP_NO_DC: Counter = Counter::new();
pub static SND_DROP_BUFFERED: Counter = Counter::new();
pub static SND_ERR: Counter = Counter::new();
pub static SND_SENT: Counter = Counter::new();
pub static SND_SEND_US: Counter = Counter::new();
pub static SND_SEND_MAX_US: Counter = Counter::new();
pub static SND_MAX_BUFFERED: Counter = Counter::new();

// Receive side (data channel callback, then decoder thread).
pub static RCV_MSGS: Counter = Counter::new();
pub static RCV_BYTES: Counter = Counter::new();
pub static RCV_QUEUE_WAIT_US: Counter = Counter::new();
pub static DEC_IN: Counter = Counter::new();
pub static DEC_FRAME: Counter = Counter::new();
pub static DEC_NONE: Counter = Counter::new();
pub static DEC_ERR: Counter = Counter::new();
pub static DEC_US: Counter = Counter::new();
pub static DEC_MAX_US: Counter = Counter::new();
/// A decoded frame replaced one the GUI never picked up, or the decoder
/// produced several at once and only the newest was kept.
pub static DEC_OVERWRITTEN: Counter = Counter::new();

// GUI thread.
pub static GUI_UPLOADS: Counter = Counter::new();
pub static GUI_UPLOAD_US: Counter = Counter::new();
pub static GUI_PAINTS: Counter = Counter::new();

/// Starts the reporter thread, once per process.
pub fn start_reporter() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("diag".into())
            .spawn(reporter);
    });
}

fn avg_ms(total_us: u64, n: u64) -> f64 {
    if n == 0 { 0.0 } else { total_us as f64 / n as f64 / 1000.0 }
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn reporter() {
    let mut last = Instant::now();
    loop {
        std::thread::sleep(REPORT_INTERVAL);
        let secs = last.elapsed().as_secs_f64();
        last = Instant::now();

        let cap_arrived = CAP_ARRIVED.take();
        let cap_paused = CAP_SKIP_PAUSED.take();
        let cap_rate = CAP_SKIP_RATE.take();
        let cap_kept = CAP_KEPT.take();
        let cap_full = CAP_DROP_FULL.take();
        let cap_gpu = CAP_GPU_US.take();
        let cap_gpu_max = CAP_GPU_MAX_US.take();

        let enc_in = ENC_IN.take();
        let enc_scale = ENC_SCALE_US.take();
        let enc_us = ENC_ENCODE_US.take();
        let enc_max = ENC_ENCODE_MAX_US.take();
        let enc_out = ENC_OUT.take();
        let enc_empty = ENC_EMPTY.take();
        let enc_err = ENC_ERR.take();
        let enc_bytes = ENC_BYTES.take();
        let enc_max_bytes = ENC_MAX_BYTES.take();
        let enc_idr = ENC_IDR.take();
        let enc_nals = ENC_NALS.take();
        let enc_blocked = ENC_BLOCKED_US.take();
        let enc_rbs = ENC_READBACKS.take();
        let enc_rb = ENC_READBACK_US.take();
        let enc_rb_max = ENC_READBACK_MAX_US.take();

        let snd_in = SND_IN.take();
        let snd_state = SND_DROP_STATE.take();
        let snd_no_dc = SND_DROP_NO_DC.take();
        let snd_buf = SND_DROP_BUFFERED.take();
        let snd_err = SND_ERR.take();
        let snd_sent = SND_SENT.take();
        let snd_us = SND_SEND_US.take();
        let snd_max = SND_SEND_MAX_US.take();
        let snd_max_buf = SND_MAX_BUFFERED.take();

        let rcv_msgs = RCV_MSGS.take();
        let rcv_bytes = RCV_BYTES.take();
        let rcv_wait = RCV_QUEUE_WAIT_US.take();
        let dec_in = DEC_IN.take();
        let dec_frame = DEC_FRAME.take();
        let dec_none = DEC_NONE.take();
        let dec_err = DEC_ERR.take();
        let dec_us = DEC_US.take();
        let dec_max = DEC_MAX_US.take();
        let dec_over = DEC_OVERWRITTEN.take();

        let gui_up = GUI_UPLOADS.take();
        let gui_up_us = GUI_UPLOAD_US.take();
        let gui_paints = GUI_PAINTS.take();

        let encoder = ENCODER_NAME.lock().unwrap().clone();
        let decoder = DECODER_NAME.lock().unwrap().clone();

        let anything = cap_arrived + enc_in + snd_in + rcv_msgs + dec_in + gui_up;
        if anything == 0 {
            continue;
        }

        tracing::info!(
            "pipeline {secs:.2}s | capture: arrived {cap_arrived}, kept {cap_kept}, \
             skip_rate {cap_rate}, skip_paused {cap_paused}, drop_queue_full {cap_full}, \
             gpu scale avg {:.1}ms max {:.1}ms",
            avg_ms(cap_gpu, cap_kept + cap_full),
            ms(cap_gpu_max),
        );
        tracing::info!(
            "pipeline | encode [{encoder}]: in {enc_in}, out {enc_out} (idr {enc_idr}), empty {enc_empty}, \
             err {enc_err}, nals/frame {:.1}, {:.2} Mbit, max frame {} KB, scale avg {:.1}ms, \
             readbacks {enc_rbs} avg {:.1}ms max {:.1}ms, \
             encode avg {:.1}ms max {:.1}ms, blocked on send queue {:.0}ms",
            if enc_out == 0 { 0.0 } else { enc_nals as f64 / enc_out as f64 },
            enc_bytes as f64 * 8.0 / 1e6 / secs,
            enc_max_bytes / 1024,
            avg_ms(enc_scale, enc_in),
            avg_ms(enc_rb, enc_rbs),
            ms(enc_rb_max),
            avg_ms(enc_us, enc_in),
            ms(enc_max),
            ms(enc_blocked),
        );
        tracing::info!(
            "pipeline | send: in {snd_in}, sent {snd_sent}, drop_buffered {snd_buf}, \
             drop_not_connected {snd_state}, drop_no_dc {snd_no_dc}, err {snd_err}, \
             send avg {:.1}ms max {:.1}ms, max buffered {} KB",
            avg_ms(snd_us, snd_sent),
            ms(snd_max),
            snd_max_buf / 1024,
        );
        tracing::info!(
            "pipeline | receive: msgs {rcv_msgs}, {:.2} Mbit, queue wait {:.0}ms | \
             decode [{decoder}]: in {dec_in}, frames {dec_frame}, none {dec_none}, err {dec_err}, \
             avg {:.1}ms max {:.1}ms, overwritten before display {dec_over} | \
             gui: paints {gui_paints}, uploads {gui_up}, upload avg {:.1}ms",
            rcv_bytes as f64 * 8.0 / 1e6 / secs,
            ms(rcv_wait),
            avg_ms(dec_us, dec_in),
            ms(dec_max),
            avg_ms(gui_up_us, gui_up),
        );
    }
}

/// Whether an Annex-B H.264 bitstream contains an IDR slice (NAL type 5).
pub fn has_idr(bitstream: &[u8]) -> bool {
    bitstream
        .windows(4)
        .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1f) == 5)
}
