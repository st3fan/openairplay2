//! `SETRATEANCHORTIME` — the sender's play/pause rate. `rate=0` engages the
//! pause gate — the player *holds* queued and arriving audio (a flush-less
//! pause gives no licence to drop anything; the sender expects it all to
//! still be buffered at resume) — and `rate=1` releases it, playing the held
//! audio from where playback stopped. Bypasses the audio queue: an in-band
//! command would sit behind the ~2 s buffer.

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::session::Session;

#[derive(Debug, Validate)]
pub struct SetRateAnchorParams {
    /// `0` = pause, `1` = play (a real sender sends nothing else).
    pub rate: u64,
    /// The RTP anchor timestamp — logged only; it matters with a PTP clock,
    /// which this receiver deliberately does not run.
    pub rtp_time: u64,
}

pub fn set_rate_anchor(
    session: &mut Session,
    params: SetRateAnchorParams,
) -> Result<(), CommandError> {
    params.validate()?;
    debug!(
        "SETRATEANCHORTIME rate={} rtpTime={}",
        params.rate, params.rtp_time
    );
    let paused = params.rate == 0;
    if let Some(ctrl) = &session.player_control {
        ctrl.set_paused(paused);
    }
    session.send_event(Event::Paused(paused));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::session;

    #[test]
    fn rate_zero_pauses_and_rate_one_resumes() {
        let (mut session, mut events) = session();
        set_rate_anchor(
            &mut session,
            SetRateAnchorParams {
                rate: 0,
                rtp_time: 0,
            },
        )
        .unwrap();
        assert_eq!(events.try_recv(), Ok(Event::Paused(true)));
        set_rate_anchor(
            &mut session,
            SetRateAnchorParams {
                rate: 1,
                rtp_time: 12345,
            },
        )
        .unwrap();
        assert_eq!(events.try_recv(), Ok(Event::Paused(false)));
    }
}
