//! `SETRATEANCHORTIME` — parse the sender's play/pause plist and hand the
//! rate to the command. The network-time fields matter only with a PTP
//! clock, so they are ignored (see notes/milestone-6.md).
//!
//! **Tolerance policy: always 200.** An unparseable body is logged and
//! ignored; the sender has already buffered ahead, and an error would only
//! make it abandon a session that is otherwise fine.

use std::io;

use log::{debug, warn};
use plist::Value;

use crate::commands::{set_rate_anchor, SetRateAnchorParams};
use crate::http::{Request, Response};
use crate::session::Session;

use super::plist::int_field;

pub fn handle_set_rate_anchor(request: &Request, session: &mut Session) -> Response {
    match parse_rate_anchor(&request.body) {
        Some(params) => {
            if let Err(e) = set_rate_anchor(session, params) {
                debug!("SETRATEANCHORTIME ignored: {e}");
            }
        }
        None => warn!("SETRATEANCHORTIME: could not parse body"),
    }
    Response::ok(&request.protocol)
}

/// Parse a `SETRATEANCHORTIME` plist. `rate` is required (0 = pause,
/// 1 = play); `rtpTime` is the anchor timestamp, defaulting to 0.
fn parse_rate_anchor(body: &[u8]) -> Option<SetRateAnchorParams> {
    let value = Value::from_reader(io::Cursor::new(body)).ok()?;
    let dict = value.as_dictionary()?;
    let rate = dict.get("rate").and_then(int_field)?;
    let rtp_time = dict.get("rtpTime").and_then(int_field).unwrap_or(0);
    Some(SetRateAnchorParams { rate, rtp_time })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::plist_bytes;
    use plist::Dictionary;

    #[test]
    fn parses_real_setrateanchortime() {
        // The exact fields a real Mac sent (milestone-5 capture).
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(1u64.into()));
        dict.insert(
            "networkTimeTimelineID".into(),
            Value::Integer((-2116301217048756216i64).into()),
        );
        dict.insert("networkTimeSecs".into(), Value::Integer(1323152u64.into()));
        dict.insert(
            "networkTimeFrac".into(),
            Value::Integer(6275326383463858176u64.into()),
        );
        dict.insert("networkTimeFlags".into(), Value::Integer(0u64.into()));
        dict.insert("rtpTime".into(), Value::Integer(3174381381u64.into()));
        let body = plist_bytes(&dict);

        let params = parse_rate_anchor(&body).unwrap();
        assert_eq!(params.rate, 1);
        assert_eq!(params.rtp_time, 3174381381);
    }

    #[test]
    fn rate_anchor_pause_and_missing_fields() {
        // rate 0 = pause; rtpTime defaults to 0 when absent.
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(0u64.into()));
        let params = parse_rate_anchor(&plist_bytes(&dict)).unwrap();
        assert_eq!(params.rate, 0);
        assert_eq!(params.rtp_time, 0);

        // No rate field at all → None.
        assert!(parse_rate_anchor(&plist_bytes(&Dictionary::new())).is_none());
    }
}
