//! `SET_PARAMETER progress:` — three RTP timestamps naming the current
//! track's extent and the sender's idea of the position.
//!
//! The extent is what matters: it is handed to the playback thread, which
//! turns the audio it plays into a running position. The sender's own
//! `current` is reported once here (it is right at track start, which is
//! essentially the only time this line arrives) and never extrapolated from.

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::player::{frames_to_duration, Track};
use crate::session::{aac_params, Session};

#[derive(Debug, Validate)]
pub struct SetProgressParams {
    /// RTP timestamp of the track's start.
    pub start: u32,
    /// The sender's idea of the current position.
    pub current: u32,
    /// RTP timestamp of the track's end.
    pub end: u32,
}

pub fn set_progress(session: &mut Session, params: SetProgressParams) -> Result<(), CommandError> {
    params.validate()?;
    *session.track.lock().unwrap() = Some(Track {
        start: params.start,
        end: params.end,
    });
    if !session.session_active {
        return Ok(()); // a position without a stream means nothing
    }
    let (rate, _) = aac_params(session.audio_format);
    // A seek can put `current` before `start`, and the timestamps wrap;
    // saturating subtraction keeps both readings sane rather than
    // reporting a position of ~27 hours.
    let elapsed = frames_to_duration(params.current.saturating_sub(params.start), rate);
    let duration = frames_to_duration(params.end.saturating_sub(params.start), rate);
    debug!(
        "SET_PARAMETER progress {:.1}s / {:.1}s",
        elapsed.as_secs_f32(),
        duration.as_secs_f32()
    );
    session.send_event(Event::Progress { elapsed, duration });
    Ok(())
}
