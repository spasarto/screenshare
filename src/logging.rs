//! Keeps recent log output in memory for the in-app log drawer.
//!
//! Release builds are a windowed app with no console, and the old way to
//! see anything was an undocumented F10 that allocated a Win32 console.
//! [`layer`] is a `tracing` layer that records every event into a bounded
//! [`LogBuffer`] the viewer draws from, so the log is always there, in any
//! build, one click away.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;

/// Oldest lines are dropped past this, so a long session can't grow the
/// buffer without bound.
const CAPACITY: usize = 5000;

pub struct LogLine {
    /// Since the app started. Wall-clock time would need a timezone to be
    /// honest; "how long into the run" is what reading a log needs anyway.
    pub at: Duration,
    pub level: Level,
    pub target: String,
    pub message: String,
}

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
    started: Instant,
}

struct Inner {
    lines: VecDeque<LogLine>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::with_capacity(CAPACITY),
            })),
            started: Instant::now(),
        }
    }

    fn push(&self, line: LogLine) {
        let mut inner = self.inner.lock().unwrap();
        if inner.lines.len() == CAPACITY {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line);
    }

    /// Runs `f` over the buffered lines, oldest first, under the lock.
    pub fn with_lines<R>(&self, f: impl FnOnce(&VecDeque<LogLine>) -> R) -> R {
        f(&self.inner.lock().unwrap().lines)
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().lines.clear();
    }
}

impl LogLine {
    /// One line of plain text, as copied out of the drawer.
    pub fn to_text(&self) -> String {
        format!(
            "{} {:>5} {}: {}",
            format_offset(self.at),
            self.level,
            self.target,
            self.message
        )
    }
}

/// `mm:ss.mmm`, rolling into hours past the first.
pub fn format_offset(d: Duration) -> String {
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}.{millis:03}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{:02}:{:02}.{millis:03}", secs / 60, secs % 60)
    }
}

pub fn layer(buffer: LogBuffer) -> BufferLayer {
    BufferLayer { buffer }
}

pub struct BufferLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        self.buffer.push(LogLine {
            at: self.buffer.started.elapsed(),
            level: *meta.level(),
            target: meta.target().to_owned(),
            message: visitor.finish(),
        });
    }
}

/// Collects an event's message, then its other fields as `key=value`, the
/// way the console formatter lays them out.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (_, true) => self.message,
            (true, false) => self.fields,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[test]
    fn events_land_in_the_buffer_with_their_fields() {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(layer(buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(attempt = 3, "Reconnecting");
        });
        buffer.with_lines(|lines| {
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].level, Level::WARN);
            assert_eq!(lines[0].message, "Reconnecting attempt=3");
        });
    }

    #[test]
    fn offsets_read_as_minutes_then_hours() {
        assert_eq!(format_offset(Duration::from_millis(65_250)), "01:05.250");
        assert_eq!(format_offset(Duration::from_secs(3723)), "1:02:03.000");
    }
}
