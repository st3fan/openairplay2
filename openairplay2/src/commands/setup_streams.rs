//! `SETUP` phase 2 (`streams` array) — capture the stream's key and format,
//! bind the audio data + control channels, and for buffered audio (type 103)
//! start the receive → decrypt → decode → play pipeline.

use std::net::SocketAddr;

use log::debug;
use plist::{Dictionary, Value};
use tokio::net::UdpSocket;
use validator::Validate;

use crate::errors::CommandError;
use crate::session::{audio_channel, encode_plist, Session, TYPE_BUFFERED, TYPE_REALTIME};

#[derive(Debug, Validate)]
pub struct SetupStreamsParams {
    /// The stream type: 96 (realtime, not decoded) or 103 (buffered).
    pub stream_type: Option<u64>,
    /// The `audioFormat` bitmask — captured but not yet honored
    /// (`aac_params` hard-codes 44.1 kHz stereo AAC-LC).
    pub audio_format: Option<u64>,
    /// The stream's ChaCha20-Poly1305 key (`shk`), for audio decrypt.
    pub shared_key: Option<Vec<u8>>,
    /// Samples per frame — logged only.
    pub spf: Option<u64>,
}

pub async fn setup_streams(
    session: &mut Session,
    params: SetupStreamsParams,
) -> Result<Vec<u8>, CommandError> {
    params.validate()?;
    session.stream_type = params.stream_type;
    session.audio_format = params.audio_format;
    session.stream_key = params.shared_key;
    debug!(
        "SETUP phase 2: type={:?} audioFormat={:?} spf={:?} shk={}",
        params.stream_type,
        params.audio_format,
        params.spf,
        session.stream_key.as_ref().map_or(0, Vec::len)
    );

    let control = UdpSocket::bind(SocketAddr::new(session.local_ip, 0)).await?;
    let control_port = control.local_addr()?.port();
    session
        .tasks
        .push(tokio::spawn(audio_channel(control, "control")));

    // Buffered audio (type 103) uses a TCP data channel we decrypt, decode
    // and play. Realtime (type 96) is UDP, still just logged for now.
    let data_port = if params.stream_type == Some(TYPE_BUFFERED) {
        session.start_buffered_audio().await?
    } else {
        let data = UdpSocket::bind(SocketAddr::new(session.local_ip, 0)).await?;
        let port = data.local_addr()?.port();
        session
            .tasks
            .push(tokio::spawn(audio_channel(data, "audio")));
        port
    };
    debug!("SETUP phase 2: data port {data_port}, control port {control_port}");

    let mut stream_response = Dictionary::new();
    stream_response.insert(
        "type".into(),
        Value::Integer(params.stream_type.unwrap_or(TYPE_REALTIME).into()),
    );
    stream_response.insert(
        "dataPort".into(),
        Value::Integer(u64::from(data_port).into()),
    );
    stream_response.insert(
        "controlPort".into(),
        Value::Integer(u64::from(control_port).into()),
    );
    if params.stream_type == Some(TYPE_BUFFERED) {
        // A buffered-audio ring the sender can push ahead into (~8 MB).
        stream_response.insert(
            "audioBufferSize".into(),
            Value::Integer(8_388_608u64.into()),
        );
    }

    let mut response = Dictionary::new();
    response.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(stream_response)]),
    );
    Ok(encode_plist(&response)?)
}
