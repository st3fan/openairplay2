//! `GET /info` — the receiver's capability plist, answered from the static
//! configuration and identity.

use crate::http::{Request, Response};
use crate::info::info_plist;
use crate::server::Context;

use super::INFO_CONTENT_TYPE;

pub fn handle_info(request: &Request, context: &Context) -> Response {
    Response::ok(&request.protocol).body(
        INFO_CONTENT_TYPE,
        info_plist(&context.config, &context.identity),
    )
}
