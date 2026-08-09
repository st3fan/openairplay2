//! An embeddable AirPlay 2 audio **receiver**: a real Mac/iPhone discovers
//! it, pairs with it, and streams to it; the host application gets decoded
//! PCM and session events. The library owns network → PCM (discovery
//! advertisement, transient pairing, the encrypted control channel, the
//! buffered-audio channel, decrypt, AAC decode, and the pause/seek/
//! backpressure semantics); the host owns PCM → speaker via an [`AudioSink`].
//!
//! Deliberate scope: one sender → one stream → one output. No PTP — for a
//! single output, the sender's buffering plus this library's backpressure
//! suffice (blocking [`AudioSink::write`] paces playback). No AirPlay wire
//! concepts (plists, `shk`, sequence numbers) appear in this API.
//!
//! ```no_run
//! use openairplay2::{AudioSink, Event, Receiver};
//!
//! struct MySink;
//!
//! impl AudioSink for MySink {
//!     fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
//!     fn flush(&mut self) { /* seek: drop device/prebuffer state */ }
//! }
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let receiver = Receiver::builder()
//!         .name("Office")
//!         .identity_path("/var/lib/myapp/airplay-identity")
//!         .build()?;
//!     let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
//!     tokio::spawn(async move {
//!         while let Some(event) = rx.recv().await {
//!             if let Event::Volume { db } = event { /* your gain path */ }
//!         }
//!     });
//!     receiver.run(|_rate, _channels| Box::new(MySink), events).await
//! }
//! ```

#![warn(missing_docs)]

mod avahi;
mod buffered;
mod crypto_stream;
mod decode;
mod dmap;
mod events;
mod fairplay;
mod http;
mod identity;
mod info;
mod mac;
mod pairing;
mod player;
mod receiver;
mod session;
mod sink;
mod takeover;

// Sender-side pieces the integration tests (and a future test-sender) drive
// the real server with. Public so the tests can reach them, but not part of
// the documented embedding API.
#[doc(hidden)]
pub mod cipher;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod srp;
#[doc(hidden)]
pub mod tlv;

pub use events::{Event, EventSender};
pub use identity::Identity;
pub use info::txt_records;
pub use receiver::{Receiver, ReceiverBuilder};
pub use sink::{AudioSink, SinkFactory};

/// Receiver-wide configuration, resolved by [`ReceiverBuilder::build`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The receiver name senders see in the AirPlay picker.
    pub name: String,
    /// TCP port of the HTTP/RTSP control server.
    pub port: u16,
    /// The MAC address used as the AirPlay `deviceid`.
    pub mac: [u8; 6],
    /// Apple model string, e.g. `OpenAirPlay2,1`.
    pub model: String,
    /// AirPlay source version, e.g. `366.0`.
    pub source_version: String,
    /// 64-bit AirPlay 2 capability bitmask.
    pub features: u64,
    /// AirPlay status flags.
    pub status_flags: u32,
    /// `Some` → require this password to pair — Apple's own word: iOS and
    /// macOS show a "password" dialog and accept alphanumerics, not just
    /// digits. Advertised as "password required" via status-flag bit 7; a
    /// sender enters it in transient pairing. `None` → the standard
    /// transient code `3939`. Unlike openairplay1 there is no "open" mode —
    /// AirPlay 2 always pairs. Never logged.
    pub password: Option<String>,
}

impl Config {
    /// MAC as colon-separated uppercase hex, e.g. `AA:BB:CC:DD:EE:FF`. This is
    /// the AirPlay `deviceid`.
    pub fn device_id(&self) -> String {
        self.mac
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The `features` bitmask split into `(low32, high32)`, the form used in
    /// both the mDNS TXT record and `GET /info`.
    pub fn features_split(&self) -> (u32, u32) {
        (self.features as u32, (self.features >> 32) as u32)
    }
}
