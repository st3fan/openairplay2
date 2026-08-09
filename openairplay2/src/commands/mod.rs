//! The business layer: one command per operation on the session.
//!
//! A command owns a `Params` struct deriving [`validator::Validate`] and a
//! function over `(&mut Session, params)` whose first line is
//! `params.validate()?` — validation happens exactly once, here. Commands
//! never parse wire bytes (handlers do that) and never choose status codes
//! (see [`crate::errors`]); they apply validated params to session state and
//! report the resulting [`crate::events::Event`]s to the host.

// SETUP

pub mod setup_streams;
pub use setup_streams::{setup_streams, SetupStreamsParams};

pub mod setup_timing;
pub use setup_timing::{setup_timing, SetupTimingParams};

// Transport control

pub mod flush_buffered;
pub use flush_buffered::{flush_buffered, FlushBufferedParams};

pub mod set_rate_anchor;
pub use set_rate_anchor::{set_rate_anchor, SetRateAnchorParams};

pub mod teardown;
pub use teardown::teardown;

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
