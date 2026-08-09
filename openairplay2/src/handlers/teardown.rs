//! `TEARDOWN` — the sender is done with the stream.

use crate::commands::teardown;
use crate::http::{Request, Response};
use crate::session::Session;

pub fn handle_teardown(request: &Request, session: &mut Session) -> Response {
    teardown(session);
    Response::ok(&request.protocol)
}
