//! Synthetic capture sources for the `--simulate` mode.
//!
//! These are full [`VideoSource`]/[`AudioSource`] implementations that need no
//! hardware: the video source renders a moving bright square over a dark
//! background (deliberately motion-detector-friendly for the analysis
//! milestone), and the audio source synthesizes a pulsing sine tone. They run
//! on both dev (macOS) and prod (Pi) unconditionally, which makes them the
//! backbone of headless testing and CI.

use std::f64::consts::TAU;
use std::time::Duration;

use bytes::Bytes;
use joy_core::Timestamp;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::audio::{
    AUDIO_CHANNEL_CAPACITY, AudioCaps, AudioChunk, AudioConfig, AudioMode, AudioSource,
};
use crate::error::{CaptureError, Result};
use crate::video::{
    PixelFormat, VIDEO_CHANNEL_CAPACITY, VideoCaps, VideoConfig, VideoFrame, VideoMode, VideoSource,
};

/// Frequency of the simulated audio tone.
const SIM_TONE_HZ: f64 = 440.0;

/// Peak amplitude of the simulated tone (kept well under full scale).
const SIM_TONE_AMPLITUDE: f64 = 0.25;

/// Period of the tone's on/off pulse, so RMS visibly rises and falls — gives
/// the future sound analyzer (and a human watching probe output) something to
/// detect.
const SIM_TONE_PULSE: Duration = Duration::from_secs(2);

/// A synthetic [`VideoSource`] that renders RGB24 test frames at the
/// configured resolution and frame rate.
///
/// Each frame is a dark background with a bright square whose position
/// advances every frame, so consecutive frames always differ — exactly what
/// the frame-differencing motion analyzer needs to see.
pub struct SimulatedVideoSource {
    tx: broadcast::Sender<VideoFrame>,
    caps: VideoCaps,
    task: JoinHandle<()>,
}

impl VideoSource for SimulatedVideoSource {
    async fn open(cfg: &VideoConfig) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            return Err(CaptureError::UnsupportedConfig(format!(
                "simulated video needs nonzero dimensions and fps, got {}x{}@{}",
                cfg.width, cfg.height, cfg.fps
            )));
        }

        let (tx, _) = broadcast::channel(VIDEO_CHANNEL_CAPACITY);
        let caps = VideoCaps {
            modes: vec![VideoMode {
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                format: PixelFormat::Rgb24,
            }],
        };

        let task = tokio::spawn(render_loop(cfg.clone(), tx.clone()));
        Ok(Self { tx, caps, task })
    }

    fn subscribe(&self) -> broadcast::Receiver<VideoFrame> {
        self.tx.subscribe()
    }

    fn capabilities(&self) -> VideoCaps {
        self.caps.clone()
    }
}

impl Drop for SimulatedVideoSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Renders frames at the configured rate until the owning source is dropped.
async fn render_loop(cfg: VideoConfig, tx: broadcast::Sender<VideoFrame>) {
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / cfg.fps as f64));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame_index: u64 = 0;

    loop {
        ticker.tick().await;
        let frame = VideoFrame {
            ts: Timestamp::now(),
            format: PixelFormat::Rgb24,
            width: cfg.width,
            height: cfg.height,
            data: render_pattern(cfg.width, cfg.height, frame_index),
        };
        frame_index = frame_index.wrapping_add(1);
        let _ = tx.send(frame);
    }
}

/// Draws the test pattern: a dark gray field with a bright square that steps
/// diagonally across the frame, wrapping at the edges.
fn render_pattern(width: u32, height: u32, frame_index: u64) -> Bytes {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![24u8; w * h * 3];

    let side = (w.min(h) / 8).max(1);
    let step = (side / 2).max(1);
    let x0 = (frame_index as usize * step) % w.saturating_sub(side).max(1);
    let y0 = (frame_index as usize * step) % h.saturating_sub(side).max(1);

    for y in y0..(y0 + side).min(h) {
        let row = (y * w + x0) * 3;
        data[row..row + side.min(w - x0) * 3].fill(230);
    }

    Bytes::from(data)
}

/// A synthetic [`AudioSource`] that emits a pulsing sine tone as interleaved
/// `f32` PCM at exactly the configured rate, channel count, and chunk size.
///
/// Chunk timestamps are derived arithmetically — `stream_anchor +
/// frames_emitted / sample_rate` — rather than read from the clock per chunk,
/// so they are drift-free and advance by exactly one chunk duration each time.
/// This is the "time discipline" pattern hardware backends approximate.
pub struct SimulatedAudioSource {
    tx: broadcast::Sender<AudioChunk>,
    caps: AudioCaps,
    task: JoinHandle<()>,
}

impl AudioSource for SimulatedAudioSource {
    async fn open(cfg: &AudioConfig) -> Result<Self> {
        if cfg.sample_rate == 0 || cfg.channels == 0 || cfg.chunk_frames == 0 {
            return Err(CaptureError::UnsupportedConfig(format!(
                "simulated audio needs nonzero rate/channels/chunk, got {} Hz, {} ch, {} frames",
                cfg.sample_rate, cfg.channels, cfg.chunk_frames
            )));
        }

        let (tx, _) = broadcast::channel(AUDIO_CHANNEL_CAPACITY);
        let caps = AudioCaps {
            modes: vec![AudioMode {
                channels: cfg.channels,
                min_sample_rate: cfg.sample_rate,
                max_sample_rate: cfg.sample_rate,
            }],
        };

        let task = tokio::spawn(synth_loop(cfg.clone(), tx.clone()));
        Ok(Self { tx, caps, task })
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioChunk> {
        self.tx.subscribe()
    }

    fn capabilities(&self) -> AudioCaps {
        self.caps.clone()
    }
}

impl Drop for SimulatedAudioSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Synthesizes chunks at the configured cadence until the source is dropped.
async fn synth_loop(cfg: AudioConfig, tx: broadcast::Sender<AudioChunk>) {
    let rate = cfg.sample_rate as f64;
    let channels = cfg.channels as usize;
    let chunk_duration = Duration::from_secs_f64(cfg.chunk_frames as f64 / rate);

    let mut ticker = tokio::time::interval(chunk_duration);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let anchor = Timestamp::now();
    let mut frames_emitted: u64 = 0;
    let phase_step = TAU * SIM_TONE_HZ / rate;
    let pulse_frames = (SIM_TONE_PULSE.as_secs_f64() * rate) as u64;

    loop {
        ticker.tick().await;

        let mut samples = vec![0.0f32; cfg.chunk_frames * channels];
        for frame in 0..cfg.chunk_frames {
            let n = frames_emitted + frame as u64;
            let audible = (n / pulse_frames.max(1)).is_multiple_of(2);
            let value = if audible {
                ((n as f64 * phase_step).sin() * SIM_TONE_AMPLITUDE) as f32
            } else {
                0.0
            };
            samples[frame * channels..(frame + 1) * channels].fill(value);
        }

        let ts =
            anchor + Duration::from_nanos(frames_emitted * 1_000_000_000 / cfg.sample_rate as u64);
        frames_emitted += cfg.chunk_frames as u64;

        let _ = tx.send(AudioChunk {
            ts,
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            samples: samples.into(),
        });
    }
}
