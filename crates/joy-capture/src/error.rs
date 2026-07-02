//! Error type shared by every capture backend.

/// Convenience alias used throughout `joy-capture`.
pub type Result<T> = std::result::Result<T, CaptureError>;

/// The unified error type for opening and running capture sources.
///
/// Backend-specific failures (nokhwa, cpal, V4L2 ioctls, …) are flattened into
/// [`CaptureError::Backend`] as strings rather than wrapped as typed sources —
/// callers upstream of the HAL have no meaningful way to match on a platform
/// library's error variants, and keeping the underlying error types out of the
/// public API is exactly what the abstraction is for.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// The requested capture device does not exist or is not connected.
    #[error("capture device not found: {0}")]
    DeviceNotFound(String),

    /// The device exists but cannot satisfy the requested configuration
    /// (resolution, frame rate, sample rate, channel count, …).
    #[error("unsupported capture configuration: {0}")]
    UnsupportedConfig(String),

    /// A platform/backend library reported a failure while opening the device
    /// or during streaming.
    #[error("capture backend error: {0}")]
    Backend(String),

    /// The source's background capture task has stopped and no more data will
    /// be produced.
    #[error("capture source closed")]
    Closed,
}
