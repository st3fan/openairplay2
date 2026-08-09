//! `SET_PARAMETER` DMAP track metadata — a complete statement about the
//! current track, not a delta: fields the sender did not carry are `None`
//! and replace the previous value.

use log::debug;
use validator::Validate;

use crate::errors::CommandError;
use crate::events::Event;
use crate::session::Session;

#[derive(Debug, Validate)]
pub struct SetMetadataParams {
    /// Track title (DMAP `minm`).
    pub title: Option<String>,
    /// Track artist (DAAP `asar`).
    pub artist: Option<String>,
    /// Track album (DAAP `asal`).
    pub album: Option<String>,
}

pub fn set_metadata(session: &mut Session, params: SetMetadataParams) -> Result<(), CommandError> {
    params.validate()?;
    debug!(
        "SET_PARAMETER metadata: title={:?} artist={:?} album={:?}",
        params.title, params.artist, params.album
    );
    session.send_session_event(Event::Metadata {
        title: params.title,
        artist: params.artist,
        album: params.album,
    });
    Ok(())
}
