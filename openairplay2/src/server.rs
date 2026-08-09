//! AirPlay 2 control server: the accept loop and the per-connection concerns
//! that live *below* request dispatch — the handshake timeout, the cipher
//! install after `pair-setup`, the `SETUP` takeover claim, and the headers
//! every response carries. Requests themselves are routed by
//! [`crate::handlers::dispatch`].

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};

/// Ceiling on concurrent control connections. The design is one sender → one
/// stream, so this is generous headroom (a sender plus a handful of probes),
/// while bounding what an unauthenticated LAN peer can reserve by opening
/// sockets — each connection can otherwise hold a task and reserve a request
/// body up to `crypto_stream::MAX_BODY`.
const MAX_CONNECTIONS: usize = 32;

/// How long an unpaired connection may take to deliver each request before it
/// is dropped. It applies only while the channel is still in the clear — a
/// pairing sender is actively talking, so seconds is ample — so it never
/// disturbs an established, legitimately idle session, while denying a
/// slowloris peer a task held open indefinitely by dribbling bytes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

use crate::cipher::control_channel;
use crate::crypto_stream::ControlConnection;
use crate::events::EventSender;
use crate::handlers::{self, PAIRING_CONTENT_TYPE};
use crate::http::{Request, Response};
use crate::identity::Identity;
use crate::pairing::{Outcome, PairSetup};
use crate::session::Session;
use crate::sink::SinkFactory;
use crate::takeover::{next_connection_id, ActiveSlot};
use crate::Config;

pub const SERVER_ID: &str = "AirTunes/366.0";

pub struct Context {
    pub config: Config,
    pub identity: Identity,
    /// Creates the host's audio sink at SETUP phase 2, once per stream.
    pub sink_factory: SinkFactory,
    /// Where sessions report their milestones to the host.
    pub events: EventSender,
    /// Which connection is allowed to play: AirPlay 2 is last-stream-wins, so
    /// a new sender's `SETUP` takes this from whoever holds it (see
    /// [`crate::takeover`]).
    pub active: Arc<ActiveSlot>,
}

pub async fn serve(listener: TcpListener, context: Arc<Context>) -> io::Result<()> {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, peer) = listener.accept().await?;
        // Bound concurrency: over the ceiling, accept and immediately drop
        // rather than queueing (a queued attacker connection is a held socket
        // too). The permit is released when the task ends.
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            debug!("[{peer}] refused: at {MAX_CONNECTIONS} connections");
            drop(stream);
            continue;
        };
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            debug!("[{peer}] connected");
            if let Err(e) = handle_connection(stream, peer, context).await {
                warn!("[{peer}] connection error: {e}");
            }
            debug!("[{peer}] disconnected");
        });
    }
}

/// What one read from the control connection produced.
enum Incoming {
    Request(Request),
    /// The sender hung up.
    Closed,
    /// Nothing arrived within [`HANDSHAKE_TIMEOUT`] while still in the clear.
    HandshakeTimeout,
}

/// Read the next request, applying the handshake timeout while the channel is
/// still in the clear. Cancel-safe only in the sense the caller needs: if the
/// connection is being taken over the whole connection is closed, so an
/// abandoned partial read costs nothing.
async fn next_request<R, W>(conn: &mut ControlConnection<R, W>) -> io::Result<Incoming>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    if conn.is_encrypted() {
        return Ok(match conn.read_request().await? {
            Some(request) => Incoming::Request(request),
            None => Incoming::Closed,
        });
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.read_request()).await {
        Ok(result) => Ok(match result? {
            Some(request) => Incoming::Request(request),
            None => Incoming::Closed,
        }),
        Err(_) => Ok(Incoming::HandshakeTimeout),
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    context: Arc<Context>,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let local_ip = stream.local_addr()?.ip();
    let (read_half, write_half) = stream.into_split();
    let mut conn = ControlConnection::new(read_half, write_half);
    let mut pair = PairSetup::new(context.config.password.as_deref());
    let mut session = Session::new(
        local_ip,
        peer.ip(),
        context.sink_factory.clone(),
        context.events.clone(),
    );
    // This connection's identity in the active-session slot, and how it is
    // told that another sender has taken over.
    let conn_id = next_connection_id();
    let evicted = Arc::new(Notify::new());

    loop {
        // While still in the clear (discovery + pairing), bound how long a
        // request may take to arrive: an unpaired peer that stops sending is a
        // resource leak, not a paused session. Once encrypted, a session may
        // idle indefinitely between requests, so the timeout is lifted.
        //
        // Either wait is abandoned the moment another sender takes the
        // session over: closing the connection is the whole signal an
        // interrupted sender needs — it pauses itself and drops the route
        // (verified against a HomePod, see plans/20260808-04).
        let next = tokio::select! {
            biased;
            _ = evicted.notified() => {
                debug!("[{peer}] taken over by another sender; closing");
                return Ok(());
            }
            outcome = next_request(&mut conn) => outcome?,
        };
        let request = match next {
            Incoming::Request(request) => request,
            Incoming::Closed => break,
            Incoming::HandshakeTimeout => {
                debug!("[{peer}] handshake timed out");
                return Ok(());
            }
        };
        log_request(&peer, &request, conn.is_encrypted());

        // `/pair-setup` advances the state machine and may install the cipher
        // (after the plaintext M4 response is written).
        if request.method == "POST" && request.target == "/pair-setup" {
            let outcome = pair.handle(&request.body);
            let (tlv, secret) = match outcome {
                Outcome::Continue(tlv) => (tlv, None),
                Outcome::Failed(tlv) => (tlv, None),
                Outcome::Done {
                    response,
                    shared_secret,
                } => (response, Some(shared_secret)),
            };
            let response = finalize(
                Response::ok(&request.protocol).body(PAIRING_CONTENT_TYPE, tlv),
                &request,
            );
            conn.write_response(&response).await?;
            if let Some(secret) = secret {
                let (enc, dec) = control_channel(&secret);
                conn.enable_encryption(enc, dec);
            }
            continue;
        }

        // A `SETUP` is a sender saying it intends to play, and AirPlay 2 is
        // last-stream-wins: take the session from whoever holds it. This is
        // the first SETUP (phase 1, ports only) in the normal sequence, so
        // the interrupted stream is already gone by the time phase 2 asks the
        // host for a sink — the two never hold the audio device at once.
        // Probing connections (`GET /info`, pairing) never reach here, so
        // browsing senders don't disturb playback.
        if request.method == "SETUP" {
            if let Some(guard) = context.active.claim(conn_id, evicted.clone()).await {
                session.set_active_guard(guard);
            }
        }

        let response = handlers::dispatch(&request, &mut session, &context).await;
        let response = finalize(response, &request);
        debug!("[{peer}] -> {} {}", request.method, response.status());
        conn.write_response(&response).await?;
    }
    Ok(())
}

fn log_request(peer: &SocketAddr, request: &Request, encrypted: bool) {
    let lock = if encrypted { " [enc]" } else { "" };
    debug!(
        "[{peer}]{lock} {} {} {}",
        request.method, request.target, request.protocol
    );
    for (name, value) in request.headers.iter() {
        debug!("[{peer}]   {name}: {value}");
    }
    if !request.body.is_empty() {
        // Dump the body as hex so the real request contents (SETUP plists,
        // pairing TLVs) can be inspected offline during bring-up.
        debug!(
            "[{peer}]   body ({} bytes): {}",
            request.body.len(),
            hex::encode(&request.body)
        );
    }
}

/// Add the headers every response carries (echoed `CSeq`, `Server`).
fn finalize(mut response: Response, request: &Request) -> Response {
    if let Some(cseq) = request.headers.get("CSeq") {
        response = response.header("CSeq", cseq);
    }
    response.header("Server", SERVER_ID)
}
