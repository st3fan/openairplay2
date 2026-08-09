//! Sender takeover end-to-end: AirPlay 2 is last-stream-wins, so a second
//! sender's `SETUP` interrupts whoever is streaming
//! (plans/20260808-04-sender-takeover.md).
//!
//! Two synthetic senders pair over real sockets. The first streams; the
//! second SETUPs. What must hold:
//!
//! - the first sender's control connection is **closed** (that is the whole
//!   signal a real sender needs — a HomePod-interrupted iPhone pauses itself
//!   and drops the route);
//! - the first sink is **dropped before** the second is created, so the two
//!   never hold the host's audio device at once;
//! - the host sees `SessionEnded` (first) before `SessionStarted` (second);
//! - and a connection that only *probes* — `GET /info`, pairing — never
//!   interrupts the stream.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

use openairplay2::cipher::{sender_control_channel, Decryptor, Encryptor};
use openairplay2::server::{serve, Context};
use openairplay2::srp::SrpClient;
use openairplay2::tlv::{ty, Tlv};
use openairplay2::Identity;
use openairplay2::{AudioSink, Config, Event, SinkFactory};

/// What the sinks did, in order — `+N` when the Nth sink was created, `-N`
/// when it was dropped. A takeover must read `+1, -1, +2`: never `+1, +2`.
type SinkLog = Arc<Mutex<Vec<String>>>;

struct TestSink {
    id: usize,
    log: SinkLog,
}

impl AudioSink for TestSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

impl Drop for TestSink {
    fn drop(&mut self) {
        self.log.lock().unwrap().push(format!("-{}", self.id));
    }
}

/// Start the server, keeping the host's event receiver and the sink log.
async fn start() -> (SocketAddr, UnboundedReceiver<Event>, SinkLog) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: SinkLog = Arc::new(Mutex::new(Vec::new()));
    let sink_log = log.clone();
    let next_id = Arc::new(Mutex::new(0usize));
    let sink_factory: SinkFactory = Arc::new(move |_, _| {
        let mut id = next_id.lock().unwrap();
        *id += 1;
        sink_log.lock().unwrap().push(format!("+{id}"));
        Box::new(TestSink {
            id: *id,
            log: sink_log.clone(),
        })
    });
    let (events, rx) = tokio::sync::mpsc::unbounded_channel();
    let context = Arc::new(Context {
        config: Config {
            name: "Test Room".into(),
            port: addr.port(),
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            model: "OpenAirPlay2,1".into(),
            source_version: "366.0".into(),
            features: 0x0001_8340_405F_CA00,
            status_flags: 0x4,
            password: None,
        },
        identity: Identity::generate(),
        sink_factory,
        events,
        active: Arc::default(),
    });
    tokio::spawn(serve(listener, context));
    (addr, rx, log)
}

/// A paired sender: its socket and channel ciphers.
struct Sender {
    stream: TcpStream,
    enc: Encryptor,
    dec: Decryptor,
}

impl Sender {
    /// Connect and complete transient pair-setup (M1..M4).
    async fn connect(addr: SocketAddr) -> Sender {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut m1 = Tlv::new();
        m1.put_u8(ty::STATE, 1)
            .put_u8(ty::METHOD, 0)
            .put_u8(ty::FLAGS, 0x10);
        let (status, body) = plain_request(
            &mut stream,
            "POST /pair-setup RTSP/1.0\r\nCSeq: 0",
            &m1.encode(),
        )
        .await;
        assert_eq!(status, "RTSP/1.0 200 OK");
        let m2 = Tlv::decode(&body).unwrap();
        let salt = m2.get(ty::SALT).unwrap();
        let b = m2.get(ty::PUBLIC_KEY).unwrap();

        let mut client = SrpClient::new("3939");
        let proof = client.process(salt, b);
        let mut m3 = Tlv::new();
        m3.put_u8(ty::STATE, 3)
            .put(ty::PUBLIC_KEY, client.public_a())
            .put(ty::PROOF, proof.to_vec());
        let (_, body) = plain_request(
            &mut stream,
            "POST /pair-setup RTSP/1.0\r\nCSeq: 1",
            &m3.encode(),
        )
        .await;
        let m4 = Tlv::decode(&body).unwrap();
        assert_eq!(m4.get_u8(ty::STATE), Some(4));

        let (enc, dec) = sender_control_channel(client.session_key().unwrap());
        Sender { stream, enc, dec }
    }

    /// SETUP phase 1 (ports) then phase 2 (a buffered stream) — after which
    /// the receiver has created a sink and started a session.
    async fn start_stream(&mut self) {
        let mut d = plist::Dictionary::new();
        d.insert("timingProtocol".into(), plist::Value::String("PTP".into()));
        let (status, _) = self.request("SETUP", &plist_bytes(d)).await;
        assert_eq!(status, "RTSP/1.0 200 OK", "SETUP phase 1");

        let mut s = plist::Dictionary::new();
        s.insert("type".into(), plist::Value::Integer(103u64.into()));
        s.insert("shk".into(), plist::Value::Data(vec![9u8; 32]));
        let mut d = plist::Dictionary::new();
        d.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(s)]),
        );
        let (status, _) = self.request("SETUP", &plist_bytes(d)).await;
        assert_eq!(status, "RTSP/1.0 200 OK", "SETUP phase 2");
    }

    async fn request(&mut self, method: &str, body: &[u8]) -> (String, Vec<u8>) {
        self.request_to(method, "rtsp://x/1", body).await
    }

    async fn request_to(&mut self, method: &str, target: &str, body: &[u8]) -> (String, Vec<u8>) {
        let mut req = format!(
            "{method} {target} RTSP/1.0\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        req.extend_from_slice(body);
        self.stream
            .write_all(&self.enc.encrypt(&req))
            .await
            .unwrap();
        read_encrypted_http(&mut self.stream, &mut self.dec).await
    }

    /// Whether the receiver has closed this connection (what an interrupted
    /// sender sees). Bounded wait, so a failure is a failure and not a hang.
    async fn closed_by_receiver(&mut self) -> bool {
        let mut buf = [0u8; 64];
        matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.stream.read(&mut buf)
            )
            .await,
            Ok(Ok(0))
        )
    }
}

#[tokio::test]
async fn a_second_sender_takes_the_session_over() {
    let (addr, mut events, sinks) = start().await;

    // Sender A pairs and streams.
    let mut a = Sender::connect(addr).await;
    a.start_stream().await;
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionStarted { .. })
    ));
    assert_eq!(*sinks.lock().unwrap(), ["+1"], "A's sink exists");

    // Sender B pairs and starts playing: last stream wins.
    let mut b = Sender::connect(addr).await;
    b.start_stream().await;

    // A's connection is closed — the signal a real sender pauses on.
    assert!(
        a.closed_by_receiver().await,
        "the interrupted sender's connection must be closed"
    );

    // The handover was ordered: A's sink was released before B's was made,
    // so an exclusive audio device is never held twice.
    assert_eq!(
        *sinks.lock().unwrap(),
        ["+1", "-1", "+2"],
        "sink lifetimes must not overlap"
    );

    // And the host saw the session end before the new one started.
    assert_eq!(events.recv().await, Some(Event::SessionEnded));
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionStarted { .. })
    ));
}

#[tokio::test]
async fn a_probing_connection_does_not_interrupt_the_stream() {
    let (addr, mut events, sinks) = start().await;

    let mut a = Sender::connect(addr).await;
    a.start_stream().await;
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionStarted { .. })
    ));

    // A second device that only looks: pairs, asks for /info, goes away —
    // exactly what a sender does while its AirPlay menu is open.
    let mut b = Sender::connect(addr).await;
    let (status, body) = b.request_to("GET", "/info", b"").await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert!(!body.is_empty(), "/info returns the device plist");
    drop(b);

    // A is untouched: still connected, still the only sink, no session end.
    let still_open = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        a.closed_by_receiver(),
    )
    .await;
    assert!(
        matches!(still_open, Err(_) | Ok(false)),
        "a probing connection must not close the streaming one"
    );
    assert_eq!(*sinks.lock().unwrap(), ["+1"], "no new sink, none dropped");
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}

fn plist_bytes(dict: plist::Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_binary(&mut buf)
        .unwrap();
    buf
}

/// A plaintext HTTP/RTSP request → (status_line, body).
async fn plain_request(stream: &mut TcpStream, line: &str, body: &[u8]) -> (String, Vec<u8>) {
    let mut req = format!("{line}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    req.extend_from_slice(body);
    stream.write_all(&req).await.unwrap();
    read_http(stream).await
}

async fn read_http(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let (status, len) = parse_head(&buf[..pos]);
            let body_start = pos + 4;
            while buf.len() < body_start + len {
                let mut tmp = [0u8; 1024];
                let n = stream.read(&mut tmp).await.unwrap();
                assert!(n > 0, "eof reading body");
                buf.extend_from_slice(&tmp[..n]);
            }
            return (status, buf[body_start..body_start + len].to_vec());
        }
        let mut tmp = [0u8; 1024];
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "eof reading head");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn parse_head(head: &[u8]) -> (String, usize) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap().to_string();
    let len = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("Content-Length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    (status, len)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP response from the encrypted channel.
async fn read_encrypted_http(stream: &mut TcpStream, dec: &mut Decryptor) -> (String, Vec<u8>) {
    let mut cipherbuf = Vec::new();
    let mut plain = Vec::new();
    loop {
        if let Some(pos) = find(&plain, b"\r\n\r\n") {
            let (status, len) = parse_head(&plain[..pos]);
            let body_start = pos + 4;
            if plain.len() >= body_start + len {
                return (status, plain[body_start..body_start + len].to_vec());
            }
        }
        let mut tmp = [0u8; 1024];
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "eof reading encrypted response");
        cipherbuf.extend_from_slice(&tmp[..n]);
        let (pt, used) = dec.decrypt_available(&cipherbuf).unwrap();
        cipherbuf.drain(0..used);
        plain.extend_from_slice(&pt);
    }
}
