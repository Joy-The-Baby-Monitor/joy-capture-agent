//! The shared H.264 video encoder task.

use joy_capture::VideoFrame;
use joy_core::Timestamp;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType, VuiConfig,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use super::VideoEncodeConfig;
use crate::convert::{I420Buffer, to_i420};
use crate::error::{MediaError, Result};

/// Bounds applied to per-sample durations derived from capture timestamps.
///
/// A duration below the floor (timestamp jitter, duplicated stamps) or above
/// the ceiling (source stall, clock hiccup) would distort the RTP clock far
/// worse than a clamped approximation does.
const MIN_SAMPLE_DURATION: Duration = Duration::from_millis(1);
const MAX_SAMPLE_DURATION: Duration = Duration::from_secs(1);

/// Drains a video fan-out, encodes to H.264, and writes samples onto the
/// shared track until the capture source closes.
///
/// The openh264 encoder is created once and re-initializes itself internally
/// whenever the incoming resolution changes; a change in pixel *format* alone
/// is absorbed by the converter. Frames arriving while the task is behind are
/// dropped by the broadcast channel (drop-oldest), which is the desired
/// live-monitoring behavior: latest frame always, backlog never.
///
/// Keyframe requests arriving via `keyframe_rx` force an IDR with the next
/// encoded frame, on top of the periodic IDR interval from `cfg`. Sample
/// durations are derived from capture-timestamp deltas, so variable frame
/// rates and dropped frames keep the RTP clock honest.
///
/// Returns `Ok(())` when the capture channel closes; any encoder failure is
/// fatal and returned as [`MediaError::Encode`].
pub async fn run_video_encoder(
    mut rx: broadcast::Receiver<VideoFrame>,
    track: Arc<TrackLocalStaticSample>,
    cfg: VideoEncodeConfig,
    mut keyframe_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let encoder_config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(cfg.bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(cfg.max_fps))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::CameraVideoRealTime)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.idr_interval_frames))
        // The converter produces limited-range BT.601 on every input path.
        .vui(VuiConfig::bt601());
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config)
        .map_err(|e| MediaError::Encode(format!("openh264 encoder construction: {e}")))?;

    let mut scratch = I420Buffer::new();
    let mut prev_written_ts: Option<Timestamp> = None;
    let fallback_duration = Duration::from_secs_f32(1.0 / cfg.max_fps.max(1.0));

    loop {
        let frame = match rx.recv().await {
            Ok(frame) => frame,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!("video encoder lagged, {n} frames dropped");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };

        // Collapse any queued keyframe requests into one forced IDR.
        let mut want_idr = false;
        while keyframe_rx.try_recv().is_ok() {
            want_idr = true;
        }
        if want_idr {
            encoder.force_intra_frame();
        }

        if let Err(e) = to_i420(&frame, &mut scratch) {
            // A single bad frame (truncated MJPEG, glitched readout) is not
            // worth killing the stream over; log and wait for the next one.
            tracing::warn!("dropping unconvertible frame: {e}");
            continue;
        }

        // Scoped so the bitstream (which borrows the encoder's internal,
        // non-Sync buffers) is dropped before the await below — required for
        // this future to be Send.
        let data = {
            let bitstream = encoder
                .encode(&scratch)
                .map_err(|e| MediaError::Encode(format!("h264 encode: {e}")))?;
            if bitstream.frame_type() == FrameType::Skip {
                // Rate control chose to emit nothing; the next written
                // sample's timestamp delta absorbs the gap.
                continue;
            }
            bitstream.to_vec()
        };
        if data.is_empty() {
            continue;
        }

        let duration = match prev_written_ts {
            Some(prev) => frame
                .ts
                .saturating_since(prev)
                .clamp(MIN_SAMPLE_DURATION, MAX_SAMPLE_DURATION),
            None => fallback_duration,
        };
        prev_written_ts = Some(frame.ts);

        track
            .write_sample(&Sample {
                data: data.into(),
                duration,
                ..Default::default()
            })
            .await
            .map_err(|e| MediaError::Encode(format!("track write: {e}")))?;
    }
}
