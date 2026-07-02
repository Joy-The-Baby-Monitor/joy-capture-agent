//! Real-camera [`VideoSource`] backend built on `nokhwa`.
//!
//! One backend, two platforms: nokhwa fronts AVFoundation on macOS (dev) and
//! V4L2 on Linux/Pi (prod), and surfaces native UVC formats — including MJPEG
//! — which is what keeps the UVC-passthrough encode path (design §5.7) open.
//! If nokhwa is ever outgrown on the Pi, the `v4l` crate slots in behind the
//! same trait under `#[cfg(target_os = "linux")]` without touching consumers.
//!
//! # Threading model
//!
//! Platform camera handles are not generally `Send`, and frame reads are
//! blocking, so the camera is created *and* polled on one dedicated OS thread.
//! `open` hands the thread its config, waits on a oneshot for the probe result
//! (capabilities or an error), and the thread then loops reading frames and
//! publishing them onto the broadcast channel until the source is dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use joy_core::Timestamp;
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use tokio::sync::{broadcast, oneshot};

use crate::error::{CaptureError, Result};
use crate::video::{
    PixelFormat, VIDEO_CHANNEL_CAPACITY, VideoCaps, VideoConfig, VideoFrame, VideoMode,
    VideoSource,
};

/// Consecutive read failures tolerated before the capture thread gives up
/// (e.g. the camera was unplugged).
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// A [`VideoSource`] backed by a physical camera via nokhwa
/// (AVFoundation / V4L2 / MSMF).
///
/// Frames are published in the camera's native wire format without decoding;
/// an MJPEG camera yields [`PixelFormat::Mjpeg`] frames ready for
/// passthrough. Dropping the source signals the capture thread to stop and
/// release the device.
pub struct CameraVideoSource {
    tx: broadcast::Sender<VideoFrame>,
    caps: VideoCaps,
    stop: Arc<AtomicBool>,
}

impl VideoSource for CameraVideoSource {
    async fn open(cfg: &VideoConfig) -> Result<Self> {
        let (tx, _) = broadcast::channel(VIDEO_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();

        let thread_cfg = cfg.clone();
        let thread_tx = tx.clone();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("joy-capture-camera".into())
            .spawn(move || capture_thread(thread_cfg, thread_tx, thread_stop, ready_tx))
            .map_err(|e| CaptureError::Backend(format!("failed to spawn capture thread: {e}")))?;

        let caps = ready_rx
            .await
            .map_err(|_| CaptureError::Backend("capture thread died during open".into()))??;

        Ok(Self { tx, caps, stop })
    }

    fn subscribe(&self) -> broadcast::Receiver<VideoFrame> {
        self.tx.subscribe()
    }

    fn capabilities(&self) -> VideoCaps {
        self.caps.clone()
    }
}

impl Drop for CameraVideoSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Body of the dedicated capture thread: opens the camera, reports the probe
/// result through `ready`, then reads frames until stopped or the device
/// fails persistently.
fn capture_thread(
    cfg: VideoConfig,
    tx: broadcast::Sender<VideoFrame>,
    stop: Arc<AtomicBool>,
    ready: oneshot::Sender<Result<VideoCaps>>,
) {
    let mut camera = match open_camera(&cfg) {
        Ok(camera) => camera,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let caps = probe_caps(&mut camera);

    if let Err(e) = camera.open_stream() {
        let _ = ready.send(Err(CaptureError::Backend(format!(
            "failed to start camera stream: {e}"
        ))));
        return;
    }

    let format = camera.camera_format();
    tracing::info!(
        width = format.resolution().width(),
        height = format.resolution().height(),
        fps = format.frame_rate(),
        format = ?format.format(),
        "camera stream opened"
    );

    if ready.send(Ok(caps)).is_err() {
        let _ = camera.stop_stream();
        return;
    }

    let mut consecutive_errors: u32 = 0;
    let mut warned_unmapped = false;
    while !stop.load(Ordering::Relaxed) {
        match camera.frame() {
            Ok(buffer) => {
                consecutive_errors = 0;
                let Some(pixel_format) = map_frame_format(buffer.source_frame_format()) else {
                    if !warned_unmapped {
                        warned_unmapped = true;
                        tracing::warn!(
                            format = ?buffer.source_frame_format(),
                            "dropping frames in unmapped pixel format"
                        );
                    }
                    continue;
                };
                let resolution = buffer.resolution();
                let _ = tx.send(VideoFrame {
                    ts: Timestamp::now(),
                    format: pixel_format,
                    width: resolution.width(),
                    height: resolution.height(),
                    data: buffer.buffer_bytes(),
                });
            }
            Err(e) => {
                consecutive_errors += 1;
                tracing::warn!(error = %e, consecutive_errors, "camera frame read failed");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    tracing::error!("camera persistently failing; stopping capture thread");
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    let _ = camera.stop_stream();
}

/// Opens the camera described by `cfg`, preferring compressed native formats.
///
/// Tries MJPEG closest-match first (enables the passthrough encode path),
/// then YUYV, then whatever the device offers at its highest frame rate.
fn open_camera(cfg: &VideoConfig) -> Result<Camera> {
    ensure_permission()?;

    let index = match &cfg.device {
        Some(selector) => selector
            .parse::<u32>()
            .map(CameraIndex::Index)
            .unwrap_or_else(|_| CameraIndex::String(selector.clone())),
        None => CameraIndex::Index(0),
    };

    let resolution = Resolution::new(cfg.width, cfg.height);
    let attempts = [
        RequestedFormatType::Closest(CameraFormat::new(resolution, FrameFormat::MJPEG, cfg.fps)),
        RequestedFormatType::Closest(CameraFormat::new(resolution, FrameFormat::YUYV, cfg.fps)),
        RequestedFormatType::AbsoluteHighestFrameRate,
    ];

    let mut last_error = None;
    for format_type in attempts {
        match Camera::new(index.clone(), RequestedFormat::new::<RgbFormat>(format_type)) {
            Ok(camera) => return Ok(camera),
            Err(e) => last_error = Some(e),
        }
    }

    Err(CaptureError::DeviceNotFound(format!(
        "could not open camera {index}: {}",
        last_error.map_or_else(|| "no formats accepted".into(), |e| e.to_string())
    )))
}

/// Requests camera authorization where the platform demands it.
///
/// On macOS, AVFoundation requires TCC camera permission; nokhwa exposes the
/// prompt via `nokhwa_initialize`. The grant callback is asynchronous, so
/// after triggering the prompt we poll briefly for the decision. On Linux
/// this is a no-op (V4L2 access is plain file permissions).
fn ensure_permission() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if !nokhwa::nokhwa_check() {
            nokhwa::nokhwa_initialize(|granted| {
                tracing::info!(granted, "macOS camera permission decision");
            });
            for _ in 0..100 {
                if nokhwa::nokhwa_check() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return Err(CaptureError::Backend(
                "macOS camera permission not granted".into(),
            ));
        }
    }
    Ok(())
}

/// Queries the device's supported formats, mapping them into HAL terms.
///
/// Falls back to the currently negotiated format when enumeration fails or
/// yields nothing mappable (some AVFoundation devices report formats the HAL
/// doesn't model yet) — a source that is producing frames must never report
/// zero capabilities.
fn probe_caps(camera: &mut Camera) -> VideoCaps {
    let mut modes: Vec<VideoMode> = match camera.compatible_camera_formats() {
        Ok(formats) => formats
            .into_iter()
            .filter_map(|f| {
                Some(VideoMode {
                    width: f.resolution().width(),
                    height: f.resolution().height(),
                    fps: f.frame_rate(),
                    format: map_frame_format(f.format())?,
                })
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not enumerate camera formats");
            Vec::new()
        }
    };

    if modes.is_empty() {
        let current = camera.camera_format();
        modes.extend(map_frame_format(current.format()).map(|format| VideoMode {
            width: current.resolution().width(),
            height: current.resolution().height(),
            fps: current.frame_rate(),
            format,
        }));
    }

    VideoCaps { modes }
}

/// Translates nokhwa's wire format into the HAL's [`PixelFormat`]. Returns
/// `None` for formats the pipeline doesn't understand yet.
fn map_frame_format(format: FrameFormat) -> Option<PixelFormat> {
    match format {
        FrameFormat::MJPEG => Some(PixelFormat::Mjpeg),
        FrameFormat::YUYV => Some(PixelFormat::Yuyv),
        FrameFormat::NV12 => Some(PixelFormat::Nv12),
        FrameFormat::GRAY => Some(PixelFormat::Luma8),
        FrameFormat::RAWRGB => Some(PixelFormat::Rgb24),
        _ => None,
    }
}
