//! Media plane.
//!
//! `joy-media` turns captured frames into a live stream: UVC passthrough when
//! the camera emits a WebRTC-carriable format, software encode otherwise, Opus
//! for audio, and one `webrtc-rs` peer connection per connected client. It also
//! owns SDP/ICE signaling behind a `Signaling` seam so a remote relay can be
//! added later without touching view or pairing code.
//!
//! This crate is currently a scaffold; the encoder and WebRTC peer land during
//! the local-live-view milestone.
