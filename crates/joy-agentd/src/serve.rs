//! Serve mode: the live-view pipeline.
//!
//! Wires capture sources into the shared encoder tasks, mounts the signaling
//! WebSocket (and, behind the `dev-ui` feature, the browser test page) on one
//! listener, and runs until ctrl-c or a fatal pipeline failure.
//!
//! Failure policy for this milestone: a dying *client session* is that
//! client's problem (handled inside `StreamHub`), but a dying *encoder* or a
//! closed *capture source* takes serve mode down with a nonzero exit — the
//! supervisor with restart/backoff arrives in a later milestone, and until
//! then a loudly dead monitor beats a silently degraded one.

use std::net::SocketAddr;
use std::sync::Arc;

use joy_capture::{AudioConfig, AudioSource, VideoConfig, VideoSource};
use joy_media::encode::KeyframeRequester;
use joy_media::peer::StreamHub;
use joy_media::signaling::ws::LanSignaling;
use joy_media::{VideoEncodeConfig, run_audio_encoder, run_video_encoder};

/// Options for serve mode, parsed from the CLI.
pub struct ServeOptions {
    /// Use synthetic capture sources instead of real hardware.
    pub simulate: bool,
    /// Address the HTTP/WebSocket listener binds to.
    pub listen: SocketAddr,
}

/// Opens capture sources per `options` and serves live A/V until shutdown.
pub async fn run(options: ServeOptions) -> Result<(), String> {
    if options.simulate {
        println!("opening simulated capture sources…");
        let video = joy_capture::SimulatedVideoSource::open(&VideoConfig::default())
            .await
            .map_err(|e| format!("video: {e}"))?;
        let audio = joy_capture::SimulatedAudioSource::open(&AudioConfig::default())
            .await
            .map_err(|e| format!("audio: {e}"))?;
        serve(video, audio, options.listen).await
    } else {
        println!("opening hardware capture sources… (use --simulate to run without hardware)");
        let video = joy_capture::CameraVideoSource::open(&VideoConfig::default())
            .await
            .map_err(|e| format!("video: {e}"))?;
        let audio = joy_capture::MicrophoneAudioSource::open(&AudioConfig::default())
            .await
            .map_err(|e| format!("audio: {e}"))?;
        serve(video, audio, options.listen).await
    }
}

/// The pipeline proper, generic over the HAL traits: encoder tasks feeding
/// the hub's shared tracks, the signaling accept loop, and the HTTP listener.
///
/// The sources are owned by this future — dropping them on return is what
/// stops the capture threads.
async fn serve<V, A>(video: V, audio: A, listen: SocketAddr) -> Result<(), String>
where
    V: VideoSource,
    A: AudioSource,
{
    let (keyframes, keyframe_rx) = KeyframeRequester::new();
    let hub = Arc::new(StreamHub::new(keyframes).map_err(|e| format!("stream hub: {e}"))?);

    let mut video_task = tokio::spawn(run_video_encoder(
        video.subscribe(),
        hub.video_track(),
        VideoEncodeConfig::default(),
        keyframe_rx,
    ));
    let mut audio_task = tokio::spawn(run_audio_encoder(audio.subscribe(), hub.audio_track()));

    let (signaling, router) = LanSignaling::new();
    let hub_task = tokio::spawn(Arc::clone(&hub).run(signaling));

    #[cfg(feature = "dev-ui")]
    let router = router.route(
        "/",
        axum::routing::get(|| async { axum::response::Html(include_str!("../ui/dev.html")) }),
    );

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind {listen}: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("local addr: {e}"))?;
    println!("serving on {bound} — signaling at ws://{bound}/signal");
    #[cfg(feature = "dev-ui")]
    println!("dev UI: http://{bound}/ (use the machine's LAN address from other devices)");

    let server = axum::serve(listener, router).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("\nshutting down…");
    });

    // Serve until ctrl-c, treating encoder exit as fatal: in a healthy serve
    // session the capture fan-outs never close and the encoders never error.
    let result = tokio::select! {
        served = server => served.map_err(|e| format!("http server: {e}")),
        encode = &mut video_task => Err(describe_encoder_exit("video", encode)),
        encode = &mut audio_task => Err(describe_encoder_exit("audio", encode)),
    };

    video_task.abort();
    audio_task.abort();
    hub_task.abort();
    result
}

/// Renders an encoder task's early exit as a fatal serve-mode error.
fn describe_encoder_exit(
    kind: &str,
    exit: Result<joy_media::Result<()>, tokio::task::JoinError>,
) -> String {
    match exit {
        Ok(Ok(())) => format!("{kind} capture source closed unexpectedly"),
        Ok(Err(e)) => format!("{kind} encoder failed: {e}"),
        Err(e) => format!("{kind} encoder task panicked: {e}"),
    }
}
