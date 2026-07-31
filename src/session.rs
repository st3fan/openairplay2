//! Per-connection AirPlay 2 streaming session: the `SETUP` phases, the bound
//! event/data/control sockets, and acknowledgement of the session control
//! methods. For buffered audio (type 103) the TCP data channel is decrypted,
//! decoded (AAC-LC) and played to ALSA.

use std::io;
use std::net::{IpAddr, SocketAddr};

use log::{debug, info, warn};
use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;

use crate::buffered::{split_blocks, AudioDecryptor};
use crate::decode::AacDecoder;
use crate::player::{Player, PlayerSender};

/// Stream type constants from the SETUP `streams` array.
const TYPE_REALTIME: u64 = 96;
const TYPE_BUFFERED: u64 = 103;

pub struct Session {
    /// The address the control connection arrived on — what we bind to and
    /// report back so the sender can reach our channels.
    local_ip: IpAddr,
    tasks: Vec<JoinHandle<()>>,
    /// Captured at SETUP phase 2, for audio decrypt/decode.
    stream_key: Option<Vec<u8>>,
    audio_format: Option<u64>,
    stream_type: Option<u64>,
    /// AirPlay volume in dB (0 = full, −30 ≈ min, −144 = mute).
    volume: f32,
    /// ALSA device to play to, or `None` for decode-only.
    alsa_device: Option<String>,
    /// The playback thread, alive for the duration of a buffered stream.
    player: Option<Player>,
}

impl Session {
    pub fn new(local_ip: IpAddr, alsa_device: Option<String>) -> Session {
        Session {
            local_ip,
            tasks: Vec::new(),
            stream_key: None,
            audio_format: None,
            stream_type: None,
            volume: 0.0,
            alsa_device,
            player: None,
        }
    }

    /// Handle a `SETUP` request. Returns the response plist bytes. Phase 1
    /// (no `streams`) sets up the event/timing channels; phase 2 (`streams`
    /// array) sets up the audio data/control channels.
    pub async fn handle_setup(&mut self, body: &[u8]) -> io::Result<Vec<u8>> {
        let request = Value::from_reader(io::Cursor::new(body))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("SETUP plist: {e}")))?;
        let dict = request
            .as_dictionary()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SETUP body not a dict"))?;

        if let Some(streams) = dict.get("streams").and_then(|v| v.as_array()) {
            self.setup_streams(streams).await
        } else {
            self.setup_timing(dict).await
        }
    }

    /// Phase 1: bind the event channel and report the event/timing ports.
    async fn setup_timing(&mut self, dict: &Dictionary) -> io::Result<Vec<u8>> {
        let timing = dict
            .get("timingProtocol")
            .and_then(|v| v.as_string())
            .unwrap_or("(none)");
        info!("SETUP phase 1: timingProtocol={timing}");

        let listener = TcpListener::bind(SocketAddr::new(self.local_ip, 0)).await?;
        let event_port = listener.local_addr()?.port();
        info!("SETUP phase 1: event port {event_port}");
        self.tasks.push(tokio::spawn(event_channel(listener)));

        let self_ip = self.local_ip.to_string();
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
        encode_plist(&response)
    }

    /// Phase 2: bind the audio data + control channels for stream 0 and report
    /// their ports.
    async fn setup_streams(&mut self, streams: &[Value]) -> io::Result<Vec<u8>> {
        let stream = streams
            .first()
            .and_then(|v| v.as_dictionary())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty streams array"))?;

        let stream_type = stream.get("type").and_then(|v| v.as_unsigned_integer());
        self.stream_type = stream_type;
        self.audio_format = stream
            .get("audioFormat")
            .and_then(|v| v.as_unsigned_integer());
        self.stream_key = stream
            .get("shk")
            .and_then(|v| v.as_data())
            .map(<[u8]>::to_vec);
        let spf = stream.get("spf").and_then(|v| v.as_unsigned_integer());
        info!(
            "SETUP phase 2: type={stream_type:?} audioFormat={:?} spf={spf:?} shk={}",
            self.audio_format,
            self.stream_key.as_ref().map_or(0, Vec::len)
        );

        let control = UdpSocket::bind(SocketAddr::new(self.local_ip, 0)).await?;
        let control_port = control.local_addr()?.port();
        self.tasks
            .push(tokio::spawn(audio_channel(control, "control")));

        // Buffered audio (type 103) uses a TCP data channel we decrypt, decode
        // and play. Realtime (type 96) is UDP, still just logged for now.
        let data_port = if stream_type == Some(TYPE_BUFFERED) {
            self.start_buffered_audio().await?
        } else {
            let data = UdpSocket::bind(SocketAddr::new(self.local_ip, 0)).await?;
            let port = data.local_addr()?.port();
            self.tasks.push(tokio::spawn(audio_channel(data, "audio")));
            port
        };
        info!("SETUP phase 2: data port {data_port}, control port {control_port}");

        let mut stream_response = Dictionary::new();
        stream_response.insert(
            "type".into(),
            Value::Integer(stream_type.unwrap_or(TYPE_REALTIME).into()),
        );
        stream_response.insert(
            "dataPort".into(),
            Value::Integer(u64::from(data_port).into()),
        );
        stream_response.insert(
            "controlPort".into(),
            Value::Integer(u64::from(control_port).into()),
        );
        if stream_type == Some(TYPE_BUFFERED) {
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
        encode_plist(&response)
    }

    /// Bind the buffered-audio TCP data channel and start the
    /// receive → decrypt → decode → play pipeline. Returns the bound port.
    async fn start_buffered_audio(&mut self) -> io::Result<u16> {
        let listener = TcpListener::bind(SocketAddr::new(self.local_ip, 0)).await?;
        let port = listener.local_addr()?.port();

        let decryptor = self.stream_key.as_deref().and_then(AudioDecryptor::new);
        let (rate, channels) = aac_params(self.audio_format);
        match (decryptor, AacDecoder::new(rate, channels)) {
            (Some(decryptor), Ok(decoder)) => {
                let player = Player::spawn(rate, channels, self.alsa_device.clone());
                let sender = player.sender();
                self.player = Some(player);
                self.tasks.push(tokio::spawn(buffered_audio(
                    listener, decryptor, decoder, sender,
                )));
                info!("buffered audio: TCP data port {port}, {rate} Hz {channels}ch");
            }
            (None, _) => {
                warn!("buffered audio: missing/invalid shk; draining without decode");
                self.tasks.push(tokio::spawn(drain_tcp(listener)));
            }
            (_, Err(e)) => {
                warn!("buffered audio: decoder init failed ({e}); draining");
                self.tasks.push(tokio::spawn(drain_tcp(listener)));
            }
        }
        Ok(port)
    }

    /// Answer a `GET_PARAMETER` query. A sender asks `volume\r\n` during setup
    /// and expects `volume: <dB>\r\n` back (an empty response makes it abort).
    pub fn get_parameter(&self, body: &[u8]) -> Vec<u8> {
        let query = String::from_utf8_lossy(body);
        if query.trim() == "volume" {
            format!("volume: {:.6}\r\n", self.volume).into_bytes()
        } else {
            debug!("GET_PARAMETER for unknown parameter: {query:?}");
            Vec::new()
        }
    }

    /// Apply a `SET_PARAMETER` body — currently just the volume line.
    pub fn set_parameter(&mut self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("volume:") {
                if let Ok(db) = v.trim().parse::<f32>() {
                    self.volume = db;
                    debug!("SET_PARAMETER volume {db} dB");
                }
            }
        }
    }

    /// Acknowledge a session control method that needs no body.
    pub fn ack(&self, method: &str) {
        debug!("ack {method}");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

fn encode_plist(dict: &Dictionary) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    Value::Dictionary(dict.clone())
        .to_writer_binary(&mut buf)
        .map_err(|e| io::Error::other(format!("plist encode: {e}")))?;
    Ok(buf)
}

/// Accept the sender's event channel and drain it (we don't emit events for
/// basic playback). Keeps the AirPlay session healthy.
async fn event_channel(listener: TcpListener) {
    let Ok((mut stream, peer)) = listener.accept().await else {
        return;
    };
    debug!("event channel connected from {peer}");
    let mut buf = [0u8; 4096];
    use tokio::io::AsyncReadExt;
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        debug!("event: {n} bytes");
    }
}

/// Receive and log packets on a realtime/control UDP socket.
async fn audio_channel(socket: UdpSocket, label: &'static str) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut count: u64 = 0;
    loop {
        match socket.recv(&mut buf).await {
            Ok(n) => {
                count += 1;
                if count <= 3 || count.is_multiple_of(250) {
                    info!("{label}: {count} packets, last {n} bytes");
                }
            }
            Err(e) => {
                warn!("{label} socket error: {e}");
                return;
            }
        }
    }
}

/// Decode parameters for the buffered stream. AirPlay 2 buffered audio is
/// AAC-LC 44.1 kHz stereo (audioFormat bit `0x400000`); other formats aren't
/// negotiated yet, so default to that.
fn aac_params(_audio_format: Option<u64>) -> (u32, u8) {
    (44100, 2)
}

/// The buffered-audio pipeline: accept the sender's TCP connection, frame the
/// stream into packets, decrypt each, decode the AAC, and hand PCM to the
/// player.
async fn buffered_audio(
    listener: TcpListener,
    decryptor: AudioDecryptor,
    mut decoder: AacDecoder,
    player: PlayerSender,
) {
    let Ok((mut stream, peer)) = listener.accept().await else {
        return;
    };
    info!("buffered audio connected from {peer}");

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut decrypt_failures: u64 = 0;
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let (packets, used) = split_blocks(&buf);
                let owned: Vec<Vec<u8>> = packets.iter().map(|p| p.to_vec()).collect();
                buf.drain(..used);
                for packet in owned {
                    let Some(audio) = decryptor.decrypt(&packet) else {
                        decrypt_failures += 1;
                        if decrypt_failures <= 3 {
                            debug!("buffered audio: packet decrypt failed");
                        }
                        continue;
                    };
                    match decoder.decode(&audio.payload) {
                        Ok(pcm) if !pcm.is_empty() => player.play(pcm),
                        Ok(_) => {}
                        Err(e) => debug!("buffered audio: decode error: {e}"),
                    }
                }
            }
            Err(e) => {
                warn!("buffered audio read error: {e}");
                break;
            }
        }
    }
    info!("buffered audio disconnected ({decrypt_failures} decrypt failures)");
}

/// Accept and drain a buffered-audio connection we can't decode (missing key
/// or unsupported format), so the sender isn't left hanging.
async fn drain_tcp(listener: TcpListener) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let mut buf = vec![0u8; 64 * 1024];
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn local() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    #[tokio::test]
    async fn phase1_response_has_event_and_timing_ports() {
        let mut session = Session::new(local(), None);
        // Minimal phase-1 plist: timingProtocol=PTP, no streams.
        let mut dict = Dictionary::new();
        dict.insert("timingProtocol".into(), Value::String("PTP".into()));
        let body = encode_plist(&dict).unwrap();

        let response = session.handle_setup(&body).await.unwrap();
        let value = Value::from_reader(io::Cursor::new(response)).unwrap();
        let d = value.as_dictionary().unwrap();
        assert!(d.get("eventPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert_eq!(d.get("timingPort").unwrap().as_unsigned_integer(), Some(0));
        assert!(d.get("timingPeerInfo").unwrap().as_dictionary().is_some());
    }

    #[tokio::test]
    async fn phase2_response_binds_ports_and_echoes_type() {
        let mut session = Session::new(local(), None);
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(TYPE_BUFFERED.into()));
        stream.insert("audioFormat".into(), Value::Integer(0x40000u64.into()));
        stream.insert("shk".into(), Value::Data(vec![7u8; 32]));
        let mut dict = Dictionary::new();
        dict.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let body = encode_plist(&dict).unwrap();

        let response = session.handle_setup(&body).await.unwrap();
        let value = Value::from_reader(io::Cursor::new(response)).unwrap();
        let streams = value
            .as_dictionary()
            .unwrap()
            .get("streams")
            .unwrap()
            .as_array()
            .unwrap();
        let s = streams[0].as_dictionary().unwrap();
        assert_eq!(
            s.get("type").unwrap().as_unsigned_integer(),
            Some(TYPE_BUFFERED)
        );
        assert!(s.get("dataPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert!(s.get("controlPort").unwrap().as_unsigned_integer().unwrap() > 0);
        assert!(s.get("audioBufferSize").is_some());
        assert_eq!(session.stream_key, Some(vec![7u8; 32]));
    }

    #[tokio::test]
    async fn phase_detection_uses_streams_presence() {
        // A dict with no streams is phase 1 (event port), with streams is phase 2.
        let mut s1 = Session::new(local(), None);
        let empty = encode_plist(&Dictionary::new()).unwrap();
        let r1 = s1.handle_setup(&empty).await.unwrap();
        assert!(Value::from_reader(io::Cursor::new(r1))
            .unwrap()
            .as_dictionary()
            .unwrap()
            .contains_key("eventPort"));
    }

    #[test]
    fn volume_query_returns_current_volume() {
        let mut session = Session::new(local(), None);
        // A sender's exact query is "volume\r\n".
        assert_eq!(
            session.get_parameter(b"volume\r\n"),
            b"volume: 0.000000\r\n"
        );
        session.set_parameter(b"volume: -12.5\r\n");
        assert_eq!(
            session.get_parameter(b"volume\r\n"),
            b"volume: -12.500000\r\n"
        );
        // Unknown parameters yield an empty body rather than a bad one.
        assert!(session.get_parameter(b"progress\r\n").is_empty());
    }
}
