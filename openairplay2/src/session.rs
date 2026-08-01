//! Per-connection AirPlay 2 streaming session: the `SETUP` phases, the bound
//! event/data/control sockets, and acknowledgement of the session control
//! methods. For buffered audio (type 103) the TCP data channel is decrypted,
//! decoded (AAC-LC) and delivered to the host's [`AudioSink`], with session
//! milestones reported as [`Event`]s.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;

use crate::buffered::{packet_seq, split_blocks, AudioDecryptor};
use crate::decode::AacDecoder;
use crate::events::{Event, EventSender};
use crate::player::{Player, PlayerSender};
use crate::sink::{AudioSink, SinkFactory};

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
    /// Creates the host's sink at SETUP phase 2.
    sink_factory: SinkFactory,
    /// Where session milestones are reported to the host.
    events: EventSender,
    /// True between `SessionStarted` and `SessionEnded`.
    session_active: bool,
    /// The playback thread, alive for the duration of a buffered stream.
    player: Option<Player>,
    /// Control handle for the player (pause/resume, flush) from the RTSP path.
    player_control: Option<PlayerSender>,
    /// `FLUSHBUFFERED` boundary: the reader drops audio packets with a sequence
    /// number below this, discarding buffered-ahead audio on seek/skip.
    flush_until_seq: Arc<AtomicU64>,
}

impl Session {
    pub fn new(local_ip: IpAddr, sink_factory: SinkFactory, events: EventSender) -> Session {
        Session {
            local_ip,
            tasks: Vec::new(),
            stream_key: None,
            audio_format: None,
            stream_type: None,
            volume: 0.0,
            sink_factory,
            events,
            session_active: false,
            player: None,
            player_control: None,
            flush_until_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Report a session milestone; a host that dropped its receiver is fine.
    fn send_event(&self, event: Event) {
        let _ = self.events.send(event);
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
                self.session_active = true;
                self.send_event(Event::SessionStarted { rate, channels });
                let sink: Box<dyn AudioSink> = (self.sink_factory)(rate, channels);
                let player = Player::spawn(sink);
                let sender = player.sender();
                self.player_control = Some(player.sender());
                self.player = Some(player);
                let max_queued = crate::player::max_queued_samples(rate, channels);
                self.tasks.push(tokio::spawn(buffered_audio(
                    listener,
                    decryptor,
                    decoder,
                    sender,
                    max_queued,
                    self.flush_until_seq.clone(),
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

    /// Handle `SETRATEANCHORTIME`: the sender's play/pause rate (and the RTP
    /// anchor, which we log). The network-time fields matter only with a PTP
    /// clock, so we ignore them (see notes/milestone-6.md). `rate=0` engages
    /// the pause gate (the player drops all audio until resumed); `rate=1`
    /// releases it. A persistent gate is required because the Mac keeps sending
    /// buffered-ahead audio during a pause.
    pub fn set_rate_anchor(&mut self, body: &[u8]) {
        let Some((rate, rtp)) = parse_rate_anchor(body) else {
            warn!("SETRATEANCHORTIME: could not parse body");
            return;
        };
        debug!("SETRATEANCHORTIME rate={rate} rtpTime={rtp}");
        if let Some(ctrl) = &self.player_control {
            ctrl.set_paused(rate == 0);
        }
        self.send_event(Event::Paused(rate == 0));
    }

    /// Handle `FLUSHBUFFERED` (seek/skip): drop buffered audio so playback
    /// jumps promptly instead of draining stale buffer. The Mac buffers far
    /// ahead, so besides clearing our decoded queue we set a sequence boundary
    /// the reader uses to discard the buffered-ahead audio still arriving over
    /// TCP (`flushUntilSeq` = drop everything before this packet).
    pub fn flush(&mut self, body: &[u8]) {
        if let Some(seq) = parse_flush_until_seq(body) {
            self.flush_until_seq.store(seq, Ordering::Relaxed);
            debug!("FLUSHBUFFERED until seq {seq}");
        } else {
            debug!("FLUSHBUFFERED (no seq boundary)");
        }
        if let Some(ctrl) = &self.player_control {
            ctrl.flush();
        }
        self.send_event(Event::Flushed);
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

    /// Apply a `SET_PARAMETER` body — currently just the volume line. The
    /// volume (dB) is recorded (to answer `GET_PARAMETER`) and reported to
    /// the host, which owns the gain path.
    pub fn set_parameter(&mut self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("volume:") {
                if let Ok(db) = v.trim().parse::<f32>() {
                    self.volume = db;
                    debug!("SET_PARAMETER volume {db} dB");
                    self.send_event(Event::Volume { db });
                }
            }
        }
    }

    /// Acknowledge a session control method that needs no body.
    pub fn ack(&self, method: &str) {
        debug!("ack {method}");
    }

    /// Handle `TEARDOWN`: the sender is done with the stream.
    pub fn teardown(&mut self) {
        debug!("ack TEARDOWN");
        self.end_session();
    }

    /// Report `SessionEnded` once per started session.
    fn end_session(&mut self) {
        if self.session_active {
            self.session_active = false;
            self.send_event(Event::SessionEnded);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.end_session();
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

/// Parse a `SETRATEANCHORTIME` plist into `(rate, rtpTime)`. `rate` is 0
/// (pause) or 1 (play); `rtpTime` is the anchor timestamp.
fn parse_rate_anchor(body: &[u8]) -> Option<(u64, u64)> {
    let value = Value::from_reader(io::Cursor::new(body)).ok()?;
    let dict = value.as_dictionary()?;
    let rate = dict.get("rate").and_then(int_field)?;
    let rtp = dict.get("rtpTime").and_then(int_field).unwrap_or(0);
    Some((rate, rtp))
}

/// Read an integer plist field whether it was encoded signed or unsigned.
fn int_field(v: &Value) -> Option<u64> {
    v.as_unsigned_integer()
        .or_else(|| v.as_signed_integer().map(|s| s as u64))
}

/// Parse a `FLUSHBUFFERED` plist for its `flushUntilSeq` boundary (drop all
/// packets with a lower sequence number).
fn parse_flush_until_seq(body: &[u8]) -> Option<u64> {
    let value = Value::from_reader(io::Cursor::new(body)).ok()?;
    value
        .as_dictionary()?
        .get("flushUntilSeq")
        .and_then(int_field)
}

/// The buffered-audio pipeline: accept the sender's TCP connection, frame the
/// stream into packets, decrypt each, decode the AAC, and hand PCM to the
/// player.
async fn buffered_audio(
    listener: TcpListener,
    decryptor: AudioDecryptor,
    mut decoder: AacDecoder,
    player: PlayerSender,
    max_queued: usize,
    flush_until_seq: Arc<AtomicU64>,
) {
    let Ok((mut stream, peer)) = listener.accept().await else {
        return;
    };
    info!("buffered audio connected from {peer}");

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut decrypt_failures: u64 = 0;
    let mut skipped: u64 = 0;
    loop {
        // Backpressure: if the player's queue is full, stop reading so the
        // Mac's TCP send blocks. This bounds latency/memory without a clock —
        // the sound card's drain rate sets the pace.
        while player.pending_samples() > max_queued {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let (packets, used) = split_blocks(&buf);
                let owned: Vec<Vec<u8>> = packets.iter().map(|p| p.to_vec()).collect();
                buf.drain(..used);
                for packet in owned {
                    // Drop buffered-ahead audio below the flush boundary using
                    // the plaintext seq — cheap, no decrypt/decode. This is how
                    // seek/skip discards the old track still in the socket.
                    let boundary = flush_until_seq.load(Ordering::Relaxed);
                    if let Some(seq) = packet_seq(&packet) {
                        if boundary != 0 && (seq as u64) < boundary {
                            skipped += 1;
                            if skipped <= 3 || skipped.is_multiple_of(2000) {
                                debug!("buffered audio: skipping seq {seq} < {boundary}");
                            }
                            continue;
                        }
                    }
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
    info!("buffered audio disconnected ({decrypt_failures} decrypt failures, {skipped} skipped)");
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
    use tokio::sync::mpsc::UnboundedReceiver;

    fn local() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    struct TestSink;

    impl AudioSink for TestSink {
        fn write(&mut self, _pcm: &[i16]) {}
        fn flush(&mut self) {}
    }

    /// A session wired to a discarding sink, plus the host's event receiver.
    fn session() -> (Session, UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let factory: SinkFactory = Arc::new(|_, _| Box::new(TestSink));
        (Session::new(local(), factory, tx), rx)
    }

    #[tokio::test]
    async fn phase1_response_has_event_and_timing_ports() {
        let (mut session, _events) = session();
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
        let (mut session, mut events) = session();
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

        // The host learned the stream started, with the negotiated format.
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                rate: 44100,
                channels: 2
            })
        );

        // Dropping the session (connection closed) ends it exactly once.
        drop(session);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn phase_detection_uses_streams_presence() {
        // A dict with no streams is phase 1 (event port), with streams is phase 2.
        let (mut s1, _events) = session();
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
        let (mut session, mut events) = session();
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
        // The volume reaches the host as an event, in dB as sent.
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
        // Unknown parameters yield an empty body rather than a bad one.
        assert!(session.get_parameter(b"progress\r\n").is_empty());
    }

    #[test]
    fn parses_real_setrateanchortime() {
        // The exact fields a real Mac sent (milestone-5 capture).
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(1u64.into()));
        dict.insert(
            "networkTimeTimelineID".into(),
            Value::Integer((-2116301217048756216i64).into()),
        );
        dict.insert("networkTimeSecs".into(), Value::Integer(1323152u64.into()));
        dict.insert(
            "networkTimeFrac".into(),
            Value::Integer(6275326383463858176u64.into()),
        );
        dict.insert("networkTimeFlags".into(), Value::Integer(0u64.into()));
        dict.insert("rtpTime".into(), Value::Integer(3174381381u64.into()));
        let body = encode_plist(&dict).unwrap();

        assert_eq!(parse_rate_anchor(&body), Some((1, 3174381381)));
    }

    #[test]
    fn parses_real_flushbuffered() {
        // The exact fields a real Mac sent on skip (log capture).
        let mut dict = Dictionary::new();
        dict.insert("flushUntilSeq".into(), Value::Integer(5179978u64.into()));
        dict.insert("flushUntilTS".into(), Value::Integer(2204469244u64.into()));
        let body = encode_plist(&dict).unwrap();
        assert_eq!(parse_flush_until_seq(&body), Some(5179978));

        // A body without the field yields None (no boundary set).
        let empty = encode_plist(&Dictionary::new()).unwrap();
        assert_eq!(parse_flush_until_seq(&empty), None);
    }

    #[test]
    fn rate_anchor_pause_and_missing_fields() {
        // rate 0 = pause; rtpTime defaults to 0 when absent.
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(0u64.into()));
        let body = encode_plist(&dict).unwrap();
        assert_eq!(parse_rate_anchor(&body), Some((0, 0)));

        // No rate field at all → None.
        let empty = encode_plist(&Dictionary::new()).unwrap();
        assert_eq!(parse_rate_anchor(&empty), None);
    }
}
