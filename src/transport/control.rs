//! In-band session control messages: the peers telling each other about
//! things that aren't video or ICE-restart signaling.
//!
//! Wire format: `[MSG_CONTROL][kind][payload]`, carried on the same data
//! channel as everything else (see [`super::framing`]). A peer that predates
//! a message kind drops the frame as unrecognised, so adding kinds is
//! backwards compatible — the older side just misses the courtesy.

use super::framing::MSG_CONTROL;

const KIND_BYE: u8 = 0x01;
const KIND_PAUSED: u8 = 0x02;
const KIND_RESUMED: u8 = 0x03;
const KIND_PING: u8 = 0x04;
const KIND_PONG: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The sender is hanging up on purpose. Lets the other side say "your
    /// peer ended the session" straight away, instead of spending minutes on
    /// ICE restarts against a connection nobody is on the other end of.
    Bye,
    /// The sender stopped sending frames; whatever the receiver has on
    /// screen is now a still.
    Paused,
    Resumed,
    /// Round-trip probe. The payload is opaque to the receiver, which echoes
    /// it back in a `Pong`; the sender uses it to find its own send time.
    Ping(u64),
    Pong(u64),
}

impl Control {
    pub fn encode(self) -> Vec<u8> {
        let (kind, payload) = match self {
            Control::Bye => (KIND_BYE, None),
            Control::Paused => (KIND_PAUSED, None),
            Control::Resumed => (KIND_RESUMED, None),
            Control::Ping(n) => (KIND_PING, Some(n)),
            Control::Pong(n) => (KIND_PONG, Some(n)),
        };
        let mut msg = vec![MSG_CONTROL, kind];
        if let Some(n) = payload {
            msg.extend_from_slice(&n.to_be_bytes());
        }
        msg
    }

    /// Decodes a control message body (the bytes after `MSG_CONTROL`).
    /// `None` for a kind this build doesn't know or a truncated payload.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let (&kind, rest) = body.split_first()?;
        let number = || -> Option<u64> { Some(u64::from_be_bytes(rest.get(..8)?.try_into().ok()?)) };
        Some(match kind {
            KIND_BYE => Control::Bye,
            KIND_PAUSED => Control::Paused,
            KIND_RESUMED => Control::Resumed,
            KIND_PING => Control::Ping(number()?),
            KIND_PONG => Control::Pong(number()?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        for msg in [
            Control::Bye,
            Control::Paused,
            Control::Resumed,
            Control::Ping(0x0102_0304_0506_0708),
            Control::Pong(u64::MAX),
        ] {
            let wire = msg.encode();
            assert_eq!(wire[0], MSG_CONTROL);
            assert_eq!(Control::decode(&wire[1..]), Some(msg));
        }
    }

    #[test]
    fn unknown_kinds_and_truncated_payloads_are_ignored() {
        assert_eq!(Control::decode(&[]), None);
        assert_eq!(Control::decode(&[0x7F]), None);
        assert_eq!(Control::decode(&[KIND_PING, 1, 2, 3]), None);
    }
}
