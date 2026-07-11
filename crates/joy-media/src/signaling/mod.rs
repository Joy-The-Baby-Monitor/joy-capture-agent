//! Signaling: the SDP/ICE exchange that bootstraps a WebRTC media session.
//!
//! Two layers live here, deliberately separated:
//!
//! - **The protocol** — [`SignalMessage`], the versioned JSON messages
//!   exchanged with a client (design doc §8). This is a public, documented
//!   wire contract: the closed-source client apps and any third-party client
//!   speak exactly these shapes.
//! - **The transport seam** — the [`Signaling`] trait and
//!   [`SignalingSession`]. The peer manager consumes sessions without knowing
//!   how bytes move; v1 ships a LAN WebSocket server ([`ws::LanSignaling`]),
//!   and a v2 remote relay slots in behind the same trait without touching
//!   peer or client-view code (design doc §7.4.6).
//!
//! ## Connection flow (v1, LAN)
//!
//! One signaling session corresponds to one media session. The client opens
//! the transport, sends an [`SignalMessage::Offer`] with two `recvonly`
//! transceivers, and may trickle [`SignalMessage::Candidate`]s afterwards.
//! The agent answers once, non-trickle: it waits for its own (instant, LAN
//! host-only) ICE gathering to finish and sends a single complete
//! [`SignalMessage::Answer`]. Renegotiation is not supported; a second offer
//! on the same session is a protocol error.

pub mod ws;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// The protocol version this agent speaks. Every message carries the version
/// so newer clients and agents can detect mismatch and degrade gracefully.
pub const PROTOCOL_VERSION: u32 = 1;

/// A signaling message, JSON-encoded on the wire with a `type` tag.
///
/// Wire shapes (version 1):
///
/// ```json
/// {"type":"offer","v":1,"sdp":"v=0\r\n..."}
/// {"type":"answer","v":1,"sdp":"v=0\r\n..."}
/// {"type":"candidate","v":1,"candidate":"candidate:...","sdp_mid":"0","sdp_mline_index":0}
/// {"type":"error","v":1,"code":"bad_version","message":"agent speaks protocol v1"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// Client → agent: an SDP offer opening a media session.
    Offer {
        /// Protocol version; must be [`PROTOCOL_VERSION`].
        v: u32,
        /// The client's SDP offer.
        sdp: String,
    },

    /// Agent → client: the SDP answer. Complete — every agent ICE candidate
    /// is inline, so no agent-side trickle follows.
    Answer {
        /// Protocol version.
        v: u32,
        /// The agent's SDP answer.
        sdp: String,
    },

    /// Client → agent: one trickled ICE candidate discovered after the offer.
    Candidate {
        /// Protocol version.
        v: u32,
        /// The `candidate:...` attribute line.
        candidate: String,
        /// The media-section identification tag the candidate belongs to.
        sdp_mid: Option<String>,
        /// The index of the media section the candidate belongs to.
        sdp_mline_index: Option<u16>,
    },

    /// Agent → client: a terminal protocol error; the agent closes the
    /// session immediately after sending it.
    Error {
        /// Protocol version.
        v: u32,
        /// Machine-readable error code (`bad_version`, `bad_message`,
        /// `renegotiation_unsupported`, `peer_failed`).
        code: String,
        /// Human-readable detail for logs and debugging.
        message: String,
    },
}

impl SignalMessage {
    /// The protocol version carried by this message.
    pub fn version(&self) -> u32 {
        match self {
            Self::Offer { v, .. }
            | Self::Answer { v, .. }
            | Self::Candidate { v, .. }
            | Self::Error { v, .. } => *v,
        }
    }

    /// Builds an [`SignalMessage::Error`] at the current protocol version.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            v: PROTOCOL_VERSION,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// One client's bidirectional signaling channel, transport-agnostic.
///
/// Both halves are bounded mpsc channels bridged to the real transport by
/// whatever produced the session. A closed `rx` means the client is gone; a
/// failed `tx.send` means the transport is gone. Either way the media session
/// tied to this signaling session should be torn down.
#[derive(Debug)]
pub struct SignalingSession {
    /// Messages arriving from the client.
    pub rx: mpsc::Receiver<SignalMessage>,
    /// Messages to deliver to the client.
    pub tx: mpsc::Sender<SignalMessage>,
}

/// A source of client signaling sessions.
///
/// v1: a WebSocket server on the LAN. v2: a cloud relay. The peer manager
/// only ever calls [`Signaling::accept`] in a loop, so swapping transports —
/// or serving several at once — never touches session handling.
pub trait Signaling: Send {
    /// Waits for the next client to open a signaling session.
    ///
    /// Returns `None` when the transport has shut down and no further
    /// sessions will arrive.
    fn accept(&mut self) -> impl Future<Output = Option<SignalingSession>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_wire_shape() {
        let msg = SignalMessage::Offer {
            v: 1,
            sdp: "v=0\r\n".into(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"offer","v":1,"sdp":"v=0\r\n"}"#);
        let back: SignalMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);
    }

    #[test]
    fn answer_wire_shape() {
        let msg = SignalMessage::Answer {
            v: 1,
            sdp: "v=0\r\n".into(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"answer","v":1,"sdp":"v=0\r\n"}"#);
    }

    #[test]
    fn candidate_wire_shape() {
        let json = r#"{"type":"candidate","v":1,"candidate":"candidate:1 1 UDP 2122252543 192.168.1.2 50000 typ host","sdp_mid":"0","sdp_mline_index":0}"#;
        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        match &msg {
            SignalMessage::Candidate {
                v,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                assert_eq!(*v, 1);
                assert!(candidate.starts_with("candidate:1"));
                assert_eq!(sdp_mid.as_deref(), Some("0"));
                assert_eq!(*sdp_mline_index, Some(0));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn candidate_tolerates_missing_mid_and_index() {
        let json = r#"{"type":"candidate","v":1,"candidate":"candidate:..."}"#;
        let msg: SignalMessage = serde_json::from_str(json).expect("deserialize");
        match msg {
            SignalMessage::Candidate {
                sdp_mid,
                sdp_mline_index,
                ..
            } => {
                assert_eq!(sdp_mid, None);
                assert_eq!(sdp_mline_index, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn error_wire_shape() {
        let msg = SignalMessage::error("bad_version", "agent speaks protocol v1");
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"error","v":1,"code":"bad_version","message":"agent speaks protocol v1"}"#
        );
    }

    #[test]
    fn unknown_type_is_rejected() {
        let result = serde_json::from_str::<SignalMessage>(r#"{"type":"subscribe","v":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn version_accessor_reads_every_variant() {
        let messages = [
            SignalMessage::Offer {
                v: 3,
                sdp: String::new(),
            },
            SignalMessage::Answer {
                v: 3,
                sdp: String::new(),
            },
            SignalMessage::Candidate {
                v: 3,
                candidate: String::new(),
                sdp_mid: None,
                sdp_mline_index: None,
            },
            SignalMessage::Error {
                v: 3,
                code: String::new(),
                message: String::new(),
            },
        ];
        assert!(messages.iter().all(|m| m.version() == 3));
    }
}
