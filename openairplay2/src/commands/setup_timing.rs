//! `SETUP` phase 1 (no `streams` array) — bind the event channel and answer
//! the event/timing ports. `timingPort` is always 0: PTP exists to align
//! *multiple* outputs, and for a single output the sender's buffering plus
//! this receiver's backpressure suffice.

use std::net::SocketAddr;

use log::debug;
use plist::{Dictionary, Value};
use tokio::net::TcpListener;
use validator::Validate;

use crate::errors::CommandError;
use crate::session::{encode_plist, event_channel, Session};

#[derive(Debug, Validate)]
pub struct SetupTimingParams {
    /// The sender's `timingProtocol` (`PTP`, `NTP`, …) — logged only; this
    /// receiver deliberately runs no timing protocol.
    pub timing_protocol: String,
}

pub async fn setup_timing(
    session: &mut Session,
    params: SetupTimingParams,
) -> Result<Vec<u8>, CommandError> {
    params.validate()?;
    debug!("SETUP phase 1: timingProtocol={}", params.timing_protocol);

    let listener = TcpListener::bind(SocketAddr::new(session.local_ip, 0)).await?;
    let event_port = listener.local_addr()?.port();
    debug!("SETUP phase 1: event port {event_port}");
    session.tasks.push(tokio::spawn(event_channel(listener)));

    let self_ip = session.local_ip.to_string();
    let mut peer_info = Dictionary::new();
    peer_info.insert(
        "Addresses".into(),
        Value::Array(vec![Value::String(self_ip.clone())]),
    );
    peer_info.insert("ID".into(), Value::String(self_ip));

    let mut response = Dictionary::new();
    response.insert(
        "eventPort".into(),
        Value::Integer(u64::from(event_port).into()),
    );
    response.insert("timingPort".into(), Value::Integer(0u64.into()));
    response.insert("timingPeerInfo".into(), Value::Dictionary(peer_info));
    Ok(encode_plist(&response)?)
}
