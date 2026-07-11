//! WebRTC peer management: one shared pair of media tracks, one peer
//! connection per connected client.
//!
//! [`StreamHub`] owns the webrtc API factory (with a media engine registered
//! for exactly the codecs the encoders produce — H.264 and Opus, nothing
//! else), the two shared [`TrackLocalStaticSample`]s the encoder tasks write
//! into, and the accept loop that turns each [`SignalingSession`] into a
//! live media session. Because every peer binds the same two tracks, a
//! sample is encoded once and packetized per client — fan-out costs almost
//! nothing.

use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::api::{API, APIBuilder};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::encode::KeyframeRequester;
use crate::error::{MediaError, Result};
use crate::signaling::{PROTOCOL_VERSION, SignalMessage, Signaling, SignalingSession};

/// Payload types offered in the agent's SDP. Any dynamic PT ≥ 96 works; these
/// match the values browsers conventionally assign.
const H264_PAYLOAD_TYPE: u8 = 102;
const OPUS_PAYLOAD_TYPE: u8 = 111;

/// The `stream_id` shared by both tracks so clients group them into a single
/// `MediaStream` — that grouping is what lets the browser lip-sync audio to
/// video.
const STREAM_ID: &str = "joy";

/// Owns the shared tracks and accepts signaling sessions into peer
/// connections.
pub struct StreamHub {
    api: API,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    keyframes: KeyframeRequester,
}

impl StreamHub {
    /// Builds the hub: a media engine restricted to H.264 + Opus, default
    /// interceptors (NACK, RTCP reports, TWCC), and the two shared tracks
    /// the encoder tasks will feed.
    pub fn new(keyframes: KeyframeRequester) -> Result<Self> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: h264_capability(),
                    payload_type: H264_PAYLOAD_TYPE,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .map_err(|e| MediaError::Peer(format!("register h264: {e}")))?;
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: opus_capability(),
                    payload_type: OPUS_PAYLOAD_TYPE,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .map_err(|e| MediaError::Peer(format!("register opus: {e}")))?;

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| MediaError::Peer(format!("register interceptors: {e}")))?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let video_track = Arc::new(TrackLocalStaticSample::new(
            h264_capability(),
            "video".to_owned(),
            STREAM_ID.to_owned(),
        ));
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            opus_capability(),
            "audio".to_owned(),
            STREAM_ID.to_owned(),
        ));

        Ok(Self {
            api,
            video_track,
            audio_track,
            keyframes,
        })
    }

    /// The track the video encoder task writes into.
    pub fn video_track(&self) -> Arc<TrackLocalStaticSample> {
        Arc::clone(&self.video_track)
    }

    /// The track the audio encoder task writes into.
    pub fn audio_track(&self) -> Arc<TrackLocalStaticSample> {
        Arc::clone(&self.audio_track)
    }

    /// Accepts signaling sessions until the transport shuts down, spawning
    /// an independent media session per client.
    ///
    /// A client session failing (protocol violation, ICE failure, abrupt
    /// disconnect) is logged and affects only that client; the accept loop
    /// itself only ends when [`Signaling::accept`] returns `None`.
    pub async fn run<S: Signaling>(self: Arc<Self>, mut signaling: S) {
        while let Some(session) = signaling.accept().await {
            let hub = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = hub.handle_session(session).await {
                    tracing::warn!("client session ended with error: {e}");
                }
            });
        }
        tracing::info!("signaling transport closed; no longer accepting clients");
    }

    /// Runs one client's media session from offer to teardown.
    async fn handle_session(&self, mut session: SignalingSession) -> Result<()> {
        let pc = self
            .api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .map_err(|e| MediaError::Peer(format!("peer connection: {e}")))?;
        let pc = Arc::new(pc);

        // Bind the shared tracks and drain each sender's RTCP stream so
        // reports from the client never back up the transport.
        for track in [
            Arc::clone(&self.video_track) as Arc<dyn TrackLocal + Send + Sync>,
            Arc::clone(&self.audio_track) as Arc<dyn TrackLocal + Send + Sync>,
        ] {
            let sender = pc
                .add_track(track)
                .await
                .map_err(|e| MediaError::Peer(format!("add track: {e}")))?;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1500];
                while let Ok((_, _)) = sender.read(&mut buf).await {}
            });
        }

        // Surface connection-state changes into the select loop below.
        let (state_tx, mut state_rx) = mpsc::channel(8);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let state_tx = state_tx.clone();
            Box::pin(async move {
                let _ = state_tx.send(state).await;
            })
        }));

        let result = self.session_loop(&pc, &mut session, &mut state_rx).await;

        if let Err(e) = pc.close().await {
            tracing::debug!("peer connection close: {e}");
        }
        result
    }

    /// The per-session message loop: answers the offer, applies trickled
    /// candidates, and watches connection state until the session ends.
    async fn session_loop(
        &self,
        pc: &Arc<RTCPeerConnection>,
        session: &mut SignalingSession,
        state_rx: &mut mpsc::Receiver<RTCPeerConnectionState>,
    ) -> Result<()> {
        let mut answered = false;
        loop {
            tokio::select! {
                inbound = session.rx.recv() => {
                    let Some(message) = inbound else {
                        return Ok(()); // client closed the transport
                    };
                    if message.version() != PROTOCOL_VERSION {
                        let _ = session.tx.send(SignalMessage::error(
                            "bad_version",
                            format!("agent speaks protocol v{PROTOCOL_VERSION}"),
                        )).await;
                        return Err(MediaError::Signaling(format!(
                            "client protocol v{}", message.version()
                        )));
                    }
                    match message {
                        SignalMessage::Offer { sdp, .. } => {
                            if answered {
                                let _ = session.tx.send(SignalMessage::error(
                                    "renegotiation_unsupported",
                                    "one offer per session",
                                )).await;
                                return Err(MediaError::Signaling("second offer".into()));
                            }
                            answered = true;
                            let answer_sdp = self.answer(pc, sdp).await?;
                            if session.tx.send(SignalMessage::Answer {
                                v: PROTOCOL_VERSION,
                                sdp: answer_sdp,
                            }).await.is_err() {
                                return Ok(()); // transport gone mid-answer
                            }
                        }
                        SignalMessage::Candidate { candidate, sdp_mid, sdp_mline_index, .. } => {
                            let init = RTCIceCandidateInit {
                                candidate,
                                sdp_mid,
                                sdp_mline_index,
                                ..Default::default()
                            };
                            if let Err(e) = pc.add_ice_candidate(init).await {
                                // A malformed candidate degrades to "that
                                // path is unavailable"; other candidates may
                                // still connect.
                                tracing::debug!("ignoring bad ICE candidate: {e}");
                            }
                        }
                        SignalMessage::Answer { .. } | SignalMessage::Error { .. } => {
                            let _ = session.tx.send(SignalMessage::error(
                                "bad_message",
                                "unexpected message direction",
                            )).await;
                            return Err(MediaError::Signaling("client sent agent-only message".into()));
                        }
                    }
                }
                state = state_rx.recv() => {
                    let Some(state) = state else { continue };
                    tracing::debug!("peer connection state: {state}");
                    match state {
                        RTCPeerConnectionState::Connected => {
                            // A fresh viewer can only start decoding at an
                            // IDR; ask for one now rather than waiting out
                            // the GOP.
                            self.keyframes.request();
                        }
                        RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed => return Ok(()),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Answers a client offer, returning the complete (non-trickle) SDP.
    ///
    /// The agent gathers only LAN host candidates (no STUN is configured),
    /// which completes in milliseconds — so waiting for gathering and
    /// sending one final answer is simpler and race-free compared to
    /// trickling agent candidates.
    async fn answer(&self, pc: &Arc<RTCPeerConnection>, offer_sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| MediaError::Signaling(format!("bad offer sdp: {e}")))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| MediaError::Peer(format!("set remote description: {e}")))?;

        let answer = pc
            .create_answer(None)
            .await
            .map_err(|e| MediaError::Peer(format!("create answer: {e}")))?;

        let mut gathered = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .map_err(|e| MediaError::Peer(format!("set local description: {e}")))?;
        let _ = gathered.recv().await;

        let local = pc
            .local_description()
            .await
            .ok_or_else(|| MediaError::Peer("no local description after gathering".into()))?;
        Ok(local.sdp)
    }
}

/// The H.264 capability offered and produced: Constrained Baseline
/// (`42e01f`), packetization-mode 1 — what openh264 emits by default and
/// every browser accepts.
fn h264_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_owned(),
        rtcp_feedback: vec![],
    }
}

/// The Opus capability: 48 kHz stereo payload format (the RTP clock rate for
/// Opus is always 48 kHz; actual encoded channels are self-described by the
/// packets).
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}
