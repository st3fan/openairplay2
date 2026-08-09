//! `SETUP` — arrives in two phases on one verb, distinguished by the
//! presence of a `streams` array: phase 1 binds the event channel, phase 2
//! the audio channels. The handler parses the plist and picks the phase;
//! each phase is its own command.
//!
//! A body that does not parse answers 400 — unlike the parameter verbs,
//! `SETUP` is load-bearing: a sender that sent garbage here cannot stream,
//! and the error is the only useful answer.

use std::io;

use log::warn;
use plist::{Dictionary, Value};

use crate::commands::{setup_streams, setup_timing, SetupStreamsParams, SetupTimingParams};
use crate::errors::CommandError;
use crate::http::{Request, Response};
use crate::session::Session;

use super::INFO_CONTENT_TYPE;

pub async fn handle_setup(request: &Request, session: &mut Session) -> Response {
    match apply(request, session).await {
        Ok(body) => Response::ok(&request.protocol).body(INFO_CONTENT_TYPE, body),
        Err(e) => {
            warn!("SETUP failed: {e}");
            e.response(&request.protocol)
        }
    }
}

async fn apply(request: &Request, session: &mut Session) -> Result<Vec<u8>, CommandError> {
    let value = Value::from_reader(io::Cursor::new(&request.body))
        .map_err(|e| CommandError::MalformedBody("SETUP", format!("plist: {e}")))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| CommandError::MalformedBody("SETUP", "body not a dict".to_string()))?;

    if let Some(streams) = dict.get("streams").and_then(|v| v.as_array()) {
        let params = parse_streams(streams)?;
        setup_streams(session, params).await
    } else {
        setup_timing(session, parse_timing(dict)).await
    }
}

/// Phase 2: the first stream's fields, exactly as `SETUP` names them.
fn parse_streams(streams: &[Value]) -> Result<SetupStreamsParams, CommandError> {
    let stream = streams
        .first()
        .and_then(|v| v.as_dictionary())
        .ok_or_else(|| CommandError::MalformedBody("SETUP", "empty streams array".to_string()))?;
    Ok(SetupStreamsParams {
        stream_type: stream.get("type").and_then(|v| v.as_unsigned_integer()),
        audio_format: stream
            .get("audioFormat")
            .and_then(|v| v.as_unsigned_integer()),
        shared_key: stream
            .get("shk")
            .and_then(|v| v.as_data())
            .map(<[u8]>::to_vec),
        spf: stream.get("spf").and_then(|v| v.as_unsigned_integer()),
    })
}

/// Phase 1: only the timing protocol, and only for the log.
fn parse_timing(dict: &Dictionary) -> SetupTimingParams {
    SetupTimingParams {
        timing_protocol: dict
            .get("timingProtocol")
            .and_then(|v| v.as_string())
            .unwrap_or("(none)")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::{peer, plist_bytes, request, session};
    use crate::events::Event;
    use crate::session::TYPE_BUFFERED;

    async fn setup(session: &mut Session, dict: &Dictionary) -> Response {
        let request = request("SETUP", "rtsp://x", &[], &plist_bytes(dict));
        handle_setup(&request, session).await
    }

    #[tokio::test]
    async fn phase1_response_has_event_and_timing_ports() {
        let (mut session, _events) = session();
        // Minimal phase-1 plist: timingProtocol=PTP, no streams.
        let mut dict = Dictionary::new();
        dict.insert("timingProtocol".into(), Value::String("PTP".into()));

        let response = setup(&mut session, &dict).await;
        assert_eq!(response.status(), 200);
        let value = Value::from_reader(io::Cursor::new(response.into_body())).unwrap();
        let d = value.as_dictionary().unwrap();
        assert!(d.get("eventPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert_eq!(d.get("timingPort").unwrap().as_unsigned_integer(), Some(0));
        assert!(d.get("timingPeerInfo").unwrap().as_dictionary().is_some());
    }

    #[tokio::test]
    async fn phase2_response_binds_ports_and_echoes_type() {
        let (mut session, mut events) = session();
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(TYPE_BUFFERED.into()));
        stream.insert("audioFormat".into(), Value::Integer(0x400000u64.into()));
        stream.insert("shk".into(), Value::Data(vec![7u8; 32]));
        let mut dict = Dictionary::new();
        dict.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );

        let response = setup(&mut session, &dict).await;
        assert_eq!(response.status(), 200);
        let value = Value::from_reader(io::Cursor::new(response.into_body())).unwrap();
        let streams = value
            .as_dictionary()
            .unwrap()
            .get("streams")
            .unwrap()
            .as_array()
            .unwrap();
        let s = streams[0].as_dictionary().unwrap();
        assert_eq!(
            s.get("type").unwrap().as_unsigned_integer(),
            Some(TYPE_BUFFERED)
        );
        assert!(s.get("dataPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert!(s.get("controlPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert!(s.get("audioBufferSize").is_some());
        assert_eq!(session.stream_key, Some(vec![7u8; 32]));

        // The host learned the stream started, with the negotiated format.
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                rate: 44100,
                channels: 2,
                peer: peer(),
            })
        );

        // Dropping the session (connection closed) ends it exactly once.
        drop(session);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn phase_detection_uses_streams_presence() {
        // A dict with no streams is phase 1 (event port), with streams is phase 2.
        let (mut s1, _events) = session();
        let response = setup(&mut s1, &Dictionary::new()).await;
        assert!(Value::from_reader(io::Cursor::new(response.into_body()))
            .unwrap()
            .as_dictionary()
            .unwrap()
            .contains_key("eventPort"));
    }

    #[tokio::test]
    async fn malformed_setup_answers_400() {
        let (mut session, _events) = session();
        for body in [&b"not a plist"[..], b""] {
            let request = request("SETUP", "rtsp://x", &[], body);
            assert_eq!(handle_setup(&request, &mut session).await.status(), 400);
        }
        // An empty streams array is phase 2 with nothing to set up.
        let mut dict = Dictionary::new();
        dict.insert("streams".into(), Value::Array(vec![]));
        assert_eq!(setup(&mut session, &dict).await.status(), 400);
    }
}
