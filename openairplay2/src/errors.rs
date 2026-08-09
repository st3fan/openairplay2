//! The error a command or handler propagates to the web layer, and the one
//! place in the crate where an error chooses a status code.
//!
//! Not every error a command returns reaches the sender: several AirPlay
//! verbs are hardware-verified to answer 200 OK to garbage (metadata is
//! decoration; a malformed `GET_PARAMETER` answer makes a real sender abort
//! before SETUP phase 2). Those handlers catch the error, log it, and answer
//! 200 themselves — the *tolerance policy* lives visibly in the handler,
//! while the status mapping for errors that *are* sender-visible lives here.

use std::io;

use thiserror::Error;

use crate::http::Response;

/// What went wrong while turning a request into an effect on the session.
#[derive(Debug, Error)]
pub enum CommandError {
    /// A request body that could not be parsed into a command's params:
    /// which body, and what was wrong with it.
    #[error("malformed {0} body: {1}")]
    MalformedBody(&'static str, String),
    /// A params struct that failed its command's validation.
    #[error("validation: {0}")]
    Validation(#[from] validator::ValidationErrors),
    /// The command could not act — binding a channel socket failed. Not the
    /// sender's fault, and the status says so.
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
}

impl CommandError {
    /// Map this error to a wire response — the only place in the crate that
    /// chooses an error status code. `protocol` is a parameter because every
    /// response must echo the request's own protocol token (`HTTP/1.1` vs
    /// `RTSP/1.0`); see [`crate::http::Response::new`].
    pub fn response(&self, protocol: &str) -> Response {
        let (status, reason) = match self {
            CommandError::MalformedBody(..) | CommandError::Validation(_) => (400, "Bad Request"),
            CommandError::Io(_) => (500, "Internal Server Error"),
        };
        Response::new(protocol, status, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        // One table, one place: every variant and the status it answers.
        let cases = [
            (
                CommandError::MalformedBody("fp-setup", "not FairPlay".into()),
                400,
            ),
            (
                CommandError::Validation(validator::ValidationErrors::new()),
                400,
            ),
            (CommandError::Io(io::Error::other("bind failed")), 500),
        ];
        for (error, status) in cases {
            assert_eq!(error.response("RTSP/1.0").status(), status, "{error}");
        }
    }

    #[test]
    fn message_names_the_body_and_the_reason() {
        let error = CommandError::MalformedBody("fp-setup", "unrecognized header".into());
        assert_eq!(
            error.to_string(),
            "malformed fp-setup body: unrecognized header"
        );
    }
}
