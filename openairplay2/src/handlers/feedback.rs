//! `POST /feedback`, `/command`, `/audioMode` — keep-alive and control
//! endpoints a sender interleaves. Acknowledged with an empty 200 so the
//! session survives long enough to reach `SETUP`; their bodies carry nothing
//! this receiver acts on.

use crate::http::{Request, Response};

pub fn handle_feedback(request: &Request) -> Response {
    Response::ok(&request.protocol)
}
