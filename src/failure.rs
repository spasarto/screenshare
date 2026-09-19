//! Turns whatever ended a session into something a person can act on.
//!
//! Session threads fail with an `anyhow` chain like "Failed to contact
//! signaling server: error sending request for url (…): dns error", which
//! used to go straight onto the error screen. [`SessionFailure`] classifies
//! that chain into a [`FailureKind`] — each with its own sentence and its own
//! way forward — and keeps the raw chain as `details`, for the error
//! screen's Details disclosure and the log.

use crate::app::Role;
use crate::signaling_server::SignalingError;

/// A step of session setup that can fail for reasons of its own, attached
/// as context so classification knows where in the setup the chain came
/// from without matching on message text.
#[derive(Debug, Clone, Copy)]
pub enum Step {
    /// Creating the WebRTC peer connection and exchanging SDP with it.
    PeerSetup,
    /// Starting Windows Graphics Capture on the chosen target.
    Capture,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::PeerSetup => write!(f, "Setting up the peer connection"),
            Step::Capture => write!(f, "Starting capture"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The signaling server's name didn't resolve: almost always no network.
    Offline,
    /// Resolved, but no connection or no reply.
    ServerUnreachable,
    /// The server answered with an error it didn't explain.
    ServerError(u16),
    RateLimited,
    PinInUse,
    /// Joining a code nobody opened.
    CodeNotFound,
    /// The session vanished partway through.
    SessionExpired,
    PeerTimedOut,
    Undecryptable,
    /// WebRTC setup failed after signaling.
    ConnectionFailed,
    CaptureFailed,
    Unknown,
}

/// What the error screen offers as the way forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Run the same session again: same role, same share, same code.
    Retry,
    /// Host again, under a freshly generated code.
    NewCode,
    /// Back to the Join screen with the code still filled in.
    CheckCode,
    /// Back to the picker to choose a different window or screen.
    PickAnother,
    OpenSettings,
}

impl Recovery {
    pub fn label(self) -> &'static str {
        match self {
            Recovery::Retry => "Try again",
            Recovery::NewCode => "Start with a new code",
            Recovery::CheckCode => "Check the code",
            Recovery::PickAnother => "Pick something else",
            Recovery::OpenSettings => "Server settings",
        }
    }
}

pub struct SessionFailure {
    pub kind: FailureKind,
    /// Which side of the session this happened on; some explanations differ.
    pub role: Role,
    /// The full error chain, for the Details disclosure.
    pub details: String,
}

impl SessionFailure {
    pub fn new(error: &anyhow::Error, role: Role) -> Self {
        Self {
            kind: classify(error),
            role,
            details: format!("{error:#}"),
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            FailureKind::Offline => "You appear to be offline",
            FailureKind::ServerUnreachable => "Couldn't reach the signaling server",
            FailureKind::ServerError(_) => "The signaling server had a problem",
            FailureKind::RateLimited => "Too many attempts",
            FailureKind::PinInUse => "That code is already in use",
            FailureKind::CodeNotFound => "No session with that code",
            FailureKind::SessionExpired => "The session has expired",
            FailureKind::PeerTimedOut => match self.role {
                Role::Offerer => "Nobody joined",
                Role::Answerer => "The host never finished setting up",
            },
            FailureKind::Undecryptable => "The session couldn't be verified",
            FailureKind::ConnectionFailed => "Couldn't set up the connection",
            FailureKind::CaptureFailed => "Couldn't capture what you picked",
            FailureKind::Unknown => "Something went wrong",
        }
    }

    pub fn explanation(&self) -> String {
        match self.kind {
            FailureKind::Offline => "Screen Share couldn't look up the signaling server's address. \
                 Check your internet connection, then try again."
                .into(),
            FailureKind::ServerUnreachable => "The server didn't respond. It may be down, or a \
                 firewall or proxy may be blocking it. If you use your own server, check its \
                 address in settings."
                .into(),
            FailureKind::ServerError(status) => format!(
                "It answered with an error (HTTP {status}). This is usually temporary."
            ),
            FailureKind::RateLimited => "The server limits how often one network can try codes, \
                 to stop guessing. Wait a minute, then try again."
                .into(),
            FailureKind::PinInUse => "Another session is already using this code. Start with a \
                 new code and send that one to your peer instead."
                .into(),
            FailureKind::CodeNotFound => "Nobody opened a session with this code in the last five \
                 minutes. Check it with your peer \u{2014} it's six digits, then four letters \
                 \u{2014} or ask them to start a new session."
                .into(),
            FailureKind::SessionExpired => match self.role {
                Role::Offerer => "The server dropped the session before your peer connected. \
                     Start again to get a working code."
                    .into(),
                Role::Answerer => "The session went away before you connected \u{2014} the host \
                     may have cancelled it. Ask them for a new code."
                    .into(),
            },
            FailureKind::PeerTimedOut => match self.role {
                Role::Offerer => "Your code went unused for five minutes. Try again, and send \
                     your peer the new code or link."
                    .into(),
                Role::Answerer => "The session exists, but the host's side never became ready. \
                     Ask them to check their screen, then try again."
                    .into(),
            },
            FailureKind::Undecryptable => "Something answered on this code, but not with data \
                 that matches it. The code may have been mistyped on the other side, or someone \
                 else may be trying codes. Starting with a new code is the safe choice."
                .into(),
            FailureKind::ConnectionFailed => "You both reached the server, but the direct \
                 connection couldn't be set up. If Windows asked whether Screen Share could use \
                 the network, allowing it usually fixes this."
                .into(),
            FailureKind::CaptureFailed => "Windows wouldn't start capturing it. The window may \
                 have closed, or it may block screen capture."
                .into(),
            FailureKind::Unknown => "The session stopped unexpectedly. The details below say \
                 what failed."
                .into(),
        }
    }

    /// The way forward to offer first, then an optional second one. "Back
    /// to setup" is always offered alongside these.
    pub fn recoveries(&self) -> (Recovery, Option<Recovery>) {
        match self.kind {
            FailureKind::Offline | FailureKind::RateLimited | FailureKind::ServerError(_) => {
                (Recovery::Retry, None)
            }
            FailureKind::ServerUnreachable => (Recovery::Retry, Some(Recovery::OpenSettings)),
            FailureKind::PinInUse | FailureKind::Undecryptable => match self.role {
                Role::Offerer => (Recovery::NewCode, None),
                Role::Answerer => (Recovery::CheckCode, None),
            },
            FailureKind::CodeNotFound => (Recovery::CheckCode, Some(Recovery::Retry)),
            FailureKind::SessionExpired => match self.role {
                Role::Offerer => (Recovery::NewCode, None),
                Role::Answerer => (Recovery::CheckCode, Some(Recovery::Retry)),
            },
            FailureKind::PeerTimedOut | FailureKind::ConnectionFailed | FailureKind::Unknown => {
                (Recovery::Retry, None)
            }
            FailureKind::CaptureFailed => (Recovery::PickAnother, None),
        }
    }
}

/// Finds a `T` anywhere in the chain: as context (which `downcast_ref`
/// sees through) or as the source of some other error (which only walking
/// `chain` reaches).
fn find<T: std::error::Error + Send + Sync + 'static>(error: &anyhow::Error) -> Option<&T> {
    error
        .downcast_ref::<T>()
        .or_else(|| error.chain().find_map(|e| e.downcast_ref::<T>()))
}

fn classify(error: &anyhow::Error) -> FailureKind {
    if let Some(signaling) = find::<SignalingError>(error) {
        return match *signaling {
            SignalingError::PinInUse => FailureKind::PinInUse,
            SignalingError::RateLimited => FailureKind::RateLimited,
            SignalingError::SessionNotFound => FailureKind::CodeNotFound,
            SignalingError::SessionGone => FailureKind::SessionExpired,
            SignalingError::PeerTimedOut => FailureKind::PeerTimedOut,
            SignalingError::Undecryptable => FailureKind::Undecryptable,
            SignalingError::Status(code) => FailureKind::ServerError(code),
        };
    }

    if let Some(http) = error.chain().find_map(|e| e.downcast_ref::<reqwest::Error>()) {
        // reqwest doesn't expose "the name didn't resolve" as its own kind;
        // hyper's resolver error only shows up in the source chain's text.
        let dns = error.chain().any(|e| {
            let msg = e.to_string().to_lowercase();
            msg.contains("dns error") || msg.contains("no such host")
        });
        return if dns {
            FailureKind::Offline
        } else if http.is_connect() || http.is_timeout() {
            FailureKind::ServerUnreachable
        } else if let Some(status) = http.status() {
            FailureKind::ServerError(status.as_u16())
        } else {
            FailureKind::ServerUnreachable
        };
    }

    // Only ever attached as context, which `downcast_ref` sees at any depth.
    match error.downcast_ref::<Step>() {
        Some(Step::Capture) => FailureKind::CaptureFailed,
        Some(Step::PeerSetup) => FailureKind::ConnectionFailed,
        None => FailureKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    fn kind(error: anyhow::Error) -> FailureKind {
        SessionFailure::new(&error, Role::Answerer).kind
    }

    #[test]
    fn signaling_errors_are_found_under_context() {
        let e = anyhow::Error::new(SignalingError::RateLimited)
            .context("Poll offer failed (429): Too many requests");
        assert_eq!(kind(e), FailureKind::RateLimited);
    }

    #[test]
    fn a_bailed_signaling_error_keeps_its_type() {
        let e: anyhow::Error = anyhow::anyhow!(SignalingError::SessionNotFound);
        assert_eq!(kind(e), FailureKind::CodeNotFound);
    }

    #[test]
    fn steps_classify_what_failed() {
        let e = anyhow::anyhow!("Failed to start capture: access denied").context(Step::Capture);
        assert_eq!(kind(e), FailureKind::CaptureFailed);
        let e: anyhow::Result<()> = Err(anyhow::anyhow!("ICE gathering failed"));
        let e = e.context(Step::PeerSetup).context("outer").unwrap_err();
        assert_eq!(kind(e), FailureKind::ConnectionFailed);
    }

    #[test]
    fn anything_else_is_unknown_but_keeps_its_details() {
        let e = anyhow::anyhow!("boom").context("while doing a thing");
        let failure = SessionFailure::new(&e, Role::Offerer);
        assert_eq!(failure.kind, FailureKind::Unknown);
        assert_eq!(failure.details, "while doing a thing: boom");
    }
}
