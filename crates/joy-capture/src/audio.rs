//! The audio half of the capture HAL: chunk/config/capability types and the
//! [`AudioSource`] trait that every audio backend implements.

use std::sync::Arc;
use std::time::Duration;

use joy_core::Timestamp;
use tokio::sync::broadcast;

use crate::error::Result;

/// Capacity of the bounded broadcast channel an [`AudioSource`] publishes on.
///
/// Larger than the video channel because chunks are small and analyzers (VAD,
/// RMS) prefer short gaps over dropped speech onsets — but still bounded, and
/// still drop-oldest under pressure, per the pipeline's real-time rule.
pub const AUDIO_CHANNEL_CAPACITY: usize = 32;

/// A fixed-duration run of interleaved PCM samples.
///
/// Samples are `f32` in `[-1.0, 1.0]`, interleaved by channel (all channels of
/// frame 0, then frame 1, …). The payload is an `Arc<[f32]>`, so cloning a
/// chunk through the broadcast fan-out is a reference-count bump, not a copy.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Monotonic capture time of the *first* sample in the chunk.
    pub ts: Timestamp,
    /// Sample rate in Hz that these samples were captured at.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Interleaved PCM payload; length is `frames × channels`.
    pub samples: Arc<[f32]>,
}

impl AudioChunk {
    /// Number of sample frames in the chunk (one frame = one sample per
    /// channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// Wall duration of audio this chunk covers.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames() as f64 / self.sample_rate.max(1) as f64)
    }

    /// Root-mean-square level of the chunk across all channels, in
    /// `[0.0, 1.0]`. A cheap loudness measure used by the sound analyzer and
    /// handy for smoke-testing that a microphone is actually hearing.
    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = self.samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
        (sum_sq / self.samples.len() as f64).sqrt() as f32
    }
}

/// Requested audio capture configuration.
///
/// As with video, hardware backends treat this as a request and may open a
/// different rate/channel count if the device can't satisfy it; the actual
/// values are reported on every [`AudioChunk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioConfig {
    /// Backend-specific device selector (device name). `None` selects the
    /// platform default input.
    pub device: Option<String>,
    /// Requested sample rate in Hz.
    pub sample_rate: u32,
    /// Requested channel count.
    pub channels: u16,
    /// Frames per published [`AudioChunk`].
    pub chunk_frames: usize,
}

impl Default for AudioConfig {
    /// 48 kHz mono in 960-frame (20 ms) chunks — Opus's native rate and frame
    /// size, so chunks map 1:1 onto encoder frames later.
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: 48_000,
            channels: 1,
            chunk_frames: 960,
        }
    }
}

/// One capture mode range an audio device supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMode {
    pub channels: u16,
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
}

/// What an audio source can do: the mode ranges it can capture in.
#[derive(Debug, Clone, Default)]
pub struct AudioCaps {
    pub modes: Vec<AudioMode>,
}

/// A source of live PCM audio — the audio half of the capture HAL.
///
/// Same lifecycle and channel semantics as
/// [`VideoSource`](crate::video::VideoSource): capture starts at `open`, stops
/// on drop, and chunks fan out on a bounded drop-oldest broadcast channel of
/// capacity [`AUDIO_CHANNEL_CAPACITY`].
///
/// # Contract
///
/// - Chunk timestamps mark the capture time of the chunk's first sample and
///   increase strictly monotonically.
/// - `sample_rate`/`channels` on published chunks reflect what the device
///   actually opened at, which may differ from the requested config.
pub trait AudioSource: Send {
    /// Opens the input device (or synthesizer) described by `cfg` and starts
    /// capturing in the background.
    fn open(cfg: &AudioConfig) -> impl Future<Output = Result<Self>> + Send
    where
        Self: Sized;

    /// Returns a new receiver on the source's chunk fan-out.
    fn subscribe(&self) -> broadcast::Receiver<AudioChunk>;

    /// Returns the capture modes this source supports, probed at open time.
    fn capabilities(&self) -> AudioCaps;
}
