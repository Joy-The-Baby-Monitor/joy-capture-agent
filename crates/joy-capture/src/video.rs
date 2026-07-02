//! The video half of the capture HAL: frame/config/capability types and the
//! [`VideoSource`] trait that every video backend implements.

use bytes::Bytes;
use joy_core::Timestamp;
use tokio::sync::broadcast;

use crate::error::Result;

/// Capacity of the bounded broadcast channel a [`VideoSource`] publishes on.
///
/// Small on purpose: for live monitoring a consumer always wants the *latest*
/// frame, never a backlog. A subscriber that falls more than this many frames
/// behind receives [`broadcast::error::RecvError::Lagged`] and skips forward —
/// the pipeline's drop-oldest rule.
pub const VIDEO_CHANNEL_CAPACITY: usize = 8;

/// The pixel/byte layout of a [`VideoFrame`]'s payload.
///
/// Frames are carried in whatever format the source natively produces — a UVC
/// camera emitting MJPEG stays MJPEG all the way to the encoder, which is what
/// makes the passthrough encode path (design §5.7) possible. Consumers that
/// need raw pixels check [`PixelFormat::is_compressed`] and decode themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Packed YUV 4:2:2 (2 bytes per pixel) — the most common raw UVC format.
    Yuyv,
    /// Motion-JPEG: each frame is an independent JPEG image.
    Mjpeg,
    /// Planar/semi-planar YUV 4:2:0 — Y plane followed by interleaved UV.
    Nv12,
    /// Packed 8-bit RGB (3 bytes per pixel). Produced by the simulate source.
    Rgb24,
    /// Single-channel 8-bit grayscale.
    Luma8,
}

impl PixelFormat {
    /// Whether the payload is a compressed bitstream rather than raw pixels.
    pub fn is_compressed(&self) -> bool {
        matches!(self, PixelFormat::Mjpeg)
    }
}

/// One captured video frame.
///
/// The payload is a [`Bytes`] handle, so cloning a frame (which the broadcast
/// fan-out does once per subscriber) is a cheap reference-count bump, never a
/// pixel copy.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Monotonic capture time, stamped at the source (design "time
    /// discipline": A/V sync and event correlation derive from this).
    pub ts: Timestamp,
    /// Layout of `data`.
    pub format: PixelFormat,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Raw or compressed payload, per `format`.
    pub data: Bytes,
}

/// Requested video capture configuration.
///
/// Sources treat this as a *request*: hardware backends open the closest mode
/// the device supports and report what they actually got via frame metadata
/// and [`VideoSource::capabilities`]. The simulate source honors it exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoConfig {
    /// Backend-specific device selector — an index (`"0"`) or a device name.
    /// `None` selects the platform default camera.
    pub device: Option<String>,
    /// Requested frame width in pixels.
    pub width: u32,
    /// Requested frame height in pixels.
    pub height: u32,
    /// Requested frame rate in frames per second.
    pub fps: u32,
}

impl Default for VideoConfig {
    /// 720p at 15 fps — the design's realistic software-encode budget for the
    /// Pi 5, and a sane default everywhere else.
    fn default() -> Self {
        Self {
            device: None,
            width: 1280,
            height: 720,
            fps: 15,
        }
    }
}

/// One capture mode a video source can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: PixelFormat,
}

/// What a video source can do: the modes it can capture in.
///
/// Probed at open time. Downstream, `joy-media` uses this to pick the encode
/// path (a native-MJPEG mode enables UVC passthrough), and the control plane
/// surfaces it during capability negotiation.
#[derive(Debug, Clone, Default)]
pub struct VideoCaps {
    pub modes: Vec<VideoMode>,
}

/// A source of live video frames — the video half of the capture HAL.
///
/// Implementations own their capture machinery (a device stream, a background
/// thread or task) and publish frames onto a bounded broadcast channel of
/// capacity [`VIDEO_CHANNEL_CAPACITY`]. Capture starts when `open` returns and
/// stops when the source is dropped.
///
/// # Contract
///
/// - Every frame carries a monotonic [`Timestamp`] stamped at capture.
/// - The channel is drop-oldest: a slow subscriber sees `Lagged` and skips
///   forward; it can never stall the producer or other subscribers.
/// - `subscribe` may be called any number of times; each receiver observes
///   frames published *after* it subscribed (live semantics, no replay).
/// - Frames are published in the source's native format — no implicit decode.
pub trait VideoSource: Send {
    /// Opens the device (or synthesizer) described by `cfg` and starts
    /// capturing in the background.
    fn open(cfg: &VideoConfig) -> impl Future<Output = Result<Self>> + Send
    where
        Self: Sized;

    /// Returns a new receiver on the source's frame fan-out.
    fn subscribe(&self) -> broadcast::Receiver<VideoFrame>;

    /// Returns the capture modes this source supports, probed at open time.
    fn capabilities(&self) -> VideoCaps;
}
