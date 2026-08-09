//! The WebSocket client: keeps a connection to a receiver's now-playing
//! endpoint, and reports what happens on it to the display.
//!
//! The receiver may be down when the display starts, may restart under it,
//! or may live on a machine that reboots — so this reconnects for as long as
//! the display runs, and the display shows the connection state rather than
//! a stale screen or an exit. With no endpoint argument, it looks for a
//! local receiver first: the Unix socket the receiver serves by default,
//! then the legacy loopback TCP endpoint.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use openairplay2_tui_protocol::Message;

/// First reconnect delay, doubling up to [`MAX_BACKOFF`] and reset by a
/// successful connection.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// The TCP endpoint a receiver serves with `--tui-listen`'s documented
/// example address — the pre-socket default, kept as the last candidate so
/// every setup that worked before keeps working.
pub const DEFAULT_TCP_ENDPOINT: &str = "ws://127.0.0.1:7392";

/// One way to reach a receiver's now-playing endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    /// A `ws://` / `wss://` URL, over TCP.
    Url(String),
    /// A local Unix socket path (the receiver's `--tui-socket`).
    Socket(PathBuf),
}

impl Endpoint {
    /// Classify an endpoint argument: `ws://` and `wss://` are URLs,
    /// anything else is a socket path.
    pub fn parse(value: &str) -> Endpoint {
        if value.starts_with("ws://") || value.starts_with("wss://") {
            Endpoint::Url(value.to_string())
        } else {
            Endpoint::Socket(PathBuf::from(value))
        }
    }

    /// What to show a human for this endpoint.
    pub fn label(&self) -> String {
        match self {
            Endpoint::Url(url) => url.clone(),
            Endpoint::Socket(path) => path.display().to_string(),
        }
    }
}

/// Where to look when no endpoint was given: the receiver's default socket
/// paths — per-user first (a receiver run by hand), then the system one
/// (the packaged service) — and finally the legacy TCP default.
pub fn default_endpoints(xdg_runtime_dir: Option<&str>) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    if let Some(dir) = xdg_runtime_dir.filter(|dir| !dir.is_empty()) {
        endpoints.push(Endpoint::Socket(
            PathBuf::from(dir).join("openairplay2").join("tui.sock"),
        ));
    }
    endpoints.push(Endpoint::Socket(PathBuf::from(
        "/run/openairplay2/tui.sock",
    )));
    endpoints.push(Endpoint::Url(DEFAULT_TCP_ENDPOINT.to_string()));
    endpoints
}

/// What the client tells the display.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The socket is up; the label says where. A snapshot follows
    /// immediately.
    Connected(String),
    /// A message from the receiver. Boxed: a snapshot carries the artwork,
    /// which dwarfs every other variant.
    Message(Box<Message>),
    /// The socket went away; the client is retrying.
    Disconnected,
    /// The receiver said 401: it wants a password we don't have (or ours is
    /// wrong). Retrying slowly — the answer will not change until someone
    /// changes a configuration.
    Unauthorized,
    /// No-endpoint mode only: a full round of the local candidates found no
    /// receiver, so the display should offer the network's instead.
    NoLocalReceiver,
    /// What browsing the network has turned up (or that it cannot happen
    /// here). Sent by [`discover`](crate::discover), on the same channel so
    /// the display has one stream of truth.
    Discovery(crate::discover::Discovery),
}

/// Try `endpoints` in order, forward everything from the first that answers
/// to `updates`, and reconnect for as long as the display is listening — a
/// dropped connection starts a new round from the first (most preferred)
/// candidate. Returns when the display is gone.
pub async fn run(
    endpoints: Vec<Endpoint>,
    password: Option<String>,
    updates: UnboundedSender<Update>,
) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        let mut update = Update::Disconnected;
        for endpoint in &endpoints {
            match session(endpoint, password.as_deref(), &updates).await {
                // A connection that worked earns a fresh backoff and a fresh
                // round: the receiver that dropped may come back anywhere.
                Ok(true) => {
                    backoff = FIRST_BACKOFF;
                    update = Update::Disconnected;
                    break;
                }
                Ok(false) => return, // the display hung up
                Err(e) => {
                    debug!("connection to {} failed: {e}", endpoint.label());
                    if is_unauthorized(&e) {
                        // Don't hammer a receiver that said no: the answer is
                        // a configuration, not a transient.
                        backoff = MAX_BACKOFF;
                        update = Update::Unauthorized;
                    }
                }
            }
        }
        if updates.send(update).is_err() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// The no-endpoint mode: keep trying the `local` candidates, and while none
/// answers, let the display offer what discovery found — a receiver chosen
/// there arrives on `selections` and becomes the endpoint, from then on
/// treated exactly like an explicit argument. Until a choice is made, a
/// *local* receiver appearing wins automatically: the zero-config case must
/// survive the display being started before its receiver. A discovered
/// (network) receiver is never auto-connected — that is a choice, not a
/// default.
pub async fn run_local_or_selected(
    local: Vec<Endpoint>,
    password: Option<String>,
    updates: UnboundedSender<Update>,
    mut selections: UnboundedReceiver<Endpoint>,
) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        for endpoint in &local {
            // A choice made while we were probing wins over more probing.
            match selections.try_recv() {
                Ok(selected) => return run(vec![selected], password, updates).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            match session(endpoint, password.as_deref(), &updates).await {
                Ok(true) => {
                    // Connected and later dropped: say so now — the round
                    // continues, and a stale "connected" screen would lie —
                    // then look local-first again from a fresh backoff.
                    backoff = FIRST_BACKOFF;
                    if updates.send(Update::Disconnected).is_err() {
                        return;
                    }
                }
                Ok(false) => return, // the display hung up
                Err(e) => {
                    debug!("connection to {} failed: {e}", endpoint.label());
                    // A local endpoint that *refuses* us (the legacy TCP one
                    // with a password) is not one we can silently use — the
                    // picker is still the right screen, just without the
                    // hammering.
                    if is_unauthorized(&e) {
                        backoff = MAX_BACKOFF;
                    }
                }
            }
        }
        if updates.send(Update::NoLocalReceiver).is_err() {
            return;
        }
        tokio::select! {
            selected = selections.recv() => match selected {
                Some(endpoint) => return run(vec![endpoint], password, updates).await,
                None => return, // the display is gone
            },
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn is_unauthorized(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(
        e,
        tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
    )
}

/// One connection attempt. `Ok(true)` means it connected and later dropped;
/// `Ok(false)` means the display is gone and we should stop.
async fn session(
    endpoint: &Endpoint,
    password: Option<&str>,
    updates: &UnboundedSender<Update>,
) -> Result<bool, tokio_tungstenite::tungstenite::Error> {
    match endpoint {
        Endpoint::Url(url) => {
            // The password travels as an Authorization header on the upgrade
            // request — a handshake detail, not a protocol message. `main`
            // validated that it can be a header, so the parse cannot fail.
            let mut request = url.as_str().into_client_request()?;
            if let Some(password) = password {
                if let Ok(header) = format!("Bearer {password}").parse() {
                    request.headers_mut().insert(
                        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                        header,
                    );
                }
            }
            let (socket, _) = tokio_tungstenite::connect_async(request).await?;
            connected(socket, endpoint, updates).await
        }
        Endpoint::Socket(path) => {
            // No password over the socket: file permissions did the gating.
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
            // The upgrade needs a nominal URL; the host is meaningless here.
            let request = "ws://localhost/".into_client_request()?;
            let (socket, _) = tokio_tungstenite::client_async(request, stream).await?;
            connected(socket, endpoint, updates).await
        }
    }
}

/// Forward one established connection's messages until it drops. Generic
/// over the stream: TCP and the Unix socket speak the same protocol.
async fn connected<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: WebSocketStream<S>,
    endpoint: &Endpoint,
    updates: &UnboundedSender<Update>,
) -> Result<bool, tokio_tungstenite::tungstenite::Error> {
    info!("connected to {}", endpoint.label());
    if updates.send(Update::Connected(endpoint.label())).is_err() {
        return Ok(false);
    }

    while let Some(frame) = socket.next().await {
        let text = match frame? {
            WsMessage::Text(text) => text,
            // Pings are answered by the library; nothing else is expected.
            WsMessage::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<Message>(&text) {
            Ok(message) => {
                if updates.send(Update::Message(Box::new(message))).is_err() {
                    return Ok(false);
                }
            }
            // A message type this build doesn't know: skip it rather than
            // dropping the connection, so an older display keeps working
            // against a newer receiver.
            Err(e) => warn!("ignoring unrecognized message: {e}"),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_values_classify_as_url_or_path() {
        assert_eq!(
            Endpoint::parse("ws://10.0.0.5:7392"),
            Endpoint::Url("ws://10.0.0.5:7392".into())
        );
        assert_eq!(
            Endpoint::parse("wss://receiver.local/now-playing"),
            Endpoint::Url("wss://receiver.local/now-playing".into())
        );
        assert_eq!(
            Endpoint::parse("/run/openairplay2/tui.sock"),
            Endpoint::Socket(PathBuf::from("/run/openairplay2/tui.sock"))
        );
        // Relative paths are paths too.
        assert_eq!(
            Endpoint::parse("tui.sock"),
            Endpoint::Socket(PathBuf::from("tui.sock"))
        );
    }

    #[tokio::test]
    async fn no_local_receiver_is_reported_and_a_choice_connects() {
        // The no-endpoint flow end to end: local candidates that answer
        // nothing, the "offer the network" signal, then a picker choice
        // arriving and behaving like an explicit endpoint.
        let missing = std::env::temp_dir().join(format!(
            "openairplay2-tui-test-{}-no-receiver.sock",
            std::process::id()
        ));
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (selections_tx, selections_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run_local_or_selected(
            vec![Endpoint::Socket(missing)],
            None,
            updates_tx,
            selections_rx,
        ));

        // A full local round found nothing.
        assert_eq!(updates_rx.recv().await, Some(Update::NoLocalReceiver));

        // A receiver on the network, as the picker would name it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            use futures_util::SinkExt;
            let text = serde_json::to_string(&Message::Paused { paused: true }).unwrap();
            socket.send(WsMessage::Text(text.into())).await.unwrap();
            // Hold the connection open until the test is done with it.
            while socket.next().await.is_some() {}
        });
        selections_tx.send(Endpoint::parse(&url)).unwrap();

        // The choice connects (skipping however many more empty local
        // rounds finished first) and its messages flow.
        loop {
            match updates_rx.recv().await.expect("the client stays up") {
                Update::NoLocalReceiver => continue,
                Update::Connected(label) => {
                    assert_eq!(label, url);
                    break;
                }
                other => panic!("unexpected update: {other:?}"),
            }
        }
        assert_eq!(
            updates_rx.recv().await,
            Some(Update::Message(Box::new(Message::Paused { paused: true })))
        );
    }

    #[test]
    fn default_candidates_prefer_the_local_socket() {
        // A user session: their own runtime dir first, then the service's
        // socket, then the legacy TCP default.
        assert_eq!(
            default_endpoints(Some("/run/user/1000")),
            vec![
                Endpoint::Socket(PathBuf::from("/run/user/1000/openairplay2/tui.sock")),
                Endpoint::Socket(PathBuf::from("/run/openairplay2/tui.sock")),
                Endpoint::Url(DEFAULT_TCP_ENDPOINT.into()),
            ]
        );
        // No runtime dir (or an empty variable): the system socket, then TCP.
        for xdg in [None, Some("")] {
            assert_eq!(
                default_endpoints(xdg),
                vec![
                    Endpoint::Socket(PathBuf::from("/run/openairplay2/tui.sock")),
                    Endpoint::Url(DEFAULT_TCP_ENDPOINT.into()),
                ]
            );
        }
    }
}
