//! `GET_PARAMETER volume` — answer the sender's query with the current
//! volume. The value is well-formed by construction (see
//! [`crate::types::VolumeDb`]); a malformed answer makes a real sender abort
//! before `SETUP` phase 2. No params: the query carries no values.

use crate::session::Session;

pub fn get_volume(session: &Session) -> String {
    format!("volume: {:.6}\r\n", session.volume)
}
