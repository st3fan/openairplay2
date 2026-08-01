//! The seam between the AirPlay session and the host's audio output.
//!
//! The library owns network → PCM (pairing, decrypt, decode, the queue and
//! its pause/flush gating); the host owns PCM → speaker. A host provides an
//! [`AudioSink`] and the library's playback thread feeds it interleaved
//! `i16` samples that should actually play — audio is withheld while paused
//! and pre-seek audio is dropped before it ever reaches the sink.

use std::sync::Arc;

/// Where decoded audio goes. Implemented by the host (the receiver binary's
/// sink is ALSA), called from a dedicated library-managed playback thread.
pub trait AudioSink: Send + 'static {
    /// Play interleaved `i16` PCM. `write` may block — blocking is the pacing
    /// mechanism: the sink drains at the hardware's rate, the playback thread
    /// waits on it, and the TCP reader backpressures on the queue behind it.
    fn write(&mut self, pcm: &[i16]);

    /// Seek/skip (or pause): immediately drop anything the sink has queued or
    /// buffered of its own (hardware buffers, prebuffer cushions). The
    /// library has already dropped its queued PCM when this is called.
    fn flush(&mut self);
}

/// Creates the sink for a stream, invoked at `SETUP` phase 2 with the
/// negotiated sample rate and channel count — once per stream.
pub type SinkFactory = Arc<dyn Fn(u32, u8) -> Box<dyn AudioSink> + Send + Sync>;
