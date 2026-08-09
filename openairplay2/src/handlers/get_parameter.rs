//! `GET_PARAMETER` — a sender asks `volume\r\n` during setup and expects a
//! `text/parameters` answer of `volume: <dB>\r\n` back.
//!
//! **Tolerance policy: always 200.** An empty answer to the volume query
//! makes a real sender abort before `SETUP` phase 2, and an unknown
//! parameter gets a well-formed empty body rather than an error.

use log::debug;

use crate::commands::get_volume;
use crate::http::{Request, Response};
use crate::session::Session;

use super::PARAMETERS_CONTENT_TYPE;

pub fn handle_get_parameter(request: &Request, session: &Session) -> Response {
    let query = String::from_utf8_lossy(&request.body);
    let body = if query.trim() == "volume" {
        get_volume(session).into_bytes()
    } else {
        debug!("GET_PARAMETER for unknown parameter: {query:?}");
        Vec::new()
    };
    Response::ok(&request.protocol).body(PARAMETERS_CONTENT_TYPE, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::{request, session};

    fn get_parameter(session: &Session, query: &[u8]) -> Vec<u8> {
        let request = request(
            "GET_PARAMETER",
            "rtsp://x",
            &[("Content-Type", PARAMETERS_CONTENT_TYPE)],
            query,
        );
        // The response body is what the sender parses; the status is always
        // 200 (see the module's tolerance policy).
        let response = handle_get_parameter(&request, session);
        assert_eq!(response.status(), 200);
        response.into_body()
    }

    #[test]
    fn volume_query_returns_current_volume() {
        let (mut session, mut events) = session();
        // A sender's exact query is "volume\r\n".
        assert_eq!(
            get_parameter(&session, b"volume\r\n"),
            b"volume: 0.000000\r\n"
        );
        session.volume = -12.5;
        assert_eq!(
            get_parameter(&session, b"volume\r\n"),
            b"volume: -12.500000\r\n"
        );
        assert!(events.try_recv().is_err(), "a query emits no events");
    }

    #[test]
    fn unknown_parameters_yield_an_empty_body_rather_than_a_bad_one() {
        let (session, _events) = session();
        assert!(get_parameter(&session, b"progress\r\n").is_empty());
    }
}
