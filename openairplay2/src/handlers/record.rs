//! `RECORD`, `SETPEERS`, `SETPEERSX` — session control verbs whose bodies
//! carry nothing this receiver acts on (peer lists matter to a PTP clock,
//! which this receiver deliberately does not run). Acknowledged so the
//! sender proceeds.

use log::debug;

use crate::http::{Request, Response};

pub fn handle_record(request: &Request) -> Response {
    debug!("ack {}", request.method);
    Response::ok(&request.protocol)
}
