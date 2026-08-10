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

use log::{debug, warn};
use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

use crate::buffered::{packet_seq, split_blocks, AudioDecryptor};
use crate::decode::AacDecoder;
use crate::dmap;
use crate::events::{Event, EventSender};
use crate::player::{frames_to_duration, Player, PlayerSender, Position, Track, TrackAnchor};
use crate::sink::{AudioSink, SinkFactory};
use crate::takeover::ActiveGuard;

/// Stream type constants from the SETUP `streams` array.
const TYPE_REALTIME: u64 = 96;
const TYPE_BUFFERED: u64 = 103;

/// `SET_PARAMETER` content type carrying DMAP track metadata.
const DMAP_CONTENT_TYPE: &str = "application/x-dmap-tagged";

/// The AirPlay volume range: `0` is full scale and `-144` is the mute
/// sentinel, so anything outside this says nothing a volume can mean.
const MUTE_DB: f32 = -144.0;
const FULL_DB: f32 = 0.0;

pub struct Session {
    /// The address the control connection arrived on — what we bind to and
    /// report back so the sender can reach our channels.
    local_ip: IpAddr,
    /// The address the sender connected *from*, reported to the host with
    /// `SessionStarted` (a display shows it; nothing else uses it).
    peer_ip: IpAddr,
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
    /// Metadata/artwork that arrived while no session was active (senders
    /// may push them during the handshake, before SETUP phase 2). The
    /// latest of each is latched here and delivered right after
    /// `SessionStarted`, so the host only ever sees them inside a session.
    pending_metadata: Option<Event>,
    pending_artwork: Option<Event>,
    /// The playback thread, alive for the duration of a buffered stream.
    player: Option<Player>,
    /// Control handle for the player (pause/resume, flush) from the RTSP path.
    player_control: Option<PlayerSender>,
    /// `FLUSHBUFFERED` boundary: the reader drops arriving audio packets with
    /// a sequence number below this, discarding buffered-ahead audio on
    /// seek/skip. Self-clearing (consumed when the stream reaches it) and
    /// reset at stream setup — a stale boundary discards wanted audio.
    flush_until_seq: Arc<AtomicU64>,
    /// The current track's extent on the RTP timeline, from the sender's
    /// `progress:` line. Shared with the playback thread, which turns it into
    /// a position that follows the audio.
    track: TrackAnchor,
    /// Proof that this connection owns the active-session slot, held here so
    /// that it is released only once this session has fully torn down — see
    /// [`Drop`] and [`crate::takeover`].
    active_guard: Option<ActiveGuard>,
}

impl Session {
    pub fn new(
        local_ip: IpAddr,
        peer_ip: IpAddr,
        sink_factory: SinkFactory,
        events: EventSender,
    ) -> Session {
        Session {
            local_ip,
            peer_ip,
            tasks: Vec::new(),
            stream_key: None,
            audio_format: None,
            stream_type: None,
            volume: 0.0,
            sink_factory,
            events,
            session_active: false,
            pending_metadata: None,
            pending_artwork: None,
            player: None,
            player_control: None,
            flush_until_seq: Arc::new(AtomicU64::new(0)),
            track: TrackAnchor::default(),
            active_guard: None,
        }
    }

    /// Take ownership of the active-session slot (the caller claimed it at
    /// `SETUP`). Kept until this session is dropped, which is what tells a
    /// sender taking over that the audio device is free.
    pub fn set_active_guard(&mut self, guard: ActiveGuard) {
        self.active_guard = Some(guard);
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
        debug!("SETUP phase 1: timingProtocol={timing}");

        let listener = TcpListener::bind(SocketAddr::new(self.local_ip, 0)).await?;
        let event_port = listener.local_addr()?.port();
        debug!("SETUP phase 1: event port {event_port}");
        self.tasks
            .push(tokio::spawn(event_channel(listener, self.peer_ip)));

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
        debug!(
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
        debug!("SETUP phase 2: data port {data_port}, control port {control_port}");

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
        // A new stream is a fresh sequence-number epoch: a boundary left over
        // from an earlier stream on this connection would silently discard
        // the new stream's audio (measured at ~47 s in one session).
        self.flush_until_seq.store(0, Ordering::Relaxed);
        match (decryptor, AacDecoder::new(rate, channels)) {
            (Some(decryptor), Ok(decoder)) => {
                self.session_active = true;
                self.send_event(Event::SessionStarted {
                    rate,
                    channels,
                    peer: self.peer_ip,
                });
                // Replay metadata/artwork that arrived before the session
                // started, so they land inside it.
                let latched = [self.pending_metadata.take(), self.pending_artwork.take()];
                for event in latched.into_iter().flatten() {
                    self.send_event(event);
                }
                let sink: Box<dyn AudioSink> = (self.sink_factory)(rate, channels);
                // A new stream is a new track timeline: drop any extent the
                // previous one left behind, so no position is reported until
                // the sender says where this track starts.
                *self.track.lock().unwrap() = None;
                let position = Position::new(rate, self.events.clone(), self.track.clone());
                let player = Player::spawn(sink, Some(position));
                let sender = player.sender();
                self.player_control = Some(player.sender());
                self.player = Some(player);
                let max_queued = crate::player::max_queued_samples(rate, channels);
                self.tasks.push(tokio::spawn(buffered_audio(
                    listener,
                    self.peer_ip,
                    decryptor,
                    decoder,
                    sender,
                    max_queued,
                    self.flush_until_seq.clone(),
                )));
                debug!("buffered audio: TCP data port {port}, {rate} Hz {channels}ch");
            }
            (None, _) => {
                warn!("buffered audio: missing/invalid shk; draining without decode");
                self.tasks
                    .push(tokio::spawn(drain_tcp(listener, self.peer_ip)));
            }
            (_, Err(e)) => {
                warn!("buffered audio: decoder init failed ({e}); draining");
                self.tasks
                    .push(tokio::spawn(drain_tcp(listener, self.peer_ip)));
            }
        }
        Ok(port)
    }

    /// Handle `SETRATEANCHORTIME`: the sender's play/pause rate (and the RTP
    /// anchor, which we log). The network-time fields matter only with a PTP
    /// clock, so we ignore them (see notes/milestone-6.md). `rate=0` engages
    /// the pause gate — the player *holds* queued and arriving audio (a
    /// flush-less pause gives no licence to drop anything; the sender expects
    /// it all to still be buffered at resume) — and `rate=1` releases it,
    /// playing the held audio from where playback stopped.
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

    /// Handle `FLUSHBUFFERED` (seek/skip): discard exactly the audio the
    /// sender names — queued/held packets with a sequence stamp below
    /// `flushUntilSeq`, plus the stale audio still arriving over TCP (the
    /// sender buffers far ahead) — while retaining everything at or after
    /// the boundary. A body without a boundary discards all queued audio.
    pub fn flush(&mut self, body: &[u8]) {
        let boundary = parse_flush_until_seq(body);
        match boundary {
            Some(seq) => {
                self.flush_until_seq.store(seq, Ordering::Relaxed);
                debug!("FLUSHBUFFERED until seq {seq}");
            }
            None => debug!("FLUSHBUFFERED (no seq boundary)"),
        }
        if let Some(ctrl) = &self.player_control {
            ctrl.flush(boundary);
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

    /// Apply a `SET_PARAMETER` body, dispatched on its `Content-Type`:
    /// DMAP track metadata, cover art, or (the default) `text/parameters`
    /// lines — currently the volume.
    pub fn set_parameter(&mut self, content_type: Option<&str>, body: &[u8]) {
        // Strip any parameters ("; charset=...") from the media type.
        let media_type = content_type.map(|ct| ct.split(';').next().unwrap_or(ct).trim());
        match media_type {
            Some(ct) if ct.eq_ignore_ascii_case(DMAP_CONTENT_TYPE) => self.set_metadata(body),
            Some(ct)
                if ct
                    .get(..6)
                    .is_some_and(|p| p.eq_ignore_ascii_case("image/")) =>
            {
                self.set_artwork(ct, body)
            }
            _ => self.set_text_parameters(body),
        }
    }

    /// The `text/parameters` flavor: the volume line, and the sender's
    /// position report. The volume (dB) is recorded (to answer
    /// `GET_PARAMETER`) and reported to the host, which owns the gain path.
    fn set_text_parameters(&mut self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("volume:") {
                match v.trim().parse::<f32>().ok().and_then(sanitize_volume) {
                    Some(db) => {
                        self.volume = db;
                        debug!("SET_PARAMETER volume {db} dB");
                        self.send_event(Event::Volume { db });
                    }
                    // Unparseable or non-finite: the knob does not move.
                    None => debug!("SET_PARAMETER unusable volume {:?}", v.trim()),
                }
            } else if let Some(v) = line.strip_prefix("progress:") {
                self.set_progress(v.trim());
            }
        }
    }

    /// `progress: <start>/<current>/<end>` — three RTP timestamps naming the
    /// current track's extent and the sender's idea of the position.
    ///
    /// The extent is what matters: it is handed to the playback thread, which
    /// turns the audio it plays into a running position. The sender's own
    /// `current` is reported once here (it is right at track start, which is
    /// essentially the only time this line arrives) and never extrapolated
    /// from.
    fn set_progress(&mut self, value: &str) {
        let mut parts = value.split('/').map(|p| p.trim().parse::<u32>());
        let (Some(Ok(start)), Some(Ok(current)), Some(Ok(end)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            debug!("SET_PARAMETER progress: unparseable value {value:?}");
            return;
        };
        *self.track.lock().unwrap() = Some(Track { start, end });
        if !self.session_active {
            return; // a position without a stream means nothing
        }
        let (rate, _) = aac_params(self.audio_format);
        // A seek can put `current` before `start`, and the timestamps wrap;
        // saturating subtraction keeps both readings sane rather than
        // reporting a position of ~27 hours.
        let elapsed = frames_to_duration(current.saturating_sub(start), rate);
        let duration = frames_to_duration(end.saturating_sub(start), rate);
        debug!(
            "SET_PARAMETER progress {:.1}s / {:.1}s",
            elapsed.as_secs_f32(),
            duration.as_secs_f32()
        );
        self.send_event(Event::Progress { elapsed, duration });
    }

    /// DMAP track metadata. Metadata is decoration: an unparseable payload
    /// is dropped with a debug log, never an error to the sender.
    fn set_metadata(&mut self, body: &[u8]) {
        let Some(meta) = dmap::parse(body) else {
            debug!(
                "SET_PARAMETER metadata: unrecognized DMAP payload ({} bytes)",
                body.len()
            );
            return;
        };
        debug!(
            "SET_PARAMETER metadata: title={:?} artist={:?} album={:?}",
            meta.title, meta.artist, meta.album
        );
        self.send_session_event(Event::Metadata {
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
        });
    }

    /// Cover art, forwarded as-is (`image/none`/empty means cleared).
    fn set_artwork(&mut self, content_type: &str, body: &[u8]) {
        debug!(
            "SET_PARAMETER artwork: {content_type}, {} bytes",
            body.len()
        );
        self.send_session_event(Event::Artwork {
            content_type: content_type.to_string(),
            data: body.to_vec(),
        });
    }

    /// Deliver an event the host expects only inside a session; before
    /// `SessionStarted` it is latched (latest wins) and replayed once the
    /// session starts.
    fn send_session_event(&mut self, event: Event) {
        if self.session_active {
            self.send_event(event);
        } else if matches!(event, Event::Artwork { .. }) {
            self.pending_artwork = Some(event);
        } else {
            self.pending_metadata = Some(event);
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
        // Order matters when a sender is taking this session over: dropping
        // the player joins the playback thread, which releases the host's
        // sink and with it the audio device. Only then is the active-session
        // guard released, because that is what the new stream is waiting for
        // — and an exclusive ALSA device would refuse it if we still held it.
        self.player_control = None;
        self.player = None;
        self.active_guard = None;
    }
}

fn encode_plist(dict: &Dictionary) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    Value::Dictionary(dict.clone())
        .to_writer_binary(&mut buf)
        .map_err(|e| io::Error::other(format!("plist encode: {e}")))?;
    Ok(buf)
}

/// Accept the first TCP connection whose peer IP matches `expected`, closing any
/// connection from another address and continuing to listen.
///
/// The event and data ports are ephemeral but scannable, and only the control
/// connection's peer is a legitimate client. Without this check a LAN peer that
/// wins the connect race (or scans the port) would occupy the single accepted
/// connection and starve the real stream — and, absent the pairing gate, could
/// push its own audio into it (#146). Returns `None` only if the listener
/// itself fails; a mismatched peer is dropped and the accept loop continues, so
/// the legitimate sender still gets in behind an attacker's connection.
async fn accept_from(
    listener: &TcpListener,
    expected: IpAddr,
    label: &str,
) -> Option<(TcpStream, SocketAddr)> {
    loop {
        let (stream, peer) = listener.accept().await.ok()?;
        if peer.ip() == expected {
            return Some((stream, peer));
        }
        // Dropping `stream` at the end of the iteration closes it.
        debug!("{label}: rejecting connection from {peer} (expected peer {expected})");
    }
}

/// Accept the sender's event channel and drain it (we don't emit events for
/// basic playback). Keeps the AirPlay session healthy.
async fn event_channel(listener: TcpListener, expected: IpAddr) {
    let Some((mut stream, peer)) = accept_from(&listener, expected, "event channel").await else {
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
                    debug!("{label}: {count} packets, last {n} bytes");
                }
            }
            Err(e) => {
                warn!("{label} socket error: {e}");
                return;
            }
        }
    }
}

/// Make a parsed `volume:` value safe to hand to a host's gain path, or reject
/// it. `f32::parse` accepts `nan`, `inf` and overflowing literals like `1e40`,
/// and none of the arithmetic downstream expects them: NaN survives a
/// `min(0.0)` (that returns the *other* operand) and comes out as full scale,
/// which is the loudest possible reading of a value that means nothing. So a
/// non-finite volume is refused outright — the knob keeps its old position —
/// and a finite one is clamped into the range AirPlay actually uses, which only
/// rewrites values that were already nonsense.
fn sanitize_volume(db: f32) -> Option<f32> {
    db.is_finite().then(|| db.clamp(MUTE_DB, FULL_DB))
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
    expected: IpAddr,
    decryptor: AudioDecryptor,
    mut decoder: AacDecoder,
    player: PlayerSender,
    max_queued: usize,
    flush_until_seq: Arc<AtomicU64>,
) {
    let Some((mut stream, peer)) = accept_from(&listener, expected, "buffered audio").await else {
        return;
    };
    debug!("buffered audio connected from {peer}");

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
                    if let Some(seq) = packet_seq(&packet) {
                        if skip_before_boundary(&flush_until_seq, seq) {
                            skipped += 1;
                            if skipped <= 3 || skipped.is_multiple_of(2000) {
                                debug!("buffered audio: skipping seq {seq}");
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
                        Ok(pcm) if !pcm.is_empty() => {
                            player.play(u64::from(audio.seq), audio.timestamp, pcm)
                        }
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
    debug!("buffered audio disconnected ({decrypt_failures} decrypt failures, {skipped} skipped)");
}

/// The `FLUSHBUFFERED` discard filter for still-arriving packets: skip while
/// `seq` is below the boundary, and **clear the boundary the moment the
/// stream reaches it** — the flush is then complete. A boundary that outlived
/// its flush measurably discarded ~47 s of re-sent audio in one session
/// (see plans/20260801-02-pause-resume-hold.md).
fn skip_before_boundary(flush_until_seq: &AtomicU64, seq: u32) -> bool {
    let boundary = flush_until_seq.load(Ordering::Relaxed);
    if boundary == 0 {
        return false;
    }
    if u64::from(seq) < boundary {
        return true;
    }
    // Reached the flush target: consume the boundary so later audio — even a
    // re-send epoch with lower sequence numbers — plays. A concurrent newer
    // flush wins the exchange and stays in place.
    let _ = flush_until_seq.compare_exchange(boundary, 0, Ordering::Relaxed, Ordering::Relaxed);
    false
}

/// Accept and drain a buffered-audio connection we can't decode (missing key
/// or unsupported format), so the sender isn't left hanging.
async fn drain_tcp(listener: TcpListener, expected: IpAddr) {
    let Some((mut stream, _)) = accept_from(&listener, expected, "buffered audio (drain)").await
    else {
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

    /// The address a test "sender" connects from.
    fn peer() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42))
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
        (Session::new(local(), peer(), factory, tx), rx)
    }

    #[tokio::test]
    async fn accept_from_accepts_the_matching_peer() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // The client connects from loopback; the expected peer is that same IP.
        let _client = TcpStream::connect(addr).await.unwrap();
        let (_stream, peer) = accept_from(&listener, IpAddr::V4(Ipv4Addr::LOCALHOST), "test")
            .await
            .expect("a peer from the expected address is accepted");
        assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn accept_from_rejects_a_mismatched_peer_and_keeps_waiting() {
        use tokio::io::AsyncReadExt;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Expect a peer the loopback client cannot be, so its connection is
        // refused. 203.0.113.0/24 (TEST-NET-3) never matches a real client.
        let expected = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let mut accepting =
            tokio::spawn(async move { accept_from(&listener, expected, "test").await });

        let mut client = TcpStream::connect(addr).await.unwrap();
        // The mismatched connection is dropped by the server: the client's read
        // returns EOF rather than hanging.
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(1), client.read(&mut buf))
            .await
            .expect("a rejected connection is closed promptly")
            .unwrap();
        assert_eq!(n, 0, "a mismatched peer's connection is closed");

        // accept_from has not returned — it keeps waiting for the right peer
        // rather than accepting the attacker or giving up.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut accepting)
                .await
                .is_err(),
            "accept_from must keep waiting after rejecting a mismatched peer"
        );
        accepting.abort();
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
        stream.insert("audioFormat".into(), Value::Integer(0x400000u64.into()));
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
                channels: 2,
                peer: peer(),
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
        session.set_parameter(Some("text/parameters"), b"volume: -12.5\r\n");
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
    fn non_finite_volume_is_refused() {
        let (mut session, mut events) = session();
        session.set_parameter(Some("text/parameters"), b"volume: -12.5\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
        // `f32::parse` takes all of these; the knob must not move for any of
        // them, least of all to full scale.
        for value in ["nan", "NaN", "inf", "-inf", "1e40", "-1e40", "banana", ""] {
            session.set_parameter(
                Some("text/parameters"),
                format!("volume: {value}\r\n").as_bytes(),
            );
            assert!(
                events.try_recv().is_err(),
                "volume: {value:?} emitted an event"
            );
            // And the answer a sender gets back stays a well-formed float —
            // a malformed one makes it abort before SETUP phase 2.
            assert_eq!(
                session.get_parameter(b"volume\r\n"),
                b"volume: -12.500000\r\n",
                "volume: {value:?} changed the current volume"
            );
        }
    }

    #[test]
    fn out_of_range_volume_is_clamped() {
        let (mut session, mut events) = session();
        // Above full scale, and far below the mute sentinel.
        for (sent, expected) in [("6.0", 0.0), ("-500", -144.0), ("-144", -144.0)] {
            session.set_parameter(
                Some("text/parameters"),
                format!("volume: {sent}\r\n").as_bytes(),
            );
            assert_eq!(events.try_recv(), Ok(Event::Volume { db: expected }));
        }
        // The echo carries the clamped value, not what was sent.
        assert_eq!(
            session.get_parameter(b"volume\r\n"),
            b"volume: -144.000000\r\n"
        );
    }

    /// One DMAP entry: 4-byte tag + big-endian u32 length + payload.
    fn dmap_entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut e = tag.to_vec();
        e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        e.extend_from_slice(payload);
        e
    }

    /// A complete track statement: `mlit` wrapping title/artist/album.
    fn dmap_track(title: &str) -> Vec<u8> {
        let children = [
            dmap_entry(b"minm", title.as_bytes()),
            dmap_entry(b"asar", b"Artist"),
            dmap_entry(b"asal", b"Album"),
        ]
        .concat();
        dmap_entry(b"mlit", &children)
    }

    /// Run a phase-2 SETUP for a buffered stream, starting the session.
    async fn start_stream(session: &mut Session) {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(TYPE_BUFFERED.into()));
        stream.insert("shk".into(), Value::Data(vec![7u8; 32]));
        let mut dict = Dictionary::new();
        dict.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let body = encode_plist(&dict).unwrap();
        session.handle_setup(&body).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_and_artwork_reach_the_host_mid_session() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));

        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Song"));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Metadata {
                title: Some("Song".into()),
                artist: Some("Artist".into()),
                album: Some("Album".into()),
            })
        );

        session.set_parameter(Some("image/png"), b"\x89PNG");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/png".into(),
                data: b"\x89PNG".to_vec(),
            })
        );

        // `image/none` with an empty body is the artwork-cleared statement,
        // forwarded rather than suppressed (it can happen mid-track).
        session.set_parameter(Some("image/none"), b"");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/none".into(),
                data: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn early_metadata_is_latched_until_session_start() {
        let (mut session, mut events) = session();
        // Pushed during the handshake, before SETUP phase 2: nothing yet...
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("First"));
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Second"));
        session.set_parameter(Some("image/jpeg"), b"JPEG");
        assert!(events.try_recv().is_err());

        // ...and the latest of each replays right after SessionStarted.
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Metadata { title: Some(t), .. }) if t == "Second"
        ));
        assert!(matches!(events.try_recv(), Ok(Event::Artwork { .. })));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_metadata_never_reaches_the_host() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));

        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"");
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"garbage, not dmap");
        // Truncated: an mlit that claims more payload than exists.
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"mlit\x00\x00\xff\xff");
        assert!(events.try_recv().is_err());

        // The session itself is unharmed — the volume path still works.
        session.set_parameter(Some("text/parameters"), b"volume: -6.0\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));
    }

    #[tokio::test]
    async fn progress_anchors_the_track_and_reports_the_senders_position() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {} // drain SessionStarted

        // A minute-long track, the sender five seconds in.
        let start = 1_000u32;
        let end = start + 44_100 * 60;
        let current = start + 44_100 * 5;
        session.set_parameter(
            Some("text/parameters"),
            format!("progress: {start}/{current}/{end}\r\n").as_bytes(),
        );

        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::from_secs(5),
                duration: Duration::from_secs(60),
            })
        );
        // The extent is what the playback thread needs: it turns the audio it
        // plays into a running position without another word from the sender.
        assert_eq!(
            *session.track.lock().unwrap(),
            Some(Track { start, end }),
            "the track extent must reach the playback thread"
        );
    }

    #[tokio::test]
    async fn progress_before_a_stream_anchors_but_reports_nothing() {
        let (mut session, mut events) = session();
        session.set_parameter(Some("text/parameters"), b"progress: 0/44100/441000\r\n");
        assert!(
            events.try_recv().is_err(),
            "a position without a stream means nothing"
        );
        assert!(session.track.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn malformed_progress_is_ignored() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        for body in [
            &b"progress: 1000/2000\r\n"[..],      // too few fields
            b"progress: 1000/2000/3000/4000\r\n", // too many
            b"progress: a/b/c\r\n",               // not numbers
            b"progress:\r\n",                     // empty
        ] {
            session.set_parameter(Some("text/parameters"), body);
            assert!(
                events.try_recv().is_err(),
                "unparseable progress must not report: {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[tokio::test]
    async fn progress_after_a_seek_clamps_instead_of_reporting_27_hours() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        // `current` before `start`: the subtraction would wrap.
        session.set_parameter(Some("text/parameters"), b"progress: 44100/0/441000\r\n");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::ZERO,
                duration: Duration::from_secs(9),
            })
        );
    }

    #[tokio::test]
    async fn a_new_stream_drops_the_previous_tracks_extent() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        session.set_parameter(Some("text/parameters"), b"progress: 0/0/441000\r\n");
        assert!(session.track.lock().unwrap().is_some());

        // A second stream on the same connection is a fresh timeline; a stale
        // extent would place the new audio inside the old track.
        start_stream(&mut session).await;
        assert_eq!(*session.track.lock().unwrap(), None);
        while events.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn volume_and_progress_travel_in_one_body() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        while events.try_recv().is_ok() {}
        session.set_parameter(
            Some("text/parameters"),
            b"volume: -6.0\r\nprogress: 0/44100/441000\r\n",
        );
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::from_secs(1),
                duration: Duration::from_secs(10),
            })
        );
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
    fn flush_boundary_is_not_sticky() {
        let boundary = AtomicU64::new(100);
        // Stale in-flight audio below the boundary is skipped.
        assert!(skip_before_boundary(&boundary, 50));
        assert!(skip_before_boundary(&boundary, 99));
        // Reaching the boundary consumes it...
        assert!(!skip_before_boundary(&boundary, 100));
        assert_eq!(boundary.load(Ordering::Relaxed), 0);
        // ...so a later re-send epoch with lower sequence numbers plays
        // (the 47-seconds-discarded regression).
        assert!(!skip_before_boundary(&boundary, 50));

        // No boundary set: nothing is skipped.
        assert!(!skip_before_boundary(&AtomicU64::new(0), 7));
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
