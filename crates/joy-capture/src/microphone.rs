//! Real-microphone [`AudioSource`] backend built on `cpal`.
//!
//! One backend, two platforms: cpal fronts CoreAudio on macOS (dev) and ALSA
//! on Linux/Pi (prod).
//!
//! # Threading model
//!
//! The audio callback runs on a real-time OS thread where blocking and
//! allocation are forbidden, and `cpal::Stream` is not `Send` — so the layout
//! is, per design §5.2, callback → ring buffer → async drain:
//!
//! - A dedicated OS thread owns the stream. The cpal callback only copies
//!   samples into a lock-free [`ringbuf`] producer (overflow drops newest and
//!   bumps a counter — the producer is never blocked).
//! - A tokio task drains the ring buffer, slices it into fixed
//!   [`AudioConfig::chunk_frames`] chunks, stamps them, and publishes onto the
//!   broadcast channel.
//!
//! # Timestamps
//!
//! Chunk timestamps are derived as `anchor + frames_drained / sample_rate`,
//! where the anchor is taken when the first samples arrive. This keeps
//! consecutive chunks exactly one chunk-duration apart. Ring-buffer overflow
//! (a stalled drain) loses samples and therefore shifts subsequent timestamps
//! late by the amount lost; that is logged, and acceptable for a first pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use joy_core::Timestamp;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tokio::sync::{broadcast, oneshot};

use crate::audio::{
    AUDIO_CHANNEL_CAPACITY, AudioCaps, AudioChunk, AudioConfig, AudioMode, AudioSource,
};
use crate::error::{CaptureError, Result};

/// Ring buffer capacity, in seconds of audio, between the cpal callback and
/// the async drain task.
const RING_BUFFER_SECS: usize = 1;

/// What the stream thread reports back to `open` once the device is running.
struct StreamReady {
    caps: AudioCaps,
    consumer: HeapCons<f32>,
    sample_rate: u32,
    channels: u16,
    dropped: Arc<AtomicU64>,
}

/// An [`AudioSource`] backed by a physical input device via cpal
/// (CoreAudio / ALSA).
///
/// Opens the requested rate/channel count when the device supports it and
/// falls back to the device's default configuration otherwise; published
/// chunks always carry the actual values. Dropping the source stops the
/// stream and the drain task.
pub struct MicrophoneAudioSource {
    tx: broadcast::Sender<AudioChunk>,
    caps: AudioCaps,
    stop: Arc<AtomicBool>,
}

impl AudioSource for MicrophoneAudioSource {
    async fn open(cfg: &AudioConfig) -> Result<Self> {
        if cfg.chunk_frames == 0 {
            return Err(CaptureError::UnsupportedConfig(
                "chunk_frames must be nonzero".into(),
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();

        let thread_cfg = cfg.clone();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("joy-capture-mic".into())
            .spawn(move || stream_thread(thread_cfg, thread_stop, ready_tx))
            .map_err(|e| CaptureError::Backend(format!("failed to spawn stream thread: {e}")))?;

        let ready = ready_rx
            .await
            .map_err(|_| CaptureError::Backend("audio stream thread died during open".into()))??;

        let (tx, _) = broadcast::channel(AUDIO_CHANNEL_CAPACITY);
        let caps = ready.caps.clone();
        tokio::spawn(drain_loop(
            ready,
            cfg.chunk_frames,
            tx.clone(),
            Arc::clone(&stop),
        ));

        Ok(Self { tx, caps, stop })
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioChunk> {
        self.tx.subscribe()
    }

    fn capabilities(&self) -> AudioCaps {
        self.caps.clone()
    }
}

impl Drop for MicrophoneAudioSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Body of the dedicated stream thread: opens the device, builds the input
/// stream feeding the ring buffer, reports readiness, then keeps the stream
/// alive until stopped.
fn stream_thread(
    cfg: AudioConfig,
    stop: Arc<AtomicBool>,
    ready: oneshot::Sender<Result<StreamReady>>,
) {
    let setup = || -> Result<(cpal::Stream, StreamReady)> {
        let device = find_device(&cfg)?;
        let caps = probe_caps(&device);
        let (stream_config, sample_format) = choose_config(&device, &cfg)?;

        let sample_rate = stream_config.sample_rate.0;
        let channels = stream_config.channels;
        let ring = HeapRb::<f32>::new(sample_rate as usize * channels as usize * RING_BUFFER_SECS);
        let (producer, consumer) = ring.split();
        let dropped = Arc::new(AtomicU64::new(0));

        let stream = build_stream(
            &device,
            &stream_config,
            sample_format,
            producer,
            Arc::clone(&dropped),
        )?;
        stream
            .play()
            .map_err(|e| CaptureError::Backend(format!("failed to start audio stream: {e}")))?;

        tracing::info!(
            device = device.name().unwrap_or_else(|_| "<unknown>".into()),
            sample_rate,
            channels,
            ?sample_format,
            "microphone stream opened"
        );

        Ok((
            stream,
            StreamReady {
                caps,
                consumer,
                sample_rate,
                channels,
                dropped,
            },
        ))
    };

    match setup() {
        Ok((stream, ready_payload)) => {
            if ready.send(Ok(ready_payload)).is_err() {
                return;
            }
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// Resolves the configured device name, or the platform default input.
fn find_device(cfg: &AudioConfig) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match &cfg.device {
        Some(name) => host
            .input_devices()
            .map_err(|e| CaptureError::Backend(format!("could not enumerate inputs: {e}")))?
            .find(|d| d.name().is_ok_and(|n| &n == name))
            .ok_or_else(|| CaptureError::DeviceNotFound(name.clone())),
        None => host
            .default_input_device()
            .ok_or_else(|| CaptureError::DeviceNotFound("no default input device".into())),
    }
}

/// Maps the device's supported input ranges into HAL capability terms.
fn probe_caps(device: &cpal::Device) -> AudioCaps {
    let modes = device
        .supported_input_configs()
        .map(|configs| {
            configs
                .map(|range| AudioMode {
                    channels: range.channels(),
                    min_sample_rate: range.min_sample_rate().0,
                    max_sample_rate: range.max_sample_rate().0,
                })
                .collect()
        })
        .unwrap_or_default();
    AudioCaps { modes }
}

/// Picks a stream configuration honoring the request when possible.
///
/// Preference order: a supported range matching the requested channel count
/// and containing the requested rate (f32 sample format first), then the
/// device's default input configuration as-is.
fn choose_config(
    device: &cpal::Device,
    cfg: &AudioConfig,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat)> {
    let ranges: Vec<_> = device
        .supported_input_configs()
        .map(|c| c.collect())
        .unwrap_or_default();

    let matches_request = |range: &cpal::SupportedStreamConfigRange| {
        range.channels() == cfg.channels
            && range.min_sample_rate().0 <= cfg.sample_rate
            && cfg.sample_rate <= range.max_sample_rate().0
    };

    let chosen = ranges
        .iter()
        .filter(|r| matches_request(r))
        .max_by_key(|r| r.sample_format() == cpal::SampleFormat::F32);

    if let Some(range) = chosen {
        let config = (*range).with_sample_rate(cpal::SampleRate(cfg.sample_rate));
        return Ok((config.config(), config.sample_format()));
    }

    let default = device
        .default_input_config()
        .map_err(|e| CaptureError::Backend(format!("no usable input config: {e}")))?;
    tracing::warn!(
        requested_rate = cfg.sample_rate,
        requested_channels = cfg.channels,
        actual_rate = default.sample_rate().0,
        actual_channels = default.channels(),
        "requested audio config unsupported; using device default"
    );
    Ok((default.config(), default.sample_format()))
}

/// Builds the input stream for whichever sample format the device speaks,
/// converting to `f32` in the callback.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    producer: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream> {
    match sample_format {
        cpal::SampleFormat::F32 => build_stream_typed::<f32>(device, config, producer, dropped),
        cpal::SampleFormat::I16 => build_stream_typed::<i16>(device, config, producer, dropped),
        cpal::SampleFormat::U16 => build_stream_typed::<u16>(device, config, producer, dropped),
        cpal::SampleFormat::I32 => build_stream_typed::<i32>(device, config, producer, dropped),
        other => Err(CaptureError::UnsupportedConfig(format!(
            "unsupported sample format {other:?}"
        ))),
    }
}

/// The typed half of [`build_stream`]. The callback body is the only code
/// that runs on the real-time audio thread: convert each sample to `f32` and
/// push into the ring buffer, counting anything that doesn't fit.
fn build_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut lost: u64 = 0;
                for sample in data {
                    if producer.try_push(f32::from_sample(*sample)).is_err() {
                        lost += 1;
                    }
                }
                if lost > 0 {
                    dropped.fetch_add(lost, Ordering::Relaxed);
                }
            },
            |e| tracing::warn!(error = %e, "audio stream error"),
            None,
        )
        .map_err(|e| CaptureError::Backend(format!("failed to build input stream: {e}")))?;
    Ok(stream)
}

/// Async side of the ring buffer: slices buffered samples into fixed-size
/// chunks, stamps them arithmetically from the stream anchor, and publishes
/// them until the source is dropped.
async fn drain_loop(
    mut ready: StreamReady,
    chunk_frames: usize,
    tx: broadcast::Sender<AudioChunk>,
    stop: Arc<AtomicBool>,
) {
    let channels = ready.channels.max(1) as usize;
    let chunk_samples = chunk_frames * channels;
    let chunk_duration = Duration::from_secs_f64(chunk_frames as f64 / ready.sample_rate as f64);

    let mut ticker = tokio::time::interval(chunk_duration / 2);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut anchor: Option<Timestamp> = None;
    let mut frames_drained: u64 = 0;
    let mut reported_drops: u64 = 0;
    let mut buf = vec![0.0f32; chunk_samples];

    while !stop.load(Ordering::Relaxed) {
        ticker.tick().await;

        while ready.consumer.occupied_len() >= chunk_samples {
            let anchor = *anchor.get_or_insert_with(Timestamp::now);
            ready.consumer.pop_slice(&mut buf);

            let ts = anchor
                + Duration::from_nanos(
                    frames_drained * 1_000_000_000 / ready.sample_rate.max(1) as u64,
                );
            frames_drained += chunk_frames as u64;

            let _ = tx.send(AudioChunk {
                ts,
                sample_rate: ready.sample_rate,
                channels: ready.channels,
                samples: Arc::from(buf.as_slice()),
            });
        }

        let total_drops = ready.dropped.load(Ordering::Relaxed);
        if total_drops > reported_drops {
            tracing::warn!(
                dropped_samples = total_drops - reported_drops,
                "audio ring buffer overflowed; drain task falling behind"
            );
            reported_drops = total_drops;
        }
    }
}
