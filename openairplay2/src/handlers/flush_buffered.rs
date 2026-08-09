//! `FLUSHBUFFERED` (seek/skip) — parse the sender's boundary and hand it to
//! the command.
//!
//! **Tolerance policy: always 200.** A body without a `flushUntilSeq` (or
//! one that does not parse) is a flush with no boundary — all queued audio
//! is discarded, which is what a boundary-less flush means.

use std::io;

use log::debug;
use plist::Value;

use crate::commands::{flush_buffered, FlushBufferedParams};
use crate::http::{Request, Response};
use crate::session::Session;

use super::plist::int_field;

pub fn handle_flush_buffered(request: &Request, session: &mut Session) -> Response {
    let params = parse_flush_buffered(&request.body);
    if let Err(e) = flush_buffered(session, params) {
        debug!("FLUSHBUFFERED ignored: {e}");
    }
    Response::ok(&request.protocol)
}

/// Parse a `FLUSHBUFFERED` plist for its `flushUntilSeq` boundary (drop all
/// packets with a lower sequence number).
fn parse_flush_buffered(body: &[u8]) -> FlushBufferedParams {
    let until_seq = Value::from_reader(io::Cursor::new(body))
        .ok()
        .and_then(|value| {
            value
                .as_dictionary()
                .and_then(|dict| dict.get("flushUntilSeq"))
                .and_then(int_field)
        });
    FlushBufferedParams { until_seq }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::plist_bytes;
    use plist::Dictionary;

    #[test]
    fn parses_real_flushbuffered() {
        // The exact fields a real Mac sent on skip (log capture).
        let mut dict = Dictionary::new();
        dict.insert("flushUntilSeq".into(), Value::Integer(5179978u64.into()));
        dict.insert("flushUntilTS".into(), Value::Integer(2204469244u64.into()));
        assert_eq!(
            parse_flush_buffered(&plist_bytes(&dict)).until_seq,
            Some(5179978)
        );

        // A body without the field yields None (no boundary set).
        assert_eq!(
            parse_flush_buffered(&plist_bytes(&Dictionary::new())).until_seq,
            None
        );
    }
}
