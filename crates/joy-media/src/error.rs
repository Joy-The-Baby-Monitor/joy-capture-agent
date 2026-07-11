//! Error type shared across the media plane.

/// Convenience alias used throughout `joy-media`.
pub type Result<T> = std::result::Result<T, MediaError>;

/// The unified error type for pixel conversion, encoding, signaling, and
/// WebRTC peer management.
///
/// Failures from the underlying native libraries (openh264, libopus,
/// webrtc-rs) are flattened into strings rather than wrapped as typed
/// sources, mirroring `joy-capture`'s convention: callers upstream have no
/// meaningful way to match on a codec library's error variants, and keeping
/// those types out of the public API is what the crate boundary is for.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MediaError {
    /// A captured frame could not be converted to the encoder's input format
    /// (unsupported pixel format, malformed payload, odd dimensions, …).
    #[error("pixel conversion failed: {0}")]
    Convert(String),

    /// The video or audio encoder rejected its configuration or failed while
    /// encoding.
    #[error("encoder error: {0}")]
    Encode(String),

    /// A signaling message could not be exchanged with the client (transport
    /// closed, malformed payload, protocol violation).
    #[error("signaling error: {0}")]
    Signaling(String),

    /// The WebRTC stack reported a failure while negotiating or running a
    /// peer connection.
    #[error("webrtc error: {0}")]
    Peer(String),

    /// The upstream capture source has closed; no more media will arrive.
    #[error("media source closed")]
    Closed,
}
