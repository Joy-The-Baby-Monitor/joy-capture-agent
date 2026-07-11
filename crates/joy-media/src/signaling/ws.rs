//! LAN WebSocket signaling transport.
//!
//! [`LanSignaling`] is the v1 implementation of the [`Signaling`] trait: an
//! axum route (`GET /signal`) that upgrades each connection to a WebSocket
//! and pumps JSON text frames to and from a [`SignalingSession`]'s channel
//! pair. The route is returned as an [`axum::Router`] rather than a bound
//! server so the binary owns the listener and can mount other routers (the
//! dev UI now, the control plane later) on the same port.

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use tokio::sync::mpsc;

use super::{SignalMessage, Signaling, SignalingSession};

/// Capacity of each per-session message channel. Signaling traffic is a
/// handful of messages per connection; this bound exists only to keep a
/// stalled peer from queueing unboundedly.
const SESSION_CHANNEL_CAPACITY: usize = 32;

/// Capacity of the accept queue between the WS handler and the peer manager.
const ACCEPT_QUEUE_CAPACITY: usize = 16;

/// WebSocket-server implementation of [`Signaling`] for the home LAN.
///
/// Created together with the router that feeds it; accepted sessions arrive
/// in connection order. Dropping the `LanSignaling` closes the accept queue,
/// after which new WebSocket connections are refused at upgrade time.
#[derive(Debug)]
pub struct LanSignaling {
    accept_rx: mpsc::Receiver<SignalingSession>,
}

impl LanSignaling {
    /// Builds the transport and the axum router serving it.
    ///
    /// Mount the router on any listener; each WebSocket connection to
    /// `/signal` becomes one [`SignalingSession`] returned from
    /// [`Signaling::accept`].
    pub fn new() -> (Self, Router) {
        let (accept_tx, accept_rx) = mpsc::channel(ACCEPT_QUEUE_CAPACITY);
        let router = Router::new()
            .route("/signal", any(upgrade_handler))
            .with_state(accept_tx);
        (Self { accept_rx }, router)
    }
}

impl Signaling for LanSignaling {
    async fn accept(&mut self) -> Option<SignalingSession> {
        self.accept_rx.recv().await
    }
}

/// Upgrades an incoming connection and hands the socket to the pump task.
async fn upgrade_handler(
    ws: WebSocketUpgrade,
    State(accept_tx): State<mpsc::Sender<SignalingSession>>,
) -> Response {
    ws.on_upgrade(move |socket| pump(socket, accept_tx))
}

/// Bridges one WebSocket to one [`SignalingSession`] until either side ends.
///
/// Inbound text frames are decoded as [`SignalMessage`]s; an undecodable
/// frame is a protocol violation that earns the client a `bad_message` error
/// and a close, per the §8 contract. Outbound messages are encoded to JSON
/// text. Non-text frames other than Close are ignored (axum answers Ping
/// frames itself).
async fn pump(mut socket: WebSocket, accept_tx: mpsc::Sender<SignalingSession>) {
    let (in_tx, in_rx) = mpsc::channel(SESSION_CHANNEL_CAPACITY);
    let (out_tx, mut out_rx) = mpsc::channel::<SignalMessage>(SESSION_CHANNEL_CAPACITY);

    let session = SignalingSession {
        rx: in_rx,
        tx: out_tx,
    };
    if accept_tx.send(session).await.is_err() {
        // The peer manager is gone; refuse the client cleanly.
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    loop {
        tokio::select! {
            outbound = out_rx.recv() => {
                let Some(message) = outbound else {
                    // Session dropped by the peer manager — close the socket.
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                };
                let Ok(json) = serde_json::to_string(&message) else {
                    tracing::error!("failed to serialize signaling message");
                    continue;
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                let Some(Ok(frame)) = inbound else {
                    break; // socket closed or errored
                };
                match frame {
                    Message::Text(text) => match serde_json::from_str(text.as_str()) {
                        Ok(message) => {
                            if in_tx.send(message).await.is_err() {
                                // Session handler gone; stop reading.
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::debug!("undecodable signaling frame: {e}");
                            let error = SignalMessage::error(
                                "bad_message",
                                "frame is not a valid signaling message",
                            );
                            if let Ok(json) = serde_json::to_string(&error) {
                                let _ = socket.send(Message::Text(json.into())).await;
                            }
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                    },
                    Message::Close(_) => break,
                    _ => {} // binary/ping/pong: ignored
                }
            }
        }
    }
}
