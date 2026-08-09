//! `SET_PARAMETER` cover art, forwarded to the host exactly as sent
//! (`image/none` with an empty body is the artwork-cleared statement, which
//! can happen mid-track).

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::session::Session;

#[derive(Debug, Validate)]
pub struct SetArtworkParams {
    /// The image media type as sent, e.g. `image/jpeg` (`image/none`
    /// accompanies a clear).
    pub content_type: String,
    /// The image bytes, exactly as sent; empty means cleared.
    pub data: Vec<u8>,
}

pub fn set_artwork(session: &mut Session, params: SetArtworkParams) -> Result<(), CommandError> {
    params.validate()?;
    debug!(
        "SET_PARAMETER artwork: {}, {} bytes",
        params.content_type,
        params.data.len()
    );
    session.send_session_event(Event::Artwork {
        content_type: params.content_type,
        data: params.data,
    });
    Ok(())
}
