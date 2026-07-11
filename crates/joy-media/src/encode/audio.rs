//! The shared Opus audio encoder task.

use joy_capture::AudioChunk;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::error::{MediaError, Result};

/// Maximum size of one encoded Opus packet. libopus recommends 4000 bytes as
/// a safe ceiling for one frame at any bitrate it will actually choose.
const MAX_OPUS_PACKET: usize = 4000;

/// Chunk cadence the pipeline is built around: 20 ms, Opus's native frame
/// duration and the capture layer's default `chunk_frames`.
const CHUNK_DURATION: Duration = Duration::from_millis(20);

/// Drains an audio fan-out, encodes to Opus, and writes samples onto the
/// shared track until the capture source closes.
///
/// The encoder is created lazily from the first chunk's *actual* sample rate
/// and channel count — the capture backend may not have honored the requested
/// config — and validated against what Opus and this pipeline support:
///
/// - sample rate ∈ {8000, 16000, 24000, 48000} Hz (resampling is out of
///   scope; a 44.1 kHz-only microphone is unsupported for now),
/// - 1 or 2 channels,
/// - exactly 20 ms per chunk (`sample_rate / 50` frames), which the capture
///   layer guarantees by construction.
///
/// Violations fail the task loudly rather than degrade silently: a baby
/// monitor with quietly missing audio is worse than one that refuses to
/// start. If a mid-stream chunk changes rate or channel count (a source
/// restart), the encoder is rebuilt; Opus packets self-describe their
/// layout, so the negotiated `opus/48000/2` payload stays valid throughout.
///
/// Returns `Ok(())` when the capture channel closes.
pub async fn run_audio_encoder(
    mut rx: broadcast::Receiver<AudioChunk>,
    track: Arc<TrackLocalStaticSample>,
) -> Result<()> {
    let mut encoder: Option<(opus::Encoder, u32, u16)> = None;
    let mut packet = vec![0u8; MAX_OPUS_PACKET];

    loop {
        let chunk = match rx.recv().await {
            Ok(chunk) => chunk,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // A skipped chunk is a 20 ms gap the client's jitter buffer
                // conceals; never worth blocking the producer over.
                tracing::debug!("audio encoder lagged, {n} chunks dropped");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };

        let rebuild = match &encoder {
            Some((_, rate, channels)) => *rate != chunk.sample_rate || *channels != chunk.channels,
            None => true,
        };
        if rebuild {
            encoder = Some((build_encoder(&chunk)?, chunk.sample_rate, chunk.channels));
        }
        let (opus_encoder, ..) = encoder.as_mut().expect("encoder was just built");

        let expected_frames = (chunk.sample_rate / 50) as usize;
        if chunk.frames() != expected_frames {
            return Err(MediaError::Encode(format!(
                "audio chunk is {} frames, pipeline requires 20 ms chunks ({expected_frames} frames at {} Hz)",
                chunk.frames(),
                chunk.sample_rate
            )));
        }

        let len = opus_encoder
            .encode_float(&chunk.samples, &mut packet)
            .map_err(|e| MediaError::Encode(format!("opus encode: {e}")))?;

        track
            .write_sample(&Sample {
                data: packet[..len].to_vec().into(),
                duration: CHUNK_DURATION,
                ..Default::default()
            })
            .await
            .map_err(|e| MediaError::Encode(format!("track write: {e}")))?;
    }
}

/// Validates a chunk's parameters and builds an Opus encoder for them.
fn build_encoder(chunk: &AudioChunk) -> Result<opus::Encoder> {
    if !matches!(chunk.sample_rate, 8_000 | 16_000 | 24_000 | 48_000) {
        return Err(MediaError::Encode(format!(
            "unsupported audio sample rate {} Hz (Opus needs 8/16/24/48 kHz; resampling is not implemented)",
            chunk.sample_rate
        )));
    }
    let channels = match chunk.channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        n => {
            return Err(MediaError::Encode(format!(
                "unsupported audio channel count {n} (need mono or stereo)"
            )));
        }
    };
    opus::Encoder::new(chunk.sample_rate, channels, opus::Application::Audio)
        .map_err(|e| MediaError::Encode(format!("opus encoder construction: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use joy_core::Timestamp;
    use std::sync::Arc;

    fn chunk(sample_rate: u32, channels: u16, frames: usize) -> AudioChunk {
        AudioChunk {
            ts: Timestamp::now(),
            sample_rate,
            channels,
            samples: Arc::from(vec![0f32; frames * channels as usize]),
        }
    }

    #[test]
    fn valid_chunk_builds_encoder() {
        assert!(build_encoder(&chunk(48_000, 1, 960)).is_ok());
        assert!(build_encoder(&chunk(16_000, 2, 320)).is_ok());
    }

    #[test]
    fn cd_sample_rate_is_rejected() {
        let err = build_encoder(&chunk(44_100, 1, 882)).unwrap_err();
        assert!(matches!(err, MediaError::Encode(_)), "{err:?}");
    }

    #[test]
    fn surround_channels_are_rejected() {
        let err = build_encoder(&chunk(48_000, 6, 960)).unwrap_err();
        assert!(matches!(err, MediaError::Encode(_)), "{err:?}");
    }

    #[tokio::test]
    async fn wrong_chunk_size_fails_the_task() {
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        let track = Arc::new(TrackLocalStaticSample::new(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "joy".to_owned(),
        ));

        // 10 ms instead of 20 ms.
        tx.send(chunk(48_000, 1, 480)).expect("send");
        let result = run_audio_encoder(rx, track).await;
        assert!(matches!(result, Err(MediaError::Encode(_))), "{result:?}");
    }

    #[tokio::test]
    async fn encodes_20ms_chunks_until_source_closes() {
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        let track = Arc::new(TrackLocalStaticSample::new(
            webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "joy".to_owned(),
        ));

        for _ in 0..3 {
            tx.send(chunk(48_000, 1, 960)).expect("send");
        }
        drop(tx); // close the source; the task should end Ok

        let result = run_audio_encoder(rx, track).await;
        assert!(result.is_ok(), "{result:?}");
    }
}
