pub mod control;
pub mod crypto;
pub mod framing;
pub mod peer;
pub mod signaling;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Connection state shared across threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnState {
    Connecting = 0,
    Connected = 1,
    Reconnecting = 2,
    /// Terminal: the peer connection is closed, whether because reconnecting
    /// gave up, the peer said goodbye, or we hung up ourselves.
    Failed = 3,
}

impl ConnState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connected,
            2 => Self::Reconnecting,
            3 => Self::Failed,
            _ => Self::Connecting,
        }
    }
}

/// ICE candidate pair information for diagnostics.
#[derive(Clone, Debug)]
pub struct IceInfo {
    pub local_type: String,
    pub relay: bool,
}

/// Thread-safe shared connection state.
#[derive(Clone)]
pub struct SharedConnState {
    state: Arc<AtomicU8>,
    /// When `state` last changed value, so the viewer can say how long it
    /// has been reconnecting rather than just that it is.
    since: Arc<Mutex<Instant>>,
    /// Which ICE-restart attempt the reconnect monitor is on (0 when not
    /// reconnecting).
    reconnect_attempt: Arc<AtomicU32>,
    /// Set when the peer hung up on purpose (sent `Control::Bye`), as opposed
    /// to the link dying under us.
    peer_left: Arc<AtomicBool>,
    /// Set while the peer has paused sharing their screen.
    peer_paused: Arc<AtomicBool>,
    ice_info: Arc<Mutex<Option<IceInfo>>>,
}

impl SharedConnState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(ConnState::Connecting as u8)),
            since: Arc::new(Mutex::new(Instant::now())),
            reconnect_attempt: Arc::new(AtomicU32::new(0)),
            peer_left: Arc::new(AtomicBool::new(false)),
            peer_paused: Arc::new(AtomicBool::new(false)),
            ice_info: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Moves to `state`. `Failed` is terminal: once the peer connection is
    /// closed nothing brings it back, and letting a late callback (e.g. the
    /// reconnect monitor's error path) flip it back to `Reconnecting` would
    /// show a retry spinner for a connection that no longer exists.
    pub fn set(&self, state: ConnState) {
        let result = self
            .state
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                let old = ConnState::from_u8(old);
                (old != ConnState::Failed && old != state).then_some(state as u8)
            });
        if result.is_ok() {
            *self.since.lock().unwrap() = Instant::now();
            if state != ConnState::Reconnecting {
                self.reconnect_attempt.store(0, Ordering::Relaxed);
            }
        }
    }

    /// When the current state was entered.
    pub fn since(&self) -> Instant {
        *self.since.lock().unwrap()
    }

    pub fn set_reconnect_attempt(&self, attempt: u32) {
        self.reconnect_attempt.store(attempt, Ordering::Relaxed);
    }

    pub fn reconnect_attempt(&self) -> u32 {
        self.reconnect_attempt.load(Ordering::Relaxed)
    }

    pub fn set_peer_left(&self) {
        self.peer_left.store(true, Ordering::Relaxed);
    }

    pub fn peer_left(&self) -> bool {
        self.peer_left.load(Ordering::Relaxed)
    }

    pub fn set_peer_paused(&self, paused: bool) {
        self.peer_paused.store(paused, Ordering::Relaxed);
    }

    pub fn peer_paused(&self) -> bool {
        self.peer_paused.load(Ordering::Relaxed)
    }

    pub fn set_ice_info(&self, info: IceInfo) {
        *self.ice_info.lock().unwrap() = Some(info);
    }

    pub fn get_ice_info(&self) -> Option<IceInfo> {
        self.ice_info.lock().unwrap().clone()
    }
}

/// The kind of encoder a session is sending with, or decoder it's receiving
/// with, for the stats panel. Its name ("AMDh264Encoder", "OpenH264", ...)
/// goes to the log instead.
#[derive(Clone, Copy, Debug)]
pub struct CodecInfo {
    pub hardware: bool,
    /// The hardware codec was allowed but didn't start, or failed mid-session.
    pub fell_back: bool,
}

/// Running totals for one session, bumped by the media threads and sampled
/// by the viewer's stats panel, which turns them into rates.
///
/// Totals rather than rates on purpose: each writer only ever adds, so there
/// is no windowing logic to keep consistent across three threads, and the
/// reader decides its own sampling interval.
#[derive(Default)]
pub struct SessionStats {
    pub frames_received: AtomicU64,
    pub bytes_received: AtomicU64,
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Last decoded remote frame size, packed as `width << 32 | height`.
    remote_size: AtomicU64,
    /// Last encoded local frame size, packed the same way.
    local_size: AtomicU64,
    /// Latest in-band ping round trip, in microseconds. 0 until measured.
    rtt_micros: AtomicU64,
    encoder: Mutex<Option<CodecInfo>>,
    decoder: Mutex<Option<CodecInfo>>,
}

impl SessionStats {
    pub fn add_received(&self, bytes: usize) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn add_sent(&self, bytes: usize) {
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn set_remote_size(&self, width: u32, height: u32) {
        self.remote_size.store(pack_size(width, height), Ordering::Relaxed);
    }

    pub fn set_local_size(&self, width: u32, height: u32) {
        self.local_size.store(pack_size(width, height), Ordering::Relaxed);
    }

    pub fn remote_size(&self) -> Option<(u32, u32)> {
        unpack_size(self.remote_size.load(Ordering::Relaxed))
    }

    pub fn local_size(&self) -> Option<(u32, u32)> {
        unpack_size(self.local_size.load(Ordering::Relaxed))
    }

    pub fn set_rtt(&self, rtt: std::time::Duration) {
        // Never store 0: that value means "not measured yet".
        let micros = (rtt.as_micros() as u64).max(1);
        self.rtt_micros.store(micros, Ordering::Relaxed);
    }

    pub fn set_encoder(&self, info: CodecInfo) {
        *self.encoder.lock().unwrap() = Some(info);
    }

    pub fn encoder(&self) -> Option<CodecInfo> {
        *self.encoder.lock().unwrap()
    }

    pub fn set_decoder(&self, info: CodecInfo) {
        *self.decoder.lock().unwrap() = Some(info);
    }

    pub fn decoder(&self) -> Option<CodecInfo> {
        *self.decoder.lock().unwrap()
    }

    pub fn rtt(&self) -> Option<std::time::Duration> {
        match self.rtt_micros.load(Ordering::Relaxed) {
            0 => None,
            us => Some(std::time::Duration::from_micros(us)),
        }
    }
}

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn unpack_size(packed: u64) -> Option<(u32, u32)> {
    (packed != 0).then_some(((packed >> 32) as u32, packed as u32))
}
