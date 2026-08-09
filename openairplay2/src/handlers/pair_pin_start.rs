//! `POST /pair-pin-start` — a sender that sees status-flag bit 7 (password
//! required) asks the receiver to make the code available before
//! `pair-setup`. Answer an empty 200 (shairport does the same); the actual
//! code is the configured password, which the user types on the sender, and
//! it becomes the SRP password in the `pair-setup` that follows.

use log::debug;

use crate::http::{Request, Response};

pub fn handle_pair_pin_start(request: &Request) -> Response {
    debug!("pair-pin-start: asking the sender for the password");
    Response::ok(&request.protocol)
}
