//! Capture hardware abstraction layer.
//!
//! `joy-capture` owns the `VideoSource`/`AudioSource` trait boundary and its
//! platform backends (V4L2/AVFoundation for video, ALSA/CoreAudio for audio),
//! plus a synthetic `--simulate` source for headless testing. The seam keeps
//! `ioctl`-level hardware details out of the encoder, analyzers, and extension
//! host.
//!
//! This crate is currently a scaffold; the trait and backend implementations
//! land as the pipeline is built out.
