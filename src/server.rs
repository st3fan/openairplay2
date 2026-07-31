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
use crate::fairplay;
use crate::http::{Request, Response};
use crate::identity::Identity;
use crate::info::info_plist;
use crate::pairing::{Outcome, PairSetup};
use crate::Config;

pub const SERVER_ID: &str = "AirTunes/366.0";
pub const INFO_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const PAIRING_CONTENT_TYPE: &str = "application/octet-stream";

pub struct Context {
    pub config: Config,
    pub identity: Identity,
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
    let (read_half, write_half) = stream.into_split();
    let mut conn = ControlConnection::new(read_half, write_half);
    let mut pair = PairSetup::new();

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

        let response = finalize(dispatch(&request, &context), &request);
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
