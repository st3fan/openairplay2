pub mod alsa_sink;
pub mod avahi;
pub mod buffered;
pub mod cipher;
pub mod crypto_stream;
pub mod decode;
pub mod events;
pub mod fairplay;
pub mod http;
pub mod identity;
pub mod info;
pub mod mac;
pub mod pairing;
pub mod player;
pub mod server;
pub mod session;
pub mod sink;
pub mod srp;
pub mod tlv;

pub use events::{Event, EventSender};
pub use sink::{AudioSink, SinkFactory};

/// Receiver-wide configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub port: u16,
    pub mac: [u8; 6],
    /// Apple model string, e.g. `OpenAirPlay2,1`.
    pub model: String,
    /// AirPlay source version, e.g. `366.0`.
    pub source_version: String,
    /// 64-bit AirPlay 2 capability bitmask.
    pub features: u64,
    /// AirPlay status flags.
    pub status_flags: u32,
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
