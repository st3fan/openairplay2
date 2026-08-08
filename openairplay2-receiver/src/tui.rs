//! The now-playing endpoint: what is playing, published over a WebSocket.
//!
//! `--tui-listen ADDR` turns this on. Every connected client gets a
//! [`Message::Snapshot`] immediately and then one message per change, so a
//! display started mid-track shows a full screen at once rather than waiting
//! for the sender to do something — which, for a sender parked on one track,
//! may be never.
//!
//! Two rules shape the design, both about not letting a display hurt the audio
//! path:
//!
//! - The fan-out channel is **bounded**. A display that stops reading fills its
//!   slice of the channel and is dropped from it; it never applies
//!   backpressure to the receiver's event task.
//! - A client that fell behind is **resynced, not disconnected**: it gets a
//!   fresh snapshot. Artwork frames are hundreds of KB, and a client that
//!   missed one is better off resynced than killed.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use openairplay2::Event;
use openairplay2_tui_protocol::{Message, ReceiverInfo, Snapshot};

/// Messages buffered per client before it is considered too far behind. A
/// track change is a handful of messages, so this is many tracks of slack.
const CHANNEL_CAPACITY: usize = 64;
/// How often to ping an idle client, so a laptop that closed its lid doesn't
/// hold a connection open forever.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The receiver's side of the display socket: keeps the current snapshot and
/// fans changes out to whoever is connected.
#[derive(Clone)]
pub struct Publisher {
    snapshot: Arc<Mutex<Snapshot>>,
    changes: broadcast::Sender<Message>,
}

impl Publisher {
    /// Create a publisher describing this receiver.
    pub fn new(name: String) -> Publisher {
        let snapshot = Snapshot {
            receiver: ReceiverInfo {
                name,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            ..Snapshot::default()
        };
        Publisher {
            snapshot: Arc::new(Mutex::new(snapshot)),
            changes: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Publish one receiver event. Events with no display meaning are dropped
    /// silently.
    pub fn publish(&self, event: &Event) {
        let Some(message) = to_message(event) else {
            return;
        };
        self.snapshot.lock().unwrap().apply(&message);
        // An error only means nobody is connected, which is the normal case.
        let _ = self.changes.send(message);
    }

    fn snapshot(&self) -> Message {
        Message::Snapshot(self.snapshot.lock().unwrap().clone())
    }

    fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.changes.subscribe()
    }
}

/// Translate a receiver event into its wire message. Returns `None` for events
/// a display has no use for.
fn to_message(event: &Event) -> Option<Message> {
    Some(match event {
        Event::SessionStarted {
            rate,
            channels,
            peer,
            ..
        } => Message::SessionStarted {
            rate: *rate,
            channels: *channels,
            peer: display_addr(*peer),
        },
        Event::Metadata {
            title,
            artist,
            album,
        } => Message::Metadata {
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
        },
        Event::Artwork { content_type, data } => Message::Artwork {
            content_type: content_type.clone(),
            data_base64: STANDARD.encode(data),
        },
        Event::Volume { db } => Message::Volume { db: *db },
        Event::Progress { elapsed, duration } => Message::Progress {
            elapsed_ms: elapsed.as_millis() as u64,
            duration_ms: duration.as_millis() as u64,
        },
        Event::Paused(paused) => Message::Paused { paused: *paused },
        Event::Flushed => Message::Flushed,
        Event::SessionEnded => Message::SessionEnded,
        _ => return None,
    })
}

/// An IPv4 sender arriving on our dual-stack socket has a v4-mapped IPv6
/// address; `::ffff:192.168.1.42` on a display is just noise, so unwrap it.
fn display_addr(addr: std::net::IpAddr) -> String {
    match addr {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        },
        std::net::IpAddr::V4(v4) => v4.to_string(),
    }
}

/// Accept display connections until the listener fails. With a `password`,
/// a client must present it before the WebSocket upgrade; without one,
/// anyone who can reach the port connects (the options file's advice to
/// keep the address on loopback stands either way).
pub async fn serve(
    listener: TcpListener,
    publisher: Publisher,
    password: Option<String>,
) -> std::io::Result<()> {
    let password = Arc::new(password);
    loop {
        let (stream, peer) = listener.accept().await?;
        let publisher = publisher.clone();
        let password = password.clone();
        tokio::spawn(async move {
            debug!("tui [{peer}] connected");
            if let Err(e) = serve_client(stream, peer, publisher, password.as_deref()).await {
                debug!("tui [{peer}] ended: {e}");
            }
            debug!("tui [{peer}] disconnected");
        });
    }
}

/// Constant-time equality: fold over the longer length, so neither a
/// matching prefix nor the length short-circuits. Not that a LAN timing
/// oracle is likely — it costs nothing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

// The auth callback's `Result<Response, ErrorResponse>` is tungstenite's
// signature, not a choice — its Err carries a whole HTTP response by design.
#[allow(clippy::result_large_err)]
async fn serve_client(
    stream: TcpStream,
    peer: SocketAddr,
    publisher: Publisher,
    password: Option<&str>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    // Subscribe before sending the snapshot: a change that lands in between is
    // then queued rather than lost.
    let mut changes = publisher.subscribe();
    let mut socket = match password {
        None => tokio_tungstenite::accept_async(stream).await?,
        // The password travels as `Authorization: Bearer <password>` on the
        // upgrade request — a handshake header, not a protocol message, so
        // the published wire format is untouched.
        Some(password) => {
            use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Response};
            use tokio_tungstenite::tungstenite::http;

            let expected = format!("Bearer {password}");
            let mut authorized = false;
            let result = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &http::Request<()>, response: Response| {
                    let presented = request
                        .headers()
                        .get(http::header::AUTHORIZATION)
                        .map(|v| v.as_bytes())
                        .unwrap_or_default();
                    if constant_time_eq(presented, expected.as_bytes()) {
                        authorized = true;
                        return Ok(response);
                    }
                    let mut refusal = ErrorResponse::new(Some("password required".into()));
                    *refusal.status_mut() = http::StatusCode::UNAUTHORIZED;
                    Err(refusal)
                },
            )
            .await;
            if !authorized {
                warn!("tui [{peer}] rejected: wrong or missing password");
            }
            result?
        }
    };
    socket.send(encode(&publisher.snapshot())).await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate

    loop {
        tokio::select! {
            change = changes.recv() => match change {
                Ok(message) => socket.send(encode(&message)).await?,
                // Too far behind to catch up message by message: resync.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!("tui [{peer}] fell behind by {missed} messages; resyncing");
                    socket.send(encode(&publisher.snapshot())).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            incoming = socket.next() => match incoming {
                // The display is read-only; anything it sends is ignored
                // except a close.
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            },
            _ = ping.tick() => socket.send(WsMessage::Ping(Vec::new().into())).await?,
        }
    }
}

/// Encode a message as a WebSocket text frame. Serialization cannot fail for
/// these types; if it somehow did, an empty frame is better than a panic in
/// the receiver.
fn encode(message: &Message) -> WsMessage {
    WsMessage::Text(serde_json::to_string(message).unwrap_or_default().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use futures_util::stream::SplitStream;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    async fn start() -> (SocketAddr, Publisher) {
        start_with_password(None).await
    }

    async fn start_with_password(password: Option<&str>) -> (SocketAddr, Publisher) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let publisher = Publisher::new("Living Room".into());
        tokio::spawn(serve(
            listener,
            publisher.clone(),
            password.map(str::to_string),
        ));
        (addr, publisher)
    }

    async fn connect(addr: SocketAddr) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("the endpoint should accept the connection");
        socket
    }

    /// Read the next message, failing rather than hanging.
    async fn next(socket: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) -> Message {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .expect("socket error");
            if let WsMessage::Text(text) = frame {
                return serde_json::from_str(&text).expect("valid protocol JSON");
            }
        }
    }

    fn session_started() -> Event {
        Event::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
        }
    }

    fn metadata() -> Event {
        Event::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: None,
        }
    }

    #[tokio::test]
    async fn a_client_gets_a_snapshot_then_every_change() {
        let (addr, publisher) = start().await;
        let (_, mut rx) = connect(addr).await.split();

        match next(&mut rx).await {
            Message::Snapshot(snapshot) => {
                assert_eq!(snapshot.receiver.name, "Living Room");
                assert_eq!(snapshot.session, None, "nothing is playing yet");
            }
            other => panic!("the first message must be a snapshot, got {other:?}"),
        }

        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::Volume { db: -12.5 });
        publisher.publish(&Event::Progress {
            elapsed: Duration::from_secs(83),
            duration: Duration::from_secs(247),
        });
        publisher.publish(&Event::Paused(true));

        assert!(matches!(
            next(&mut rx).await,
            Message::SessionStarted { rate: 44100, .. }
        ));
        assert!(
            matches!(next(&mut rx).await, Message::Metadata { title: Some(t), .. } if t == "Sonata No. 1")
        );
        assert_eq!(next(&mut rx).await, Message::Volume { db: -12.5 });
        assert_eq!(
            next(&mut rx).await,
            Message::Progress {
                elapsed_ms: 83_000,
                duration_ms: 247_000
            }
        );
        assert_eq!(next(&mut rx).await, Message::Paused { paused: true });
    }

    #[tokio::test]
    async fn a_client_connecting_mid_track_sees_the_whole_state() {
        // The case the snapshot exists for: a sender parked on one track may
        // send nothing at all after this point.
        let (addr, publisher) = start().await;
        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::Artwork {
            content_type: "image/jpeg".into(),
            data: vec![1, 2, 3],
        });
        publisher.publish(&Event::Progress {
            elapsed: Duration::from_secs(5),
            duration: Duration::from_secs(200),
        });
        publisher.publish(&Event::Paused(true));

        let (_, mut rx) = connect(addr).await.split();
        let Message::Snapshot(snapshot) = next(&mut rx).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.session.unwrap().peer, "192.168.1.42");
        assert_eq!(
            snapshot.track.unwrap().title.as_deref(),
            Some("Sonata No. 1")
        );
        assert_eq!(
            snapshot.artwork.unwrap().data_base64,
            STANDARD.encode([1, 2, 3])
        );
        assert_eq!(snapshot.progress.unwrap().duration_ms, 200_000);
        assert!(snapshot.paused, "a display joining a paused stream says so");
    }

    #[tokio::test]
    async fn two_displays_both_stay_current() {
        let (addr, publisher) = start().await;
        let (_, mut first) = connect(addr).await.split();
        let (_, mut second) = connect(addr).await.split();
        assert!(matches!(next(&mut first).await, Message::Snapshot(_)));
        assert!(matches!(next(&mut second).await, Message::Snapshot(_)));

        publisher.publish(&Event::Volume { db: -3.0 });
        assert_eq!(next(&mut first).await, Message::Volume { db: -3.0 });
        assert_eq!(next(&mut second).await, Message::Volume { db: -3.0 });
    }

    #[tokio::test]
    async fn a_display_that_stops_reading_never_stalls_the_receiver() {
        let (addr, publisher) = start().await;
        let (_, mut reader) = connect(addr).await.split();
        assert!(matches!(next(&mut reader).await, Message::Snapshot(_)));
        // A second client that never reads a single frame.
        let _stalled = connect(addr).await;

        // Far more than the channel holds; publish must not block or panic.
        for i in 0..CHANNEL_CAPACITY * 4 {
            publisher.publish(&Event::Volume { db: -(i as f32) });
        }
        publisher.publish(&Event::Flushed);

        // The healthy client still gets everything, in order, ending with the
        // flush.
        let mut seen_flush = false;
        for _ in 0..CHANNEL_CAPACITY * 4 + 1 {
            if next(&mut reader).await == Message::Flushed {
                seen_flush = true;
                break;
            }
        }
        assert!(seen_flush, "the reading client must keep receiving");
    }

    #[tokio::test]
    async fn session_end_clears_the_snapshot_for_the_next_client() {
        let (addr, publisher) = start().await;
        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::SessionEnded);

        let (_, mut rx) = connect(addr).await.split();
        let Message::Snapshot(snapshot) = next(&mut rx).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.session, None);
        assert_eq!(snapshot.track, None);
        assert_eq!(snapshot.receiver.name, "Living Room");
    }

    #[test]
    fn a_v4_mapped_sender_is_shown_as_ipv4() {
        // Our listener is dual-stack, so an iPhone on IPv4 arrives as
        // ::ffff:192.168.1.42 — nobody wants to read that.
        assert_eq!(
            display_addr("::ffff:192.168.1.42".parse().unwrap()),
            "192.168.1.42"
        );
        assert_eq!(
            display_addr("192.168.1.42".parse().unwrap()),
            "192.168.1.42"
        );
        assert_eq!(display_addr("fdab:110::1".parse().unwrap()), "fdab:110::1");
    }

    #[test]
    fn pause_and_resume_both_reach_the_wire() {
        // The thing AirPlay 1 could never say out loud.
        assert_eq!(
            to_message(&Event::Paused(true)),
            Some(Message::Paused { paused: true })
        );
        assert_eq!(
            to_message(&Event::Paused(false)),
            Some(Message::Paused { paused: false })
        );
    }

    /// Connect presenting an Authorization header.
    async fn connect_with_auth(
        addr: SocketAddr,
        auth: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Error>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{addr}").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Authorization", auth.parse().unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(s, _)| s)
    }

    fn assert_unauthorized(err: tokio_tungstenite::tungstenite::Error) {
        match err {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), 401, "expected 401")
            }
            other => panic!("expected an HTTP 401 rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_password_gates_the_upgrade() {
        let (addr, _publisher) = start_with_password(Some("sekrit")).await;

        // No header, and a wrong password: refused before the upgrade.
        let err = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect_err("a connection without the password must be refused");
        assert_unauthorized(err);
        let err = connect_with_auth(addr, "Bearer wrong")
            .await
            .expect_err("a wrong password must be refused");
        assert_unauthorized(err);

        // The right password: upgraded, snapshot delivered.
        let socket = connect_with_auth(addr, "Bearer sekrit")
            .await
            .expect("the right password must connect");
        let (_, mut rx) = socket.split();
        match next(&mut rx).await {
            Message::Snapshot(snapshot) => assert_eq!(snapshot.receiver.name, "Living Room"),
            other => panic!("the first message must be a snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn without_a_password_a_plain_client_still_connects() {
        // The compatibility case: no password configured, no header sent.
        let (addr, _publisher) = start().await;
        let (_, mut rx) = connect(addr).await.split();
        assert!(matches!(next(&mut rx).await, Message::Snapshot(_)));
    }

    #[test]
    fn constant_time_eq_is_correct() {
        assert!(constant_time_eq(b"Bearer x", b"Bearer x"));
        assert!(!constant_time_eq(b"Bearer x", b"Bearer y"));
        assert!(!constant_time_eq(b"Bearer x", b"Bearer xx"));
        assert!(!constant_time_eq(b"", b"Bearer x"));
        assert!(constant_time_eq(b"", b""));
    }
}
