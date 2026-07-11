//! Media encoding: long-running tasks that drain a capture fan-out, encode,
//! and write samples onto a shared WebRTC track.
//!
//! One encoder task runs per media kind regardless of how many clients are
//! connected — software encode is the expensive part of the pipeline, and
//! `TrackLocalStaticSample` fans a single written sample out to every bound
//! peer, so per-client work stays at the (cheap) packetization layer.
//!
//! Both tasks follow the pipeline's real-time rule: on lag they skip forward
//! (drop-oldest) rather than build a backlog, and a closed capture channel
//! ends the task cleanly.

pub mod audio;
pub mod video;

use tokio::sync::mpsc;

pub use audio::run_audio_encoder;
pub use video::run_video_encoder;

/// Settings for the shared H.264 video encoder.
#[derive(Debug, Clone, Copy)]
pub struct VideoEncodeConfig {
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Upper bound on encoded frame rate, used by the encoder's rate control.
    /// The actual rate follows the capture source.
    pub max_fps: f32,
    /// Frames between forced IDR frames (the GOP ceiling). Late-joining
    /// clients can only start decoding at an IDR, so this bounds their worst
    /// case wait; connection-time keyframe requests bound the typical case.
    pub idr_interval_frames: u32,
}

impl Default for VideoEncodeConfig {
    /// 1.5 Mb/s at up to 15 fps with an IDR at least every 30 frames —
    /// matched to the capture layer's 720p@15 default and comfortable for
    /// software encode on a Pi 5.
    fn default() -> Self {
        Self {
            bitrate_bps: 1_500_000,
            max_fps: 15.0,
            idr_interval_frames: 30,
        }
    }
}

/// Handle for requesting an immediate keyframe from the running video
/// encoder task.
///
/// Cheap to clone; the peer manager fires one request per newly connected
/// client so the fresh viewer gets a decodable stream right away instead of
/// waiting out the GOP. Requests are collapsed — many requests before the
/// next frame produce one IDR.
#[derive(Debug, Clone)]
pub struct KeyframeRequester {
    tx: mpsc::Sender<()>,
}

impl KeyframeRequester {
    /// Creates the requester and the receiving half for the encoder task.
    pub fn new() -> (Self, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel(4);
        (Self { tx }, rx)
    }

    /// Asks the encoder to emit an IDR with the next frame. Never blocks;
    /// if the request queue is full an IDR is already on the way.
    pub fn request(&self) {
        let _ = self.tx.try_send(());
    }
}
