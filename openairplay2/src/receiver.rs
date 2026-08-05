//! The embedding facade: configure a [`Receiver`], hand it a sink factory and
//! an event channel, and run it on your own tokio runtime.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use log::warn;
use tokio::net::TcpListener;

use crate::events::EventSender;
use crate::identity::Identity;
use crate::server::{serve, Context};
use crate::sink::{AudioSink, SinkFactory};
use crate::{avahi, info, mac, Config};

const DEFAULT_NAME: &str = "OpenAirPlay2";
const DEFAULT_PORT: u16 = 7000;
const DEFAULT_MODEL: &str = "OpenAirPlay2,1";
const DEFAULT_SOURCE_VERSION: &str = "366.0";
/// shairport-sync's known-good AirPlay 2 features: transient pairing (bit 48)
/// plus AirPlay 2 audio, plus the metadata bits 15/16/17 (covers, progress,
/// DAAP text) — senders only push track metadata and artwork when these are
/// advertised. Getting this wrong makes senders offer AirPlay 1 or nothing
/// at all.
const DEFAULT_FEATURES: u64 = 0x0001_8340_405F_CA00;
const DEFAULT_STATUS_FLAGS: u32 = 0x4;
/// Status-flag bit 7 ("Password required"), which makes Apple senders prompt
/// for a pincode and pair with it instead of silently using transient `3939`
/// (proven on iOS 26). Shairport-sync sets the same bit when a password is
/// configured.
const PASSWORD_REQUIRED_FLAG: u32 = 1 << 7;

/// Advertised status flags: audio-attached, plus "password required" when a
/// pincode is configured.
fn status_flags(pincode: bool) -> u32 {
    if pincode {
        DEFAULT_STATUS_FLAGS | PASSWORD_REQUIRED_FLAG
    } else {
        DEFAULT_STATUS_FLAGS
    }
}
/// Locally-administered fallback (starts with 0x02) when discovery finds no
/// interface MAC.
const FALLBACK_MAC: [u8; 6] = [0x02, 0x4f, 0x41, 0x50, 0x32, 0x00];

/// Configures a [`Receiver`]. Create one with [`Receiver::builder`].
///
/// The identity (Ed25519 key + `pi` UUID) must be stable across restarts —
/// senders remember a receiver by it — so exactly one of
/// [`identity`](Self::identity) or [`identity_path`](Self::identity_path) is
/// required; there is no ephemeral default.
pub struct ReceiverBuilder {
    name: String,
    port: u16,
    mac: Option<[u8; 6]>,
    identity: Option<Identity>,
    identity_path: Option<PathBuf>,
    advertise: bool,
    pincode: Option<String>,
}

impl ReceiverBuilder {
    fn new() -> ReceiverBuilder {
        ReceiverBuilder {
            name: DEFAULT_NAME.to_string(),
            port: DEFAULT_PORT,
            mac: None,
            identity: None,
            identity_path: None,
            advertise: true,
            pincode: None,
        }
    }

    /// The receiver name senders see in the AirPlay picker.
    /// Default: `OpenAirPlay2`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The control port to listen on (also advertised). Default: 7000.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The MAC used as the AirPlay `deviceid`. Default: discovered from the
    /// first up, non-loopback network interface, with a fixed
    /// locally-administered fallback.
    pub fn mac(mut self, mac: [u8; 6]) -> Self {
        self.mac = Some(mac);
        self
    }

    /// Use this identity. Mutually exclusive with
    /// [`identity_path`](Self::identity_path); the host owns persistence.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Load the identity from this file, generating and persisting one on
    /// first run.
    pub fn identity_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_path = Some(path.into());
        self
    }

    /// Whether [`Receiver::run`] registers `_airplay._tcp` with the system
    /// Avahi daemon (default `true`). Pass `false` when the host owns its
    /// mDNS registration and use [`Receiver::txt_records`] to advertise.
    pub fn advertise(mut self, advertise: bool) -> Self {
        self.advertise = advertise;
        self
    }

    /// Require this pincode to pair: a sender must enter it (AirPlay 2's
    /// analog of openairplay1's `--password`); the receiver advertises
    /// "password required" (status-flag bit 7). Unset → transient `3939`.
    /// Never logged.
    pub fn pincode(mut self, pincode: impl Into<String>) -> Self {
        self.pincode = Some(pincode.into());
        self
    }

    /// Resolve the identity and MAC and produce a runnable [`Receiver`].
    pub fn build(self) -> io::Result<Receiver> {
        let identity = match (self.identity, &self.identity_path) {
            (Some(identity), None) => identity,
            (None, Some(path)) => Identity::load_or_create(path)?,
            (Some(_), Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "set either identity or identity_path, not both",
                ))
            }
            (None, None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "an identity is required (identity or identity_path): \
                     senders remember a receiver by it, so it must be stable",
                ))
            }
        };
        let mac = self.mac.or_else(mac::discover).unwrap_or_else(|| {
            warn!("no network interface MAC found, using a fixed fallback");
            FALLBACK_MAC
        });
        Ok(Receiver {
            config: Config {
                name: self.name,
                port: self.port,
                mac,
                model: DEFAULT_MODEL.to_string(),
                source_version: DEFAULT_SOURCE_VERSION.to_string(),
                features: DEFAULT_FEATURES,
                status_flags: status_flags(self.pincode.is_some()),
                pincode: self.pincode,
            },
            identity,
            advertise: self.advertise,
        })
    }
}

/// An AirPlay 2 audio receiver: senders discover it, pair with it, and stream
/// to it; the host gets decoded PCM (via its [`AudioSink`]) and
/// [`Event`](crate::Event)s. One sender → one stream → one output.
pub struct Receiver {
    config: Config,
    identity: Identity,
    advertise: bool,
}

impl Receiver {
    /// Start configuring a receiver. See [`ReceiverBuilder`] for the options;
    /// an identity is the one required setting.
    pub fn builder() -> ReceiverBuilder {
        ReceiverBuilder::new()
    }

    /// The resolved configuration (name, port, deviceid, features).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The stable device identity (`pk`, `pi`).
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The `_airplay._tcp` TXT records, for hosts that own their mDNS
    /// registration (built with `advertise(false)`). Advertise these on
    /// [`Config::port`] under the receiver's name.
    pub fn txt_records(&self) -> Vec<String> {
        info::txt_records(&self.config, &self.identity)
    }

    /// Serve AirPlay on the caller's runtime until a listener error.
    ///
    /// `sink_factory` is invoked at `SETUP` phase 2 with the negotiated
    /// sample rate and channel count, once per stream; the sink is then fed
    /// from a dedicated playback thread (see [`AudioSink`]). Session
    /// milestones are reported on `events`; dropping the receiving half is
    /// allowed and simply discards them.
    ///
    /// Cancellation: dropping the returned future (e.g. in `select!`) stops
    /// accepting new connections and withdraws the Avahi advertisement;
    /// already-accepted connections are detached and end when their sockets
    /// close.
    pub async fn run<F>(self, sink_factory: F, events: EventSender) -> io::Result<()>
    where
        F: Fn(u32, u8) -> Box<dyn AudioSink> + Send + Sync + 'static,
    {
        // Dual-stack if possible (IPv4 clients arrive v4-mapped), else IPv4.
        let port = self.config.port;
        let listener =
            match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).await {
                Ok(l) => l,
                Err(_) => TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
                    .await
                    .map_err(|e| {
                        io::Error::new(e.kind(), format!("cannot bind control port {port}: {e}"))
                    })?,
            };

        let _advertisement = if self.advertise {
            let records = self.txt_records();
            match avahi::publish(&self.config.name, port, &records).await {
                Ok(ad) => Some(ad),
                Err(e) => {
                    warn!("avahi advertisement disabled ({e}); is avahi-daemon running?");
                    None
                }
            }
        } else {
            None
        };

        let sink_factory: SinkFactory = Arc::new(sink_factory);
        let context = Arc::new(Context {
            config: self.config,
            identity: self.identity,
            sink_factory,
            events,
        });
        serve(listener, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pincode_sets_password_required_status_flag() {
        // No pincode: just audio attached.
        assert_eq!(status_flags(false), 0x4);
        // Pincode set: also advertise "password required" (status bit 7),
        // which is what makes iOS prompt for it.
        assert_eq!(status_flags(true), 0x4 | 1 << 7);
    }

    #[test]
    fn build_applies_the_pincode() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .pincode("1212")
            .build()
            .unwrap();
        assert_eq!(receiver.config().pincode.as_deref(), Some("1212"));
        assert_eq!(receiver.config().status_flags, 0x4 | 1 << 7);
    }

    #[test]
    fn build_requires_an_identity() {
        let err = Receiver::builder()
            .build()
            .err()
            .expect("no identity must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = Receiver::builder()
            .identity(Identity::generate())
            .identity_path("/nonexistent/identity")
            .build()
            .err()
            .expect("both identity sources must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn build_applies_defaults_and_overrides() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .build()
            .unwrap();
        let config = receiver.config();
        assert_eq!(config.name, "OpenAirPlay2");
        assert_eq!(config.port, 7000);
        assert_eq!(config.features, 0x0001_8340_405F_CA00);
        assert_eq!(config.device_id(), "AA:BB:CC:DD:EE:FF");

        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .name("Office")
            .port(7100)
            .advertise(false)
            .build()
            .unwrap();
        assert_eq!(receiver.config().name, "Office");
        assert_eq!(receiver.config().port, 7100);
        assert!(!receiver.advertise);
    }

    #[test]
    fn txt_records_match_identity_and_config() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .build()
            .unwrap();
        let records = receiver.txt_records();
        assert!(records.iter().any(|r| r == "deviceid=AA:BB:CC:DD:EE:FF"));
        let pk = format!("pk={}", receiver.identity().public_key_hex());
        assert!(records.iter().any(|r| r == &pk));
    }
}
