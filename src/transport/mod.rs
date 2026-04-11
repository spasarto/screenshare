pub mod framing;
pub mod peer;
pub mod signaling;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Connection state shared across threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnState {
    Connecting = 0,
    Connected = 1,
    Reconnecting = 2,
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
    ice_info: Arc<Mutex<Option<IceInfo>>>,
}

impl SharedConnState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(ConnState::Connecting as u8)),
            ice_info: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn set(&self, state: ConnState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn set_ice_info(&self, info: IceInfo) {
        *self.ice_info.lock().unwrap() = Some(info);
    }

    pub fn get_ice_info(&self) -> Option<IceInfo> {
        self.ice_info.lock().unwrap().clone()
    }
}
