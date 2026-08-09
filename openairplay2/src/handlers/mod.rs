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
mod flush_buffered;
mod fp_setup;
mod get_parameter;
mod info;
mod pair_pin_start;
mod pair_setup;
mod plist;
mod record;
mod set_parameter;
mod set_rate_anchor;
mod setup;
mod teardown;

// `pair-setup` is dispatched by the connection loop, not the table below: its
// side effect (installing the cipher) is connection-level.
pub use pair_setup::handle_pair_setup;

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
    match request.method.as_str() {
        "SETUP" => return setup::handle_setup(request, session).await,
        "GET_PARAMETER" => return get_parameter::handle_get_parameter(request, session),
        "SET_PARAMETER" => return set_parameter::handle_set_parameter(request, session),
        "SETRATEANCHORTIME" => return set_rate_anchor::handle_set_rate_anchor(request, session),
        "FLUSHBUFFERED" => return flush_buffered::handle_flush_buffered(request, session),
        "TEARDOWN" => return teardown::handle_teardown(request, session),
        "RECORD" | "SETPEERS" | "SETPEERSX" => return record::handle_record(request),
        _ => {}
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
