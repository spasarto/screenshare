pub mod framing;
pub mod peer;
pub mod signaling;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

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

/// Thread-safe shared connection state.
#[derive(Clone)]
pub struct SharedConnState(Arc<AtomicU8>);

impl SharedConnState {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(ConnState::Connecting as u8)))
    }

    pub fn get(&self) -> ConnState {
        ConnState::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, state: ConnState) {
        self.0.store(state as u8, Ordering::Relaxed);
    }
}
