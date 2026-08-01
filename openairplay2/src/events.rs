//! Session events delivered to the host.
//!
//! Sent over an unbounded `tokio::sync::mpsc` channel so the host consumes
//! them at its own pace; a dropped receiver is tolerated (events are then
//! discarded). Wire concepts (plists, sequence numbers, `shk`) never appear
//! here.

/// What the host needs to know about the streaming session. Transport
/// control (pause, seek) is already handled inside the library — those
/// variants are informational.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `SETUP` phase 2 completed; a sink is about to be created and used.
    SessionStarted { rate: u32, channels: u8 },
    /// `SET_PARAMETER volume`, in AirPlay dB (0 = full, −144 = mute). The
    /// library does not apply gain — the host maps this onto its own volume
    /// model.
    Volume { db: f32 },
    /// `SETRATEANCHORTIME` rate gate engaged (`true`) or released. The
    /// library already gates audio delivery itself.
    Paused(bool),
    /// `FLUSHBUFFERED` (seek/skip). The library already dropped its queue
    /// and called [`crate::sink::AudioSink::flush`].
    Flushed,
    /// `TEARDOWN`, or the control connection closed.
    SessionEnded,
}

/// The sending half handed to the library; the host keeps the receiver.
pub type EventSender = tokio::sync::mpsc::UnboundedSender<Event>;
