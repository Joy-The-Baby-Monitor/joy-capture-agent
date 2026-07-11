//! End-to-end media-plane test with no browser and no network beyond UDP
//! loopback: a second in-process webrtc-rs peer plays the client role.
//!
//! Simulated capture sources feed the real encoder tasks, which write into
//! the real `StreamHub` tracks; the "client" peer sends an offer through an
//! in-memory `Signaling` implementation, and the test passes once RTP flows
//! for both video and audio. This exercises the signaling trait seam, codec
//! negotiation, ICE over loopback, encoding, and sample packetization —
//! everything the browser path uses except the WebSocket transport.

use std::sync::Arc;
use std::time::Duration;

use joy_capture::{
    AudioConfig, AudioSource, SimulatedAudioSource, SimulatedVideoSource, VideoConfig, VideoSource,
};
use joy_media::encode::KeyframeRequester;
use joy_media::peer::StreamHub;
use joy_media::signaling::{PROTOCOL_VERSION, SignalMessage, Signaling, SignalingSession};
use joy_media::{VideoEncodeConfig, run_audio_encoder, run_video_encoder};
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;

/// A `Signaling` transport that yields a fixed set of pre-built sessions —
/// the in-memory stand-in for the WebSocket server.
struct StaticSignaling {
    sessions: Vec<SignalingSession>,
}

impl Signaling for StaticSignaling {
    async fn accept(&mut self) -> Option<SignalingSession> {
        self.sessions.pop()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_peer_receives_rtp_on_both_tracks() {
    // Agent side: simulated sources → encoder tasks → hub tracks.
    let video = SimulatedVideoSource::open(&VideoConfig {
        width: 320,
        height: 240,
        fps: 15,
        ..Default::default()
    })
    .await
    .expect("video source");
    let audio = SimulatedAudioSource::open(&AudioConfig::default())
        .await
        .expect("audio source");

    let (keyframes, keyframe_rx) = KeyframeRequester::new();
    let hub = Arc::new(StreamHub::new(keyframes).expect("hub"));

    tokio::spawn(run_video_encoder(
        video.subscribe(),
        hub.video_track(),
        VideoEncodeConfig::default(),
        keyframe_rx,
    ));
    tokio::spawn(run_audio_encoder(audio.subscribe(), hub.audio_track()));

    // Wire one in-memory signaling session between hub and test client.
    let (client_out_tx, agent_in_rx) = mpsc::channel(32);
    let (agent_out_tx, mut client_in_rx) = mpsc::channel(32);
    let session = SignalingSession {
        rx: agent_in_rx,
        tx: agent_out_tx,
    };
    tokio::spawn(Arc::clone(&hub).run(StaticSignaling {
        sessions: vec![session],
    }));

    // Client side: a vanilla webrtc-rs peer with browser-default codecs and
    // two recvonly transceivers, exactly like the dev page.
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs().expect("codecs");
    let client_api = APIBuilder::new().with_media_engine(media_engine).build();
    let client_pc = Arc::new(
        client_api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .expect("client peer"),
    );

    let recvonly = || RTCRtpTransceiverInit {
        direction: RTCRtpTransceiverDirection::Recvonly,
        send_encodings: vec![],
    };
    client_pc
        .add_transceiver_from_kind(RTPCodecType::Video, Some(recvonly()))
        .await
        .expect("video transceiver");
    client_pc
        .add_transceiver_from_kind(RTPCodecType::Audio, Some(recvonly()))
        .await
        .expect("audio transceiver");

    // Report each track kind once the first RTP packet is read from it.
    let (rtp_tx, mut rtp_rx) = mpsc::channel::<String>(4);
    client_pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let rtp_tx = rtp_tx.clone();
        Box::pin(async move {
            let kind = track.kind().to_string();
            let mut buf = vec![0u8; 2048];
            if track.read(&mut buf).await.is_ok() {
                let _ = rtp_tx.send(kind).await;
            }
        })
    }));

    // Non-trickle offer: gather every client candidate up front so no
    // candidate messages are needed in either direction.
    let offer = client_pc.create_offer(None).await.expect("create offer");
    let mut gathered = client_pc.gathering_complete_promise().await;
    client_pc
        .set_local_description(offer)
        .await
        .expect("set local");
    let _ = gathered.recv().await;
    let offer_sdp = client_pc
        .local_description()
        .await
        .expect("local description")
        .sdp;

    client_out_tx
        .send(SignalMessage::Offer {
            v: PROTOCOL_VERSION,
            sdp: offer_sdp,
        })
        .await
        .expect("send offer");

    // The agent must answer with a complete SDP.
    let answer = tokio::time::timeout(Duration::from_secs(10), client_in_rx.recv())
        .await
        .expect("no answer within 10s")
        .expect("signaling closed before answer");
    let answer_sdp = match answer {
        SignalMessage::Answer { v, sdp } => {
            assert_eq!(v, PROTOCOL_VERSION);
            sdp
        }
        other => panic!("expected answer, got {other:?}"),
    };
    client_pc
        .set_remote_description(RTCSessionDescription::answer(answer_sdp).expect("answer sdp"))
        .await
        .expect("set remote");

    // Success = RTP observed on both tracks within the timeout.
    let mut kinds_seen = std::collections::HashSet::new();
    while kinds_seen.len() < 2 {
        let kind = tokio::time::timeout(Duration::from_secs(15), rtp_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for RTP; saw only {kinds_seen:?}"))
            .expect("rtp channel closed");
        kinds_seen.insert(kind);
    }
    assert!(kinds_seen.contains("video"), "{kinds_seen:?}");
    assert!(kinds_seen.contains("audio"), "{kinds_seen:?}");

    client_pc.close().await.expect("client close");
}
