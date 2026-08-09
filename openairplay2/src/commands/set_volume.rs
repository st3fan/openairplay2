//! `SET_PARAMETER volume:` — record the sender's volume (to answer
//! `GET_PARAMETER volume`) and report it to the host, which owns the gain
//! path; the library never applies volume itself.

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::session::Session;
use crate::types::VolumeDb;

#[derive(Debug, Validate)]
pub struct SetVolumeParams {
    /// The sender's volume: 0 dB = full scale, −30 ≈ minimum, −144 = mute.
    #[validate(nested)]
    pub db: VolumeDb,
}

pub fn set_volume(session: &mut Session, params: SetVolumeParams) -> Result<(), CommandError> {
    params.validate()?;
    let db = params.db.get();
    session.volume = db;
    debug!("SET_PARAMETER volume {db} dB");
    session.send_event(Event::Volume { db });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::session;

    #[test]
    fn records_and_reports_the_volume() {
        let (mut session, mut events) = session();
        let params = SetVolumeParams {
            db: VolumeDb::sanitize(-12.5).unwrap(),
        };
        set_volume(&mut session, params).unwrap();
        assert_eq!(session.volume, -12.5);
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
    }

    #[test]
    fn validation_refuses_what_sanitization_would_have() {
        // A params struct built without sanitization does not move the knob.
        let (mut session, mut events) = session();
        let params = SetVolumeParams {
            db: VolumeDb::new_unchecked(f32::NAN),
        };
        let result = set_volume(&mut session, params);
        assert!(matches!(result, Err(CommandError::Validation(_))));
        assert_eq!(session.volume, 0.0, "the knob must not move");
        assert!(events.try_recv().is_err(), "no event for a refused volume");
    }
}
