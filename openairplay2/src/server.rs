//! AirPlay 2 control server: accept loop and request dispatch.
//!
//! Milestone 2: `GET /info`, transient `POST /pair-setup`, and the switch to
//! the encrypted channel once pairing completes. Everything else is logged and
//! `501`'d so later milestones can see the real request sequence.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::net::{TcpListener, TcpStream};

use crate::cipher::control_channel;
use crate::crypto_stream::ControlConnection;
use crate::events::EventSender;
use crate::fairplay;
use crate::http::{Request, Response};
use crate::identity::Identity;
use crate::info::info_plist;
use crate::pairing::{Outcome, PairSetup};
use crate::session::Session;
use crate::sink::SinkFactory;
use crate::Config;

pub const SERVER_ID: &str = "AirTunes/366.0";
pub const INFO_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const PAIRING_CONTENT_TYPE: &str = "application/octet-stream";
pub const PARAMETERS_CONTENT_TYPE: &str = "text/parameters";

pub struct Context {
    pub config: Config,
    pub identity: Identity,
    /// Creates the host's audio sink at SETUP phase 2, once per stream.
    pub sink_factory: SinkFactory,
    /// Where sessions report their milestones to the host.
    pub events: EventSender,
}

pub async fn serve(listener: TcpListener, context: Arc<Context>) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let context = context.clone();
        tokio::spawn(async move {
            info!("[{peer}] connected");
            if let Err(e) = handle_connection(stream, peer, context).await {
                warn!("[{peer}] connection error: {e}");
            }
            info!("[{peer}] disconnected");
        });
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
    // The room PIN: a configured password, or the historical default. AirPlay
    // 2 always pairs, so there is no "open" mode.
    let pin = context
        .config
        .password
        .as_deref()
        .unwrap_or(crate::pairing::PAIR_SETUP_PIN);
    let mut pair = PairSetup::new(pin);
    let mut session = Session::new(
        local_ip,
        context.sink_factory.clone(),
        context.events.clone(),
    );

    while let Some(request) = conn.read_request().await? {
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

        let response = match dispatch_session(&mut session, &request).await {
            Some(response) => response,
            None => dispatch(&request, &context),
        };
        let response = finalize(response, &request);
        debug!("[{peer}] -> {} {}", request.method, response.status());
        conn.write_response(&response).await?;
    }
    Ok(())
}

fn log_request(peer: &SocketAddr, request: &Request, encrypted: bool) {
    let lock = if encrypted { " [enc]" } else { "" };
    info!(
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

/// Handle the streaming-session methods (SETUP and the control verbs) that
/// operate on the per-connection [`Session`]. Returns `None` if `request`
/// isn't one of them, so the caller can fall back to the stateless dispatch.
async fn dispatch_session(session: &mut Session, request: &Request) -> Option<Response> {
    let proto = &request.protocol;
    match request.method.as_str() {
        "SETUP" => Some(match session.handle_setup(&request.body).await {
            Ok(body) => Response::ok(proto).body(INFO_CONTENT_TYPE, body),
            Err(e) => {
                warn!("SETUP failed: {e}");
                Response::new(proto, 400, "Bad Request")
            }
        }),
        // A sender queries the current volume during setup and expects a
        // `text/parameters` body back; an empty response makes it give up.
        "GET_PARAMETER" => {
            let body = session.get_parameter(&request.body);
            Some(Response::ok(proto).body(PARAMETERS_CONTENT_TYPE, body))
        }
        "SET_PARAMETER" => {
            session.set_parameter(request.headers.get("Content-Type"), &request.body);
            Some(Response::ok(proto))
        }
        // Transport control: play/pause rate and the RTP anchor.
        "SETRATEANCHORTIME" => {
            session.set_rate_anchor(&request.body);
            Some(Response::ok(proto))
        }
        // Seek/skip: drop buffered audio.
        "FLUSHBUFFERED" => {
            session.flush(&request.body);
            Some(Response::ok(proto))
        }
        // The sender is done with the stream.
        "TEARDOWN" => {
            session.teardown();
            Some(Response::ok(proto))
        }
        // Other session control verbs: acknowledge so the sender proceeds.
        "RECORD" | "SETPEERS" | "SETPEERSX" => {
            session.ack(&request.method);
            Some(Response::ok(proto))
        }
        _ => None,
    }
}

fn dispatch(request: &Request, context: &Context) -> Response {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/info") => Response::ok(&request.protocol).body(
            INFO_CONTENT_TYPE,
            info_plist(&context.config, &context.identity),
        ),
        ("POST", "/fp-setup") => match fairplay::fp_setup(&request.body) {
            Some(reply) => Response::ok(&request.protocol).body(PAIRING_CONTENT_TYPE, reply),
            None => {
                warn!("fp-setup: malformed FairPlay request");
                Response::new(&request.protocol, 400, "Bad Request")
            }
        },
        // Keep-alive / control methods a sender interleaves; acknowledge so the
        // session survives long enough to reach SETUP.
        ("POST", "/feedback") | ("POST", "/command") | ("POST", "/audioMode") => {
            Response::ok(&request.protocol)
        }
        (method, target) => {
            warn!("{method} {target} not implemented yet");
            Response::new(&request.protocol, 501, "Not Implemented")
        }
    }
}

/// Add the headers every response carries (echoed `CSeq`, `Server`).
fn finalize(mut response: Response, request: &Request) -> Response {
    if let Some(cseq) = request.headers.get("CSeq") {
        response = response.header("CSeq", cseq);
    }
    response.header("Server", SERVER_ID)
}
