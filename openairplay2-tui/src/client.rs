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
use tokio::sync::mpsc::UnboundedSender;
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
