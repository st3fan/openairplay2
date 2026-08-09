//! The web layer: one handler per HTTP/RTSP endpoint, and the dispatch table
//! that routes a request to it — the router, in a codebase whose hybrid
//! protocol rules out an off-the-shelf one.
//!
//! A handler owns exactly the web-side concerns: parse the wire body, build
//! the command's params, call the command, shape the response — and state its
//! tolerance policy where an error must *not* reach the sender (see
//! [`crate::errors`]). Validation lives in the commands; connection-level
//! concerns (the cipher install after `pair-setup`, the `SETUP` takeover
//! claim, the `CSeq`/`Server` echo) stay in [`crate::server`].

mod feedback;
mod fp_setup;
mod info;
mod pair_pin_start;

use log::warn;

use crate::http::{Request, Response};
use crate::server::Context;
use crate::session::Session;

pub const INFO_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const PAIRING_CONTENT_TYPE: &str = "application/octet-stream";
pub const PARAMETERS_CONTENT_TYPE: &str = "text/parameters";

/// Route a request to its handler. RTSP session verbs dispatch on the method
/// (their target is an `rtsp://…` URL); HTTP endpoints dispatch on method +
/// target. Anything unknown answers 501 so a real sender's next move shows
/// up in the logs.
pub async fn dispatch(request: &Request, session: &mut Session, context: &Context) -> Response {
    if let Some(response) = dispatch_session(session, request).await {
        return response;
    }
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/info") => info::handle_info(request, context),
        ("POST", "/fp-setup") => fp_setup::handle_fp_setup(request),
        // Keep-alive / control methods a sender interleaves; acknowledge so
        // the session survives long enough to reach SETUP.
        ("POST", "/feedback" | "/command" | "/audioMode") => feedback::handle_feedback(request),
        ("POST", "/pair-pin-start") => pair_pin_start::handle_pair_pin_start(request),
        (method, target) => {
            warn!("{method} {target} not implemented yet");
            Response::new(&request.protocol, 501, "Not Implemented")
        }
    }
}

/// The streaming-session verbs, still dispatched to [`Session`] methods.
/// Temporary: phases 2–4 of plan `20260809-03` dissolve these into per-verb
/// handlers and commands, and this function with them.
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
