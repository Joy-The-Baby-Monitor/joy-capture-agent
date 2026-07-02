//! The Joy Capture Agent daemon.
//!
//! `joy-agentd` is the binary that wires every crate into a running agent.
//! At roadmap step 1 it hosts the **capture probe**: it opens a video and an
//! audio source (real hardware by default, synthetic with `--simulate`),
//! subscribes to both fan-outs, and reports throughput and timestamp health —
//! the proof that the HAL and its time discipline work on this machine.
//!
//! ```text
//! joy-agentd [--simulate] [--duration <secs>]
//! ```
//!
//! Later milestones replace this probe with the supervised pipeline (encoder,
//! analyzers, WebRTC, control plane).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use joy_capture::{
    AudioChunk, AudioConfig, AudioSource, SimulatedAudioSource, SimulatedVideoSource, VideoConfig,
    VideoFrame, VideoSource,
};
use tokio::sync::broadcast;

/// Parsed command-line options for the capture probe.
struct Options {
    simulate: bool,
    duration: Duration,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        simulate: false,
        duration: Duration::from_secs(5),
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--simulate" => options.simulate = true,
            "--duration" => {
                let value = args.next().ok_or("--duration requires a value")?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --duration value: {value}"))?;
                options.duration = Duration::from_secs(secs);
            }
            "--help" | "-h" => {
                return Err("usage: joy-agentd [--simulate] [--duration <secs>]".into());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .init();

    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let result = if options.simulate {
        run_simulated_probe(options.duration).await
    } else {
        run_hardware_probe(options.duration).await
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("capture probe failed: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run_simulated_probe(duration: Duration) -> Result<(), String> {
    println!("opening simulated capture sources…");
    let video = SimulatedVideoSource::open(&VideoConfig::default())
        .await
        .map_err(|e| format!("video: {e}"))?;
    let audio = SimulatedAudioSource::open(&AudioConfig::default())
        .await
        .map_err(|e| format!("audio: {e}"))?;
    run_probe(&video, &audio, duration).await
}

async fn run_hardware_probe(duration: Duration) -> Result<(), String> {
    println!("opening hardware capture sources… (use --simulate to run without hardware)");
    let video = joy_capture::CameraVideoSource::open(&VideoConfig::default())
        .await
        .map_err(|e| format!("video: {e}"))?;
    let audio = joy_capture::MicrophoneAudioSource::open(&AudioConfig::default())
        .await
        .map_err(|e| format!("audio: {e}"))?;
    run_probe(&video, &audio, duration).await
}

/// Watches both fan-outs for `duration`, printing periodic stats and a final
/// verdict. Generic over the HAL traits — the probe cannot tell a webcam from
/// the simulator, which is exactly the point of the seam.
async fn run_probe<V: VideoSource, A: AudioSource>(
    video: &V,
    audio: &A,
    duration: Duration,
) -> Result<(), String> {
    let video_caps = video.capabilities();
    println!("video modes: {}", summarize_video_caps(&video_caps));
    println!("audio modes: {}", audio.capabilities().modes.len());
    println!("probing for {}s…", duration.as_secs());

    let (video_stats, audio_stats) = tokio::join!(
        watch_video(video.subscribe(), duration),
        watch_audio(audio.subscribe(), duration),
    );

    println!("{video_stats}");
    println!("{audio_stats}");

    if video_stats.count == 0 {
        return Err("no video frames received".into());
    }
    if audio_stats.count == 0 {
        return Err("no audio chunks received".into());
    }
    if !video_stats.monotonic || !audio_stats.monotonic {
        return Err("timestamps regressed — time discipline violated".into());
    }
    println!("capture probe OK");
    Ok(())
}

fn summarize_video_caps(caps: &joy_capture::VideoCaps) -> String {
    let mut summary = caps
        .modes
        .iter()
        .take(4)
        .map(|m| format!("{}x{}@{} {:?}", m.width, m.height, m.fps, m.format))
        .collect::<Vec<_>>()
        .join(", ");
    if caps.modes.len() > 4 {
        summary.push_str(&format!(" … ({} total)", caps.modes.len()));
    }
    summary
}

/// Accumulated health counters for one media stream.
struct StreamStats {
    label: &'static str,
    count: u64,
    lagged: u64,
    monotonic: bool,
    detail: String,
}

impl std::fmt::Display for StreamStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} received, {} dropped to lag, timestamps {}{}",
            self.label,
            self.count,
            self.lagged,
            if self.monotonic { "monotonic" } else { "REGRESSED" },
            self.detail,
        )
    }
}

async fn watch_video(mut rx: broadcast::Receiver<VideoFrame>, duration: Duration) -> StreamStats {
    let deadline = Instant::now() + duration;
    let mut stats = StreamStats {
        label: "video",
        count: 0,
        lagged: 0,
        monotonic: true,
        detail: String::new(),
    };
    let mut last_ts = None;
    let mut first_ts = None;
    let mut last_frame: Option<VideoFrame> = None;

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => break,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => stats.lagged += n,
            Ok(Ok(frame)) => {
                stats.count += 1;
                if let Some(prev) = last_ts
                    && frame.ts <= prev
                {
                    stats.monotonic = false;
                }
                last_ts = Some(frame.ts);
                first_ts.get_or_insert(frame.ts);
                last_frame = Some(frame);
            }
        }
    }

    if let (Some(frame), Some(first), Some(last)) = (&last_frame, first_ts, last_ts) {
        let span = (last - first).as_secs_f64();
        let fps = if span > 0.0 {
            (stats.count.saturating_sub(1)) as f64 / span
        } else {
            0.0
        };
        stats.detail = format!(
            "; {}x{} {:?}, {:.1} fps measured",
            frame.width, frame.height, frame.format, fps
        );
    }
    stats
}

async fn watch_audio(mut rx: broadcast::Receiver<AudioChunk>, duration: Duration) -> StreamStats {
    let deadline = Instant::now() + duration;
    let mut stats = StreamStats {
        label: "audio",
        count: 0,
        lagged: 0,
        monotonic: true,
        detail: String::new(),
    };
    let mut last_ts = None;
    let mut frames_total: u64 = 0;
    let mut peak_rms: f32 = 0.0;
    let mut last_chunk: Option<AudioChunk> = None;

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => break,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => stats.lagged += n,
            Ok(Ok(chunk)) => {
                stats.count += 1;
                frames_total += chunk.frames() as u64;
                peak_rms = peak_rms.max(chunk.rms());
                if let Some(prev) = last_ts
                    && chunk.ts <= prev
                {
                    stats.monotonic = false;
                }
                last_ts = Some(chunk.ts);
                last_chunk = Some(chunk);
            }
        }
    }

    if let Some(chunk) = &last_chunk {
        stats.detail = format!(
            "; {} Hz × {} ch, {} frames total, peak RMS {:.3}",
            chunk.sample_rate, chunk.channels, frames_total, peak_rms
        );
    }
    stats
}
