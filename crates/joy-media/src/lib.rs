//! Media plane.
//!
//! `joy-media` turns captured frames into a live stream: pixel conversion to
//! I420, software H.264 encode (openh264) and Opus encode, one `webrtc-rs`
//! peer connection per connected client fed from a single shared encoder per
//! media kind, and SDP/ICE signaling behind a [`Signaling`] seam so a remote
//! relay can be added later without touching view or pairing code.
//!
//! [`Signaling`]: signaling::Signaling

pub mod convert;
pub mod encode;
pub mod error;
pub mod peer;
pub mod signaling;

pub use convert::{I420Buffer, to_i420};
pub use encode::{KeyframeRequester, VideoEncodeConfig, run_audio_encoder, run_video_encoder};
pub use error::{MediaError, Result};
pub use peer::StreamHub;
pub use signaling::{PROTOCOL_VERSION, SignalMessage, Signaling, SignalingSession};
