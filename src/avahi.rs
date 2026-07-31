//! `_airplay._tcp` advertisement via the system Avahi daemon's D-Bus API.
//!
//! Registers directly over D-Bus (no `avahi-utils` dependency); the
//! registration lives as long as the [`Advertisement`] holds the connection.
//! Ported from the AirPlay 1 receiver, changed to the `_airplay._tcp` service
//! type and AirPlay 2 TXT records.

use log::{info, warn};
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

const AVAHI_DEST: &str = "org.freedesktop.Avahi";
const SERVER_IFACE: &str = "org.freedesktop.Avahi.Server";
const GROUP_IFACE: &str = "org.freedesktop.Avahi.EntryGroup";
const IF_UNSPEC: i32 = -1;
const PROTO_UNSPEC: i32 = -1;

/// A live Avahi registration; dropping it withdraws the service.
pub struct Advertisement {
    _connection: Connection,
    _group: OwnedObjectPath,
}

/// Register `service_name` as `_airplay._tcp` on `port` with `txt_records`.
pub async fn publish(
    service_name: &str,
    port: u16,
    txt_records: &[String],
) -> zbus::Result<Advertisement> {
    let connection = Connection::system().await?;

    let group: OwnedObjectPath = connection
        .call_method(
            Some(AVAHI_DEST),
            "/",
            Some(SERVER_IFACE),
            "EntryGroupNew",
            &(),
        )
        .await?
        .body()
        .deserialize()?;

    let txt: Vec<Vec<u8>> = txt_records.iter().map(|r| r.clone().into_bytes()).collect();

    connection
        .call_method(
            Some(AVAHI_DEST),
            &group,
            Some(GROUP_IFACE),
            "AddService",
            &(
                IF_UNSPEC,
                PROTO_UNSPEC,
                0u32,
                service_name,
                "_airplay._tcp",
                "",
                "",
                port,
                txt,
            ),
        )
        .await?;

    connection
        .call_method(Some(AVAHI_DEST), &group, Some(GROUP_IFACE), "Commit", &())
        .await?;

    info!("advertising \"{service_name}\" on _airplay._tcp port {port}");

    Ok(Advertisement {
        _connection: connection,
        _group: group,
    })
}

impl Advertisement {
    /// Best-effort explicit withdrawal (dropping the connection also works).
    pub async fn withdraw(self) {
        if let Err(e) = self
            ._connection
            .call_method(
                Some(AVAHI_DEST),
                &self._group,
                Some(GROUP_IFACE),
                "Free",
                &(),
            )
            .await
        {
            warn!("failed to free Avahi entry group: {e}");
        }
    }
}
