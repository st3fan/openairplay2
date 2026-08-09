//! The business layer: one command per operation on the session.
//!
//! A command owns a `Params` struct deriving [`validator::Validate`] and a
//! function over `(&mut Session, params)` whose first line is
//! `params.validate()?` — validation happens exactly once, here. Commands
//! never parse wire bytes (handlers do that) and never choose status codes
//! (see [`crate::errors`]); they apply validated params to session state and
//! report the resulting [`crate::events::Event`]s to the host.

// SET_PARAMETER / GET_PARAMETER

pub mod get_volume;
pub use get_volume::get_volume;

pub mod set_artwork;
pub use set_artwork::{set_artwork, SetArtworkParams};

pub mod set_metadata;
pub use set_metadata::{set_metadata, SetMetadataParams};

pub mod set_progress;
pub use set_progress::{set_progress, SetProgressParams};

pub mod set_volume;
pub use set_volume::{set_volume, SetVolumeParams};

#[cfg(test)]
pub mod test_helpers;
