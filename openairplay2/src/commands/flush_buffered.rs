//! `FLUSHBUFFERED` (seek/skip) — discard exactly the audio the sender names:
//! queued/held packets with a sequence stamp below the boundary, plus the
//! stale audio still arriving over TCP (the sender buffers far ahead),
//! while retaining everything at or after it. Applied out-of-band — to the
//! queue by sequence stamp, and pre-decrypt to arriving packets by their
//! plaintext sequence number — because an in-band command would sit behind
//! the ~2 s buffer.

use std::sync::atomic::Ordering;

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::session::Session;

#[derive(Debug, Validate)]
pub struct FlushBufferedParams {
    /// Discard packets with a sequence number below this, retaining the
    /// rest; `None` (a body without a boundary) discards all queued audio.
    pub until_seq: Option<u64>,
}

pub fn flush_buffered(
    session: &mut Session,
    params: FlushBufferedParams,
) -> Result<(), CommandError> {
    params.validate()?;
    match params.until_seq {
        Some(seq) => {
            session.flush_until_seq.store(seq, Ordering::Relaxed);
            debug!("FLUSHBUFFERED until seq {seq}");
        }
        None => debug!("FLUSHBUFFERED (no seq boundary)"),
    }
    if let Some(ctrl) = &session.player_control {
        ctrl.flush(params.until_seq);
    }
    session.send_event(Event::Flushed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::session;

    #[test]
    fn boundary_reaches_the_reader_and_the_host_learns_of_the_flush() {
        let (mut session, mut events) = session();
        flush_buffered(
            &mut session,
            FlushBufferedParams {
                until_seq: Some(5_179_978),
            },
        )
        .unwrap();
        assert_eq!(
            session.flush_until_seq.load(Ordering::Relaxed),
            5_179_978,
            "the reader drops arriving packets below this"
        );
        assert_eq!(events.try_recv(), Ok(Event::Flushed));
    }

    #[test]
    fn a_flush_without_a_boundary_sets_none() {
        let (mut session, mut events) = session();
        flush_buffered(&mut session, FlushBufferedParams { until_seq: None }).unwrap();
        assert_eq!(session.flush_until_seq.load(Ordering::Relaxed), 0);
        assert_eq!(events.try_recv(), Ok(Event::Flushed));
    }
}
