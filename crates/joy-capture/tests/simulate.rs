//! HAL contract tests, exercised through the simulate backends.
//!
//! These run on any machine with no hardware attached, so they double as the
//! CI proof that the trait semantics (timestamps, drop-oldest fan-out, caps)
//! hold.

use std::time::Duration;

use joy_capture::{
    AudioConfig, AudioSource, PixelFormat, SimulatedAudioSource, SimulatedVideoSource, VideoConfig,
    VideoSource,
};

/// A small, fast config so tests finish quickly.
fn test_video_config() -> VideoConfig {
    VideoConfig {
        device: None,
        width: 64,
        height: 48,
        fps: 60,
    }
}

fn test_audio_config() -> AudioConfig {
    AudioConfig {
        device: None,
        sample_rate: 8_000,
        channels: 2,
        chunk_frames: 160,
    }
}

#[tokio::test]
async fn video_frames_have_correct_shape_and_monotonic_timestamps() {
    let cfg = test_video_config();
    let source = SimulatedVideoSource::open(&cfg).await.unwrap();
    let mut rx = source.subscribe();

    let mut last_ts = None;
    for _ in 0..5 {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("frame arrived in time")
            .expect("channel open");

        assert_eq!(frame.width, cfg.width);
        assert_eq!(frame.height, cfg.height);
        assert_eq!(frame.format, PixelFormat::Rgb24);
        assert_eq!(frame.data.len(), (cfg.width * cfg.height * 3) as usize);

        if let Some(prev) = last_ts {
            assert!(frame.ts > prev, "timestamps must increase strictly");
        }
        last_ts = Some(frame.ts);
    }
}

#[tokio::test]
async fn consecutive_video_frames_differ() {
    let source = SimulatedVideoSource::open(&test_video_config())
        .await
        .unwrap();
    let mut rx = source.subscribe();

    let a = rx.recv().await.unwrap();
    let b = rx.recv().await.unwrap();
    assert_ne!(a.data, b.data, "the pattern must move between frames");
}

#[tokio::test]
async fn video_caps_reflect_config() {
    let cfg = test_video_config();
    let source = SimulatedVideoSource::open(&cfg).await.unwrap();
    let caps = source.capabilities();

    assert_eq!(caps.modes.len(), 1);
    assert_eq!(caps.modes[0].width, cfg.width);
    assert_eq!(caps.modes[0].height, cfg.height);
    assert_eq!(caps.modes[0].fps, cfg.fps);
}

#[tokio::test]
async fn video_rejects_zero_config() {
    let cfg = VideoConfig {
        width: 0,
        ..test_video_config()
    };
    assert!(SimulatedVideoSource::open(&cfg).await.is_err());
}

#[tokio::test]
async fn late_subscriber_still_receives_frames() {
    let source = SimulatedVideoSource::open(&test_video_config())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut rx = source.subscribe();
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("late subscriber still gets live frames")
        .unwrap();
    assert!(frame.ts.as_nanos() > 0);
}

#[tokio::test]
async fn audio_chunks_have_exact_shape_and_derived_timestamps() {
    let cfg = test_audio_config();
    let source = SimulatedAudioSource::open(&cfg).await.unwrap();
    let mut rx = source.subscribe();

    let expected_delta =
        Duration::from_nanos(cfg.chunk_frames as u64 * 1_000_000_000 / cfg.sample_rate as u64);

    let mut last_ts = None;
    for _ in 0..5 {
        let chunk = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("chunk arrived in time")
            .expect("channel open");

        assert_eq!(chunk.sample_rate, cfg.sample_rate);
        assert_eq!(chunk.channels, cfg.channels);
        assert_eq!(chunk.frames(), cfg.chunk_frames);
        assert_eq!(
            chunk.samples.len(),
            cfg.chunk_frames * cfg.channels as usize
        );

        if let Some(prev) = last_ts {
            assert_eq!(
                chunk.ts - prev,
                expected_delta,
                "derived timestamps advance by exactly one chunk duration"
            );
        }
        last_ts = Some(chunk.ts);
    }
}

#[tokio::test]
async fn audio_tone_is_audible() {
    let source = SimulatedAudioSource::open(&test_audio_config())
        .await
        .unwrap();
    let mut rx = source.subscribe();

    let chunk = rx.recv().await.unwrap();
    assert!(chunk.rms() > 0.01, "the tone's first pulse must be audible");
    assert!(chunk.samples.iter().all(|s| s.abs() <= 1.0));
}

#[tokio::test]
async fn audio_rejects_zero_config() {
    let cfg = AudioConfig {
        sample_rate: 0,
        ..test_audio_config()
    };
    assert!(SimulatedAudioSource::open(&cfg).await.is_err());
}
