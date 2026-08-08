//! The WebSocket client: keeps a connection to a receiver's now-playing
//! endpoint, and reports what happens on it to the display.
//!
//! The receiver may be down when the display starts, may restart under it,
//! or may live on a machine that reboots — so this reconnects for as long as
//! the display runs, and the display shows the connection state rather than
//! a stale screen or an exit.

use std::time::Duration;

use futures_util::StreamExt;
use log::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use openairplay2_tui_protocol::Message;

/// First reconnect delay, doubling up to [`MAX_BACKOFF`] and reset by a
/// successful connection.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// What the client tells the display.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The socket is up; a snapshot follows immediately.
    Connected,
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

/// Connect to `url` and forward everything to `updates`, reconnecting for as
/// long as the display is listening. Returns when the display is gone.
pub async fn run(url: String, password: Option<String>, updates: UnboundedSender<Update>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        let update = match session(&url, password.as_deref(), &updates).await {
            // A connection that worked earns a fresh backoff.
            Ok(true) => {
                backoff = FIRST_BACKOFF;
                Update::Disconnected
            }
            Ok(false) => return, // the display hung up
            Err(e) => {
                debug!("connection to {url} failed: {e}");
                if is_unauthorized(&e) {
                    // Don't hammer a receiver that said no: the answer is a
                    // configuration, not a transient.
                    backoff = MAX_BACKOFF;
                    Update::Unauthorized
                } else {
                    Update::Disconnected
                }
            }
        };
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
    url: &str,
    password: Option<&str>,
    updates: &UnboundedSender<Update>,
) -> Result<bool, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    // The password travels as an Authorization header on the upgrade
    // request — a handshake detail, not a protocol message. `main` validated
    // that it can be a header, so the parse cannot fail here.
    let mut request = url.into_client_request()?;
    if let Some(password) = password {
        if let Ok(header) = format!("Bearer {password}").parse() {
            request.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                header,
            );
        }
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
    debug!("connected to {url}");
    if updates.send(Update::Connected).is_err() {
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
