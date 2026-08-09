//! Finding receivers on the network: browse `_openairplay2._tcp` through
//! the Avahi daemon's D-Bus API — the same door a receiver advertises
//! through — and keep a live roster of what is out there.
//!
//! The D-Bus plumbing is kept thin and untested; everything with logic in
//! it — the roster's dedupe across interfaces and address families, TXT
//! parsing, URL formatting — is pure and tested. Where there is no Avahi
//! to talk to (macOS, or a Linux box without the daemon), the roster is
//! simply declared unavailable; the display says so instead of searching
//! forever.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use log::{debug, info, warn};
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::client::Update;

/// The service type a receiver's `--tui-listen` endpoint is advertised
/// under. (The receiver's own copy of this constant explains why it is not
/// `_openairplay2-tui._tcp`: DNS-SD caps the label at 15 characters.)
pub const SERVICE_TYPE: &str = "_openairplay2._tcp";

const AVAHI_DEST: &str = "org.freedesktop.Avahi";
const SERVER_IFACE: &str = "org.freedesktop.Avahi.Server";
const BROWSER_IFACE: &str = "org.freedesktop.Avahi.ServiceBrowser";
const IF_UNSPEC: i32 = -1;
const PROTO_UNSPEC: i32 = -1;

/// A receiver seen on the network, resolved to something connectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    /// The advertised instance name — the same name the AirPlay menu shows.
    pub name: String,
    /// `ws://host:port`, ready to be an [`Endpoint`](crate::client::Endpoint).
    pub url: String,
    /// Whether the endpoint wants a password (`pw=1` in its TXT records).
    pub password: bool,
}

/// What browsing reports to the display.
#[derive(Debug, Clone, PartialEq)]
pub enum Discovery {
    /// The roster changed; this is the whole current list, one entry per
    /// receiver name, sorted.
    Receivers(Vec<Receiver>),
    /// Browsing is not possible here — no Avahi daemon to ask (macOS, say).
    Unavailable(String),
}

/// One advertisement as Avahi keys it: the same service is seen once per
/// interface and address family, and `ItemRemove` names exactly one of them.
type Key = (i32, i32, String);

/// Everything currently advertised, deduplicated for display: one row per
/// receiver *name*, IPv4 preferred when a receiver resolves both ways —
/// a bracketed IPv6 literal is the harder URL to have to read back.
#[derive(Debug, Default)]
struct Roster {
    entries: BTreeMap<Key, Receiver>,
}

impl Roster {
    fn add(&mut self, interface: i32, protocol: i32, receiver: Receiver) {
        self.entries
            .insert((interface, protocol, receiver.name.clone()), receiver);
    }

    fn remove(&mut self, interface: i32, protocol: i32, name: &str) {
        self.entries
            .remove(&(interface, protocol, name.to_string()));
    }

    /// The list to offer: one entry per name, sorted by name.
    fn receivers(&self) -> Vec<Receiver> {
        let mut by_name: BTreeMap<&str, &Receiver> = BTreeMap::new();
        for receiver in self.entries.values() {
            by_name
                .entry(&receiver.name)
                .and_modify(|kept| {
                    if is_ipv6_url(&kept.url) && !is_ipv6_url(&receiver.url) {
                        *kept = receiver;
                    }
                })
                .or_insert(receiver);
        }
        by_name.into_values().cloned().collect()
    }
}

fn is_ipv6_url(url: &str) -> bool {
    url.starts_with("ws://[")
}

/// `ws://host:port`, with the brackets an IPv6 literal needs.
fn endpoint_url(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("ws://[{address}]:{port}")
    } else {
        format!("ws://{address}:{port}")
    }
}

/// Does TXT say the endpoint wants a password (`pw=1`)?
fn wants_password(txt: &[Vec<u8>]) -> bool {
    txt.iter().any(|record| record.as_slice() == b"pw=1")
}

/// A resolved address the display could not actually connect to: IPv6
/// link-local needs a zone id that mDNS does not carry, so a URL built from
/// it would only produce a dead picker entry.
fn unusable_address(address: &str) -> bool {
    match address.parse::<std::net::Ipv6Addr>() {
        // fe80::/10.
        Ok(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

/// Browse until the display goes away, reporting roster changes (and
/// unavailability) on `updates`.
pub async fn run(updates: UnboundedSender<Update>) {
    if let Err(e) = browse(&updates).await {
        info!("discovery unavailable: {e}");
        let _ = updates.send(Update::Discovery(Discovery::Unavailable(e.to_string())));
    }
}

async fn browse(updates: &UnboundedSender<Update>) -> zbus::Result<()> {
    let connection = Connection::system().await?;

    // Subscribe before the browser exists: Avahi replays the current world
    // as a burst of ItemNew signals the moment the browser is created, and
    // a subscription set up after that would race it.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(BROWSER_IFACE)?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &connection, None).await?;

    let browser: OwnedObjectPath = connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "ServiceBrowserNew",
            &(IF_UNSPEC, PROTO_UNSPEC, SERVICE_TYPE, "", 0u32),
        )
        .await?
        .body()
        .deserialize()?;
    info!("browsing for {SERVICE_TYPE} receivers");

    let mut roster = Roster::default();
    while let Some(message) = signals.next().await {
        let message = match message {
            Ok(message) => message,
            Err(e) => {
                debug!("dropping unreadable D-Bus message: {e}");
                continue;
            }
        };
        let header = message.header();
        // The rule matches any ServiceBrowser on the bus; only ours counts.
        if header.path().map(|p| p.as_str()) != Some(browser.as_str()) {
            continue;
        }
        let changed = match header.member().map(|m| m.as_str()) {
            Some("ItemNew") => {
                let Ok((interface, protocol, name, service_type, domain, _flags)) = message
                    .body()
                    .deserialize::<(i32, i32, String, String, String, u32)>()
                else {
                    continue;
                };
                match resolve(
                    &connection,
                    interface,
                    protocol,
                    &name,
                    &service_type,
                    &domain,
                )
                .await
                {
                    Ok(Some(receiver)) => {
                        debug!("found \"{}\" at {}", receiver.name, receiver.url);
                        roster.add(interface, protocol, receiver);
                        true
                    }
                    Ok(None) => false,
                    // Typically the service vanished between the signal and
                    // the resolve; ItemRemove follows.
                    Err(e) => {
                        warn!("cannot resolve \"{name}\": {e}");
                        false
                    }
                }
            }
            Some("ItemRemove") => {
                let Ok((interface, protocol, name, _service_type, _domain, _flags)) = message
                    .body()
                    .deserialize::<(i32, i32, String, String, String, u32)>()
                else {
                    continue;
                };
                debug!("\"{name}\" went away");
                roster.remove(interface, protocol, &name);
                true
            }
            _ => false,
        };
        if changed
            && updates
                .send(Update::Discovery(Discovery::Receivers(roster.receivers())))
                .is_err()
        {
            return Ok(()); // the display is gone
        }
    }
    Ok(())
}

/// Ask Avahi to resolve one advertisement to an address, port and TXT.
/// `Ok(None)` is a resolution the display could do nothing with.
async fn resolve(
    connection: &Connection,
    interface: i32,
    protocol: i32,
    name: &str,
    service_type: &str,
    domain: &str,
) -> zbus::Result<Option<Receiver>> {
    let reply = connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "ResolveService",
            &(
                interface,
                protocol,
                name,
                service_type,
                domain,
                PROTO_UNSPEC,
                0u32,
            ),
        )
        .await?;
    type Resolved = (
        i32,
        i32,
        String,
        String,
        String,
        String,
        i32,
        String,
        u16,
        Vec<Vec<u8>>,
        u32,
    );
    let (_, _, name, _, _, _host, _, address, port, txt, _) =
        reply.body().deserialize::<Resolved>()?;
    if unusable_address(&address) {
        debug!("skipping link-local address {address} for \"{name}\"");
        return Ok(None);
    }
    Ok(Some(Receiver {
        name,
        url: endpoint_url(&address, port),
        password: wants_password(&txt),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver(name: &str, url: &str) -> Receiver {
        Receiver {
            name: name.into(),
            url: url.into(),
            password: false,
        }
    }

    #[test]
    fn urls_bracket_ipv6_literals() {
        assert_eq!(endpoint_url("192.168.1.5", 7392), "ws://192.168.1.5:7392");
        assert_eq!(endpoint_url("2001:db8::5", 7392), "ws://[2001:db8::5]:7392");
    }

    #[test]
    fn txt_names_the_password_requirement() {
        assert!(wants_password(&[b"txtvers=1".to_vec(), b"pw=1".to_vec()]));
        assert!(!wants_password(&[b"txtvers=1".to_vec(), b"pw=0".to_vec()]));
        assert!(!wants_password(&[]));
    }

    #[test]
    fn link_local_addresses_are_unusable() {
        // fe80::/10 — needs a zone id mDNS does not carry.
        assert!(unusable_address("fe80::1c2a:ff:fe00:1"));
        assert!(unusable_address("FE80::1"));
        assert!(unusable_address("febf::1"));
        assert!(!unusable_address("fec0::1"));
        assert!(!unusable_address("2001:db8::5"));
        assert!(!unusable_address("192.168.1.5"));
    }

    #[test]
    fn the_roster_shows_one_row_per_name_sorted() {
        // The same receiver appears once per interface and address family;
        // the picker must not.
        let mut roster = Roster::default();
        roster.add(2, 0, receiver("Kitchen", "ws://10.0.0.2:7392"));
        roster.add(3, 0, receiver("Kitchen", "ws://10.1.0.2:7392"));
        roster.add(2, 0, receiver("Attic", "ws://10.0.0.3:7392"));
        let names: Vec<_> = roster.receivers().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, ["Attic", "Kitchen"]);
    }

    #[test]
    fn the_roster_prefers_ipv4_and_survives_removal() {
        let mut roster = Roster::default();
        roster.add(2, 1, receiver("Kitchen", "ws://[2001:db8::5]:7392"));
        roster.add(2, 0, receiver("Kitchen", "ws://10.0.0.2:7392"));
        assert_eq!(
            roster.receivers(),
            [receiver("Kitchen", "ws://10.0.0.2:7392")],
            "an IPv4 URL beats a bracketed IPv6 one"
        );

        // The v4 advertisement goes away: the v6 one still counts…
        roster.remove(2, 0, "Kitchen");
        assert_eq!(
            roster.receivers(),
            [receiver("Kitchen", "ws://[2001:db8::5]:7392")]
        );
        // …until the last one is gone.
        roster.remove(2, 1, "Kitchen");
        assert_eq!(roster.receivers(), []);
    }
}
