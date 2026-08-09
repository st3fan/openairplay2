//! `POST /fp-setup` — the canned FairPlay handshake (see
//! [`crate::fairplay`]; no live crypto).

use log::warn;

use crate::errors::CommandError;
use crate::fairplay;
use crate::http::{Request, Response};

use super::PAIRING_CONTENT_TYPE;

pub fn handle_fp_setup(request: &Request) -> Response {
    match fairplay::fp_setup(&request.body) {
        Some(reply) => Response::ok(&request.protocol).body(PAIRING_CONTENT_TYPE, reply),
        None => {
            let error = CommandError::MalformedBody(
                "fp-setup",
                "unrecognized FairPlay request".to_string(),
            );
            warn!("{error}");
            error.response(&request.protocol)
        }
    }
}
