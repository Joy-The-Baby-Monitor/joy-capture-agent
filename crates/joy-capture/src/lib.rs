//! Capture hardware abstraction layer.
//!
//! `joy-capture` owns the [`VideoSource`]/[`AudioSource`] trait boundary and
//! its platform backends. The seam keeps `ioctl`-level hardware details out of
//! the encoder, analyzers, and extension host, and buys headless testing and
//! tolerance to hardware swaps (design §5.2).
//!
//! # Backends
//!
//! | Backend | Type | Platforms | Cargo feature |
//! |---|---|---|---|
//! | [`simulate::SimulatedVideoSource`] | synthetic frames | all | always available |
//! | [`simulate::SimulatedAudioSource`] | synthetic tone | all | always available |
//! | [`camera::CameraVideoSource`] | nokhwa (AVFoundation / V4L2) | macOS, Linux | `camera` (default) |
//! | [`microphone::MicrophoneAudioSource`] | cpal (CoreAudio / ALSA) | macOS, Linux | `microphone` (default) |
//!
//! Build with `--no-default-features` for a hardware-free crate (CI runners,
//! containers) — the simulate sources still provide the full pipeline.
//!
//! # Real-time semantics
//!
//! Every source publishes onto a **bounded broadcast channel with drop-oldest
//! behavior** ([`VIDEO_CHANNEL_CAPACITY`] / [`AUDIO_CHANNEL_CAPACITY`]). This
//! is the pipeline's core rule: under pressure, drop old data rather than
//! block the producer or grow without bound. Consumers each hold their own
//! subscription and are mutually isolated — a slow consumer sees
//! [`tokio::sync::broadcast::error::RecvError::Lagged`] and skips forward,
//! degrading only itself.
//!
//! # Time discipline
//!
//! Every frame and chunk carries a monotonic [`joy_core::Timestamp`] stamped
//! at the source. A/V sync, event correlation, and RTP timestamps all derive
//! from these stamps.

pub mod audio;
pub mod error;
pub mod simulate;
pub mod video;

#[cfg(feature = "camera")]
pub mod camera;
#[cfg(feature = "microphone")]
pub mod microphone;

pub use audio::{
    AUDIO_CHANNEL_CAPACITY, AudioCaps, AudioChunk, AudioConfig, AudioMode, AudioSource,
};
pub use error::{CaptureError, Result};
pub use simulate::{SimulatedAudioSource, SimulatedVideoSource};
pub use video::{
    PixelFormat, VIDEO_CHANNEL_CAPACITY, VideoCaps, VideoConfig, VideoFrame, VideoMode,
    VideoSource,
};

#[cfg(feature = "camera")]
pub use camera::CameraVideoSource;
#[cfg(feature = "microphone")]
pub use microphone::MicrophoneAudioSource;
