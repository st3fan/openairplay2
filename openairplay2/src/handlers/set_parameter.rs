//! `SET_PARAMETER` — dispatched on its `Content-Type`: DMAP track metadata,
//! cover art, or (the default) `text/parameters` lines.
//!
//! **Tolerance policy: always 200.** Everything this verb carries is
//! decoration — an unparseable DMAP blob is dropped with a debug log, an
//! unusable `volume:` leaves the knob where it was, a malformed `progress:`
//! is ignored — and within `text/parameters` tolerance is **per line**,
//! because real senders combine `volume:` and `progress:` in one body and a
//! bad line must not stop the others. All hardware-verified behavior; an
//! error here must never reach the sender.

use log::debug;

use crate::commands::{
    set_artwork, set_metadata, set_progress, set_volume, SetArtworkParams, SetMetadataParams,
    SetProgressParams, SetVolumeParams,
};
use crate::dmap;
use crate::errors::CommandError;
use crate::http::{Request, Response};
use crate::session::Session;
use crate::types::VolumeDb;

/// `SET_PARAMETER` content type carrying DMAP track metadata.
const DMAP_CONTENT_TYPE: &str = "application/x-dmap-tagged";

pub fn handle_set_parameter(request: &Request, session: &mut Session) -> Response {
    if let Err(e) = apply(request, session) {
        debug!("SET_PARAMETER ignored: {e}");
    }
    Response::ok(&request.protocol)
}

fn apply(request: &Request, session: &mut Session) -> Result<(), CommandError> {
    let content_type = request.headers.get("Content-Type");
    // Strip any parameters ("; charset=...") from the media type.
    let media_type = content_type.map(|ct| ct.split(';').next().unwrap_or(ct).trim());
    match media_type {
        Some(ct) if ct.eq_ignore_ascii_case(DMAP_CONTENT_TYPE) => {
            let meta = dmap::parse(&request.body).ok_or_else(|| {
                CommandError::MalformedBody(
                    "DMAP metadata",
                    format!("unrecognized payload ({} bytes)", request.body.len()),
                )
            })?;
            set_metadata(
                session,
                SetMetadataParams {
                    title: meta.title,
                    artist: meta.artist,
                    album: meta.album,
                },
            )
        }
        Some(ct)
            if ct
                .get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case("image/")) =>
        {
            set_artwork(
                session,
                SetArtworkParams {
                    content_type: ct.to_string(),
                    data: request.body.clone(),
                },
            )
        }
        _ => {
            text_parameters(session, &request.body);
            Ok(())
        }
    }
}

/// The `text/parameters` flavor: the volume line and the sender's position
/// report, each line tolerated (or not) on its own.
fn text_parameters(session: &mut Session, body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("volume:") {
            match v.trim().parse::<f32>().ok().and_then(VolumeDb::sanitize) {
                Some(db) => {
                    if let Err(e) = set_volume(session, SetVolumeParams { db }) {
                        debug!("SET_PARAMETER volume ignored: {e}");
                    }
                }
                // Unparseable or non-finite: the knob does not move.
                None => debug!("SET_PARAMETER unusable volume {:?}", v.trim()),
            }
        } else if let Some(v) = line.strip_prefix("progress:") {
            match parse_progress(v.trim()) {
                Some(params) => {
                    if let Err(e) = set_progress(session, params) {
                        debug!("SET_PARAMETER progress ignored: {e}");
                    }
                }
                None => debug!("SET_PARAMETER progress: unparseable value {:?}", v.trim()),
            }
        }
    }
}

/// `progress: <start>/<current>/<end>` — three RTP timestamps.
fn parse_progress(value: &str) -> Option<SetProgressParams> {
    let mut parts = value.split('/').map(|p| p.trim().parse::<u32>());
    let (Some(Ok(start)), Some(Ok(current)), Some(Ok(end)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    Some(SetProgressParams {
        start,
        current,
        end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::{dmap_track, request, session, start_stream};
    use crate::events::Event;
    use crate::player::Track;
    use std::time::Duration;

    /// Drive the handler the way dispatch would, with the given content type.
    fn set_parameter(session: &mut Session, content_type: Option<&str>, body: &[u8]) -> Response {
        let headers: Vec<(&str, &str)> = content_type
            .map(|ct| ("Content-Type", ct))
            .into_iter()
            .collect();
        let request = request("SET_PARAMETER", "rtsp://x", &headers, body);
        handle_set_parameter(&request, session)
    }

    #[test]
    fn volume_reaches_the_host_and_the_session() {
        let (mut session, mut events) = session();
        let response = set_parameter(&mut session, Some("text/parameters"), b"volume: -12.5\r\n");
        assert_eq!(response.status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
        assert_eq!(session.volume, -12.5);
    }

    #[test]
    fn non_finite_volume_is_refused() {
        let (mut session, mut events) = session();
        set_parameter(&mut session, Some("text/parameters"), b"volume: -12.5\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
        // `f32::parse` takes all of these; the knob must not move for any of
        // them, least of all to full scale.
        for value in ["nan", "NaN", "inf", "-inf", "1e40", "-1e40", "banana", ""] {
            let response = set_parameter(
                &mut session,
                Some("text/parameters"),
                format!("volume: {value}\r\n").as_bytes(),
            );
            // The tolerance policy: garbage is still a 200.
            assert_eq!(response.status(), 200);
            assert!(
                events.try_recv().is_err(),
                "volume: {value:?} emitted an event"
            );
            assert_eq!(
                session.volume, -12.5,
                "volume: {value:?} changed the current volume"
            );
        }
    }

    #[test]
    fn out_of_range_volume_is_clamped() {
        let (mut session, mut events) = session();
        // Above full scale, and far below the mute sentinel.
        for (sent, expected) in [("6.0", 0.0), ("-500", -144.0), ("-144", -144.0)] {
            set_parameter(
                &mut session,
                Some("text/parameters"),
                format!("volume: {sent}\r\n").as_bytes(),
            );
            assert_eq!(events.try_recv(), Ok(Event::Volume { db: expected }));
        }
        // The session records the clamped value, not what was sent.
        assert_eq!(session.volume, -144.0);
    }

    #[tokio::test]
    async fn metadata_and_artwork_reach_the_host_mid_session() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));

        set_parameter(&mut session, Some(DMAP_CONTENT_TYPE), &dmap_track("Song"));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Metadata {
                title: Some("Song".into()),
                artist: Some("Artist".into()),
                album: Some("Album".into()),
            })
        );

        set_parameter(&mut session, Some("image/png"), b"\x89PNG");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/png".into(),
                data: b"\x89PNG".to_vec(),
            })
        );

        // `image/none` with an empty body is the artwork-cleared statement,
        // forwarded rather than suppressed (it can happen mid-track).
        set_parameter(&mut session, Some("image/none"), b"");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/none".into(),
                data: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn early_metadata_is_latched_until_session_start() {
        let (mut session, mut events) = session();
        // Pushed during the handshake, before SETUP phase 2: nothing yet...
        set_parameter(&mut session, Some(DMAP_CONTENT_TYPE), &dmap_track("First"));
        set_parameter(&mut session, Some(DMAP_CONTENT_TYPE), &dmap_track("Second"));
        set_parameter(&mut session, Some("image/jpeg"), b"JPEG");
        assert!(events.try_recv().is_err());

        // ...and the latest of each replays right after SessionStarted.
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Metadata { title: Some(t), .. }) if t == "Second"
        ));
        assert!(matches!(events.try_recv(), Ok(Event::Artwork { .. })));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_metadata_never_reaches_the_host() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));

        for body in [
            &b""[..],
            b"garbage, not dmap",
            // Truncated: an mlit that claims more payload than exists.
            b"mlit\x00\x00\xff\xff",
        ] {
            let response = set_parameter(&mut session, Some(DMAP_CONTENT_TYPE), body);
            assert_eq!(
                response.status(),
                200,
                "metadata is decoration: still a 200"
            );
        }
        assert!(events.try_recv().is_err());

        // The session itself is unharmed — the volume path still works.
        set_parameter(&mut session, Some("text/parameters"), b"volume: -6.0\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));
    }

    #[tokio::test]
    async fn progress_anchors_the_track_and_reports_the_senders_position() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {} // drain SessionStarted

        // A minute-long track, the sender five seconds in.
        let start = 1_000u32;
        let end = start + 44_100 * 60;
        let current = start + 44_100 * 5;
        set_parameter(
            &mut session,
            Some("text/parameters"),
            format!("progress: {start}/{current}/{end}\r\n").as_bytes(),
        );

        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::from_secs(5),
                duration: Duration::from_secs(60),
            })
        );
        // The extent is what the playback thread needs: it turns the audio it
        // plays into a running position without another word from the sender.
        assert_eq!(
            *session.track.lock().unwrap(),
            Some(Track { start, end }),
            "the track extent must reach the playback thread"
        );
    }

    #[tokio::test]
    async fn progress_before_a_stream_anchors_but_reports_nothing() {
        let (mut session, mut events) = session();
        set_parameter(
            &mut session,
            Some("text/parameters"),
            b"progress: 0/44100/441000\r\n",
        );
        assert!(
            events.try_recv().is_err(),
            "a position without a stream means nothing"
        );
        assert!(session.track.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn malformed_progress_is_ignored() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        for body in [
            &b"progress: 1000/2000\r\n"[..],      // too few fields
            b"progress: 1000/2000/3000/4000\r\n", // too many
            b"progress: a/b/c\r\n",               // not numbers
            b"progress:\r\n",                     // empty
        ] {
            set_parameter(&mut session, Some("text/parameters"), body);
            assert!(
                events.try_recv().is_err(),
                "unparseable progress must not report: {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[tokio::test]
    async fn progress_after_a_seek_clamps_instead_of_reporting_27_hours() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        // `current` before `start`: the subtraction would wrap.
        set_parameter(
            &mut session,
            Some("text/parameters"),
            b"progress: 44100/0/441000\r\n",
        );
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::ZERO,
                duration: Duration::from_secs(9),
            })
        );
    }

    #[tokio::test]
    async fn volume_and_progress_travel_in_one_body() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        set_parameter(
            &mut session,
            Some("text/parameters"),
            b"volume: -6.0\r\nprogress: 0/44100/441000\r\n",
        );
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::from_secs(1),
                duration: Duration::from_secs(10),
            })
        );
    }
}
