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
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;

use crate::buffered::{packet_seq, split_blocks, AudioDecryptor};
use crate::decode::AacDecoder;
use crate::events::{Event, EventSender};
use crate::player::{Player, PlayerSender, Position, TrackAnchor};
use crate::sink::{AudioSink, SinkFactory};
use crate::takeover::ActiveGuard;

/// Stream type constants from the SETUP `streams` array.
const TYPE_REALTIME: u64 = 96;
const TYPE_BUFFERED: u64 = 103;

/// The per-connection session state the commands operate on
/// ([`crate::commands`] is the only place outside this module that touches
/// the fields), plus the audio pipeline it feeds.
pub struct Session {
    /// The address the control connection arrived on — what we bind to and
    /// report back so the sender can reach our channels.
    pub(crate) local_ip: IpAddr,
    /// The address the sender connected *from*, reported to the host with
    /// `SessionStarted` (a display shows it; nothing else uses it).
    pub(crate) peer_ip: IpAddr,
    pub(crate) tasks: Vec<JoinHandle<()>>,
    /// Captured at SETUP phase 2, for audio decrypt/decode.
    pub(crate) stream_key: Option<Vec<u8>>,
    pub(crate) audio_format: Option<u64>,
    pub(crate) stream_type: Option<u64>,
    /// AirPlay volume in dB (0 = full, −30 ≈ min, −144 = mute). Always
    /// finite and in range: it is only ever written from a validated
    /// [`crate::types::VolumeDb`].
    pub(crate) volume: f32,
    /// Creates the host's sink at SETUP phase 2.
    pub(crate) sink_factory: SinkFactory,
    /// Where session milestones are reported to the host.
    pub(crate) events: EventSender,
    /// True between `SessionStarted` and `SessionEnded`.
    pub(crate) session_active: bool,
    /// Metadata/artwork that arrived while no session was active (senders
    /// may push them during the handshake, before SETUP phase 2). The
    /// latest of each is latched here and delivered right after
    /// `SessionStarted`, so the host only ever sees them inside a session.
    pub(crate) pending_metadata: Option<Event>,
    pub(crate) pending_artwork: Option<Event>,
    /// The playback thread, alive for the duration of a buffered stream.
    pub(crate) player: Option<Player>,
    /// Control handle for the player (pause/resume, flush) from the RTSP path.
    pub(crate) player_control: Option<PlayerSender>,
    /// `FLUSHBUFFERED` boundary: the reader drops arriving audio packets with
    /// a sequence number below this, discarding buffered-ahead audio on
    /// seek/skip. Self-clearing (consumed when the stream reaches it) and
    /// reset at stream setup — a stale boundary discards wanted audio.
    pub(crate) flush_until_seq: Arc<AtomicU64>,
    /// The current track's extent on the RTP timeline, from the sender's
    /// `progress:` line. Shared with the playback thread, which turns it into
    /// a position that follows the audio.
    pub(crate) track: TrackAnchor,
    /// Proof that this connection owns the active-session slot, held here so
    /// that it is released only once this session has fully torn down — see
    /// [`Drop`] and [`crate::takeover`].
    pub(crate) active_guard: Option<ActiveGuard>,
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
    pub(crate) fn send_event(&self, event: Event) {
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
                self.tasks.push(tokio::spawn(drain_tcp(listener)));
            }
            (_, Err(e)) => {
                warn!("buffered audio: decoder init failed ({e}); draining");
                self.tasks.push(tokio::spawn(drain_tcp(listener)));
            }
        }
        Ok(port)
    }

    /// Deliver an event the host expects only inside a session; before
    /// `SessionStarted` it is latched (latest wins) and replayed once the
    /// session starts.
    pub(crate) fn send_session_event(&mut self, event: Event) {
        if self.session_active {
            self.send_event(event);
        } else if matches!(event, Event::Artwork { .. }) {
            self.pending_artwork = Some(event);
        } else {
            self.pending_metadata = Some(event);
        }
    }

    /// Report `SessionEnded` once per started session.
    pub(crate) fn end_session(&mut self) {
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

/// Decode parameters for the buffered stream. AirPlay 2 buffered audio is
/// AAC-LC 44.1 kHz stereo (audioFormat bit `0x400000`); other formats aren't
/// negotiated yet, so default to that.
pub(crate) fn aac_params(_audio_format: Option<u64>) -> (u32, u8) {
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
    max_queued: usize,
    flush_until_seq: Arc<AtomicU64>,
) {
    let Ok((mut stream, peer)) = listener.accept().await else {
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
    use crate::commands::test_helpers::{peer, session};
    use crate::player::Track;

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

    #[tokio::test]
    async fn a_new_stream_drops_the_previous_tracks_extent() {
        use crate::commands::test_helpers::start_stream;
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        *session.track.lock().unwrap() = Some(Track {
            start: 0,
            end: 441_000,
        });

        // A second stream on the same connection is a fresh timeline; a stale
        // extent would place the new audio inside the old track.
        start_stream(&mut session).await;
        assert_eq!(*session.track.lock().unwrap(), None);
        while events.try_recv().is_ok() {}
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
}
