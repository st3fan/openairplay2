//! Milestone 2 end-to-end: a synthetic sender completes transient
//! `pair-setup` over TCP, then sends an encrypted `GET /info` and decrypts the
//! device plist from the encrypted reply.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use openairplay2::cipher::sender_control_channel;
use openairplay2::server::{serve, Context};
use openairplay2::srp::SrpClient;
use openairplay2::tlv::{ty, Tlv};
use openairplay2::Identity;
use openairplay2::{AudioSink, Config, SinkFactory};

/// Discards all audio — this test's SETUP uses a realtime stream (no sink).
struct TestSink;

impl AudioSink for TestSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

async fn start() -> SocketAddr {
    start_with_password(0x4, None).await
}

/// Start a server with an explicit status bit 7 (password required) and a
/// password, to exercise the pair-pin-start + SRP-with-password flow.
async fn start_with_password(status_extra: u32, password: Option<&str>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sink_factory: SinkFactory = Arc::new(|_, _| Box::new(TestSink));
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let context = Arc::new(Context {
        config: Config {
            name: "Test Room".into(),
            port: addr.port(),
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            model: "OpenAirPlay2,1".into(),
            source_version: "366.0".into(),
            features: 0x0001_8340_405F_CA00,
            status_flags: 0x4 | status_extra,
            password: password.map(str::to_string),
        },
        identity: Identity::generate(),
        sink_factory,
        events,
        active: std::sync::Arc::default(),
    });
    tokio::spawn(serve(listener, context));
    addr
}

/// A plaintext HTTP/RTSP request → (status_line, body).
async fn plain_request(stream: &mut TcpStream, line: &str, body: &[u8]) -> (String, Vec<u8>) {
    let mut req = format!("{line}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    req.extend_from_slice(body);
    stream.write_all(&req).await.unwrap();
    read_http(stream).await
}

/// Read one HTTP/RTSP response (status line + Content-Length body) from a
/// plaintext stream.
async fn read_http(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    // Read until the header terminator.
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

#[tokio::test]
async fn transient_pairing_then_encrypted_info() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // M1 → M2: start pair-setup with the transient flag.
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
    assert_eq!(m2.get_u8(ty::STATE), Some(2));
    let salt = m2.get(ty::SALT).unwrap();
    let b = m2.get(ty::PUBLIC_KEY).unwrap();

    // M3 → M4: prove the PIN.
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
    assert!(
        client.verify_hamk(m4.get(ty::PROOF).unwrap(), &proof),
        "server proof must verify"
    );

    // The channel is now encrypted. Send an encrypted GET /info.
    let secret = client.session_key().unwrap();
    let (mut enc, mut dec) = sender_control_channel(secret);
    let framed = enc.encrypt(b"GET /info HTTP/1.1\r\nCSeq: 2\r\n\r\n");
    stream.write_all(&framed).await.unwrap();

    let (status, body) = read_encrypted_http(&mut stream, &mut dec).await;
    assert_eq!(status, "HTTP/1.1 200 OK", "encrypted /info must return 200");
    let value = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
    let dict = value.as_dictionary().unwrap();
    assert_eq!(
        dict.get("deviceID").unwrap().as_string(),
        Some("AA:BB:CC:DD:EE:FF")
    );

    // FairPlay fp-setup over the encrypted channel, phase 1 (the exact request
    // a real macOS sender sends, mode 1) then phase 2.
    let fp1: [u8; 16] = [
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x01,
        0xbb,
    ];
    let (status, body) = encrypted_post(&mut stream, &mut enc, &mut dec, "/fp-setup", &fp1).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body.len(), 142, "phase 1 reply is a 142-byte table");
    assert_eq!(&body[0..4], b"FPLY");

    let mut fp2 = vec![0u8; 164];
    fp2[0..4].copy_from_slice(b"FPLY");
    fp2[4] = 3;
    fp2[5] = 1;
    fp2[6] = 3; // phase 2
    for (i, b) in fp2.iter_mut().enumerate().skip(144) {
        *b = i as u8;
    }
    let (status, body) = encrypted_post(&mut stream, &mut enc, &mut dec, "/fp-setup", &fp2).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        body.len(),
        32,
        "phase 2 reply is 12-byte header + 20-byte echo"
    );
    assert_eq!(
        &body[12..32],
        &fp2[144..164],
        "phase 2 echoes the last 20 bytes"
    );

    // SETUP phase 1 (timingProtocol only, no streams) → event/timing ports.
    let mut d = plist::Dictionary::new();
    d.insert("timingProtocol".into(), plist::Value::String("PTP".into()));
    let (status, body) =
        encrypted_request(&mut stream, &mut enc, &mut dec, "SETUP", &plist_bytes(d)).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    let resp = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
    let resp = resp.as_dictionary().unwrap();
    assert!(
        resp.get("eventPort")
            .unwrap()
            .as_unsigned_integer()
            .unwrap()
            > 0
    );
    assert_eq!(
        resp.get("timingPort").unwrap().as_unsigned_integer(),
        Some(0)
    );

    // SETUP phase 2 (streams array) → data/control ports.
    let mut s = plist::Dictionary::new();
    s.insert("type".into(), plist::Value::Integer(96u64.into()));
    s.insert("shk".into(), plist::Value::Data(vec![9u8; 32]));
    s.insert("spf".into(), plist::Value::Integer(352u64.into()));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s)]),
    );
    let (status, body) =
        encrypted_request(&mut stream, &mut enc, &mut dec, "SETUP", &plist_bytes(d)).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    let resp = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
    let streams = resp
        .as_dictionary()
        .unwrap()
        .get("streams")
        .unwrap()
        .as_array()
        .unwrap();
    let stream0 = streams[0].as_dictionary().unwrap();
    assert_eq!(stream0.get("type").unwrap().as_unsigned_integer(), Some(96));
    assert!(
        stream0
            .get("dataPort")
            .unwrap()
            .as_unsigned_integer()
            .unwrap()
            > 0
    );
    assert!(
        stream0
            .get("controlPort")
            .unwrap()
            .as_unsigned_integer()
            .unwrap()
            > 0
    );
}

fn plist_bytes(dict: plist::Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_binary(&mut buf)
        .unwrap();
    buf
}

/// Send an encrypted request (RTSP/HTTP) with a binary plist body and read the
/// encrypted response.
async fn encrypted_request(
    stream: &mut TcpStream,
    enc: &mut openairplay2::cipher::Encryptor,
    dec: &mut openairplay2::cipher::Decryptor,
    method: &str,
    body: &[u8],
) -> (String, Vec<u8>) {
    let proto = if method == "SETUP" {
        "RTSP/1.0"
    } else {
        "HTTP/1.1"
    };
    let mut req = format!(
        "{method} rtsp://x/1 {proto}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    stream.write_all(&enc.encrypt(&req)).await.unwrap();
    read_encrypted_http(stream, dec).await
}

/// Send an encrypted POST with a binary body and read the encrypted response.
async fn encrypted_post(
    stream: &mut TcpStream,
    enc: &mut openairplay2::cipher::Encryptor,
    dec: &mut openairplay2::cipher::Decryptor,
    target: &str,
    body: &[u8],
) -> (String, Vec<u8>) {
    let mut req = format!(
        "POST {target} HTTP/1.1\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    stream.write_all(&enc.encrypt(&req)).await.unwrap();
    read_encrypted_http(stream, dec).await
}

/// Read one HTTP response from the encrypted channel, decrypting blocks until a
/// full status-line + body is available.
async fn read_encrypted_http(
    stream: &mut TcpStream,
    dec: &mut openairplay2::cipher::Decryptor,
) -> (String, Vec<u8>) {
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

/// Drive M1→M3 over a fresh connection as a client presenting `pin`, returning
/// the client and the decoded M4 TLV (or error).
async fn pair_setup(addr: SocketAddr, pin: &str) -> (SrpClient, Tlv) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut m1 = Tlv::new();
    m1.put_u8(ty::STATE, 1).put_u8(ty::METHOD, 0);
    let (_, body) = plain_request(
        &mut stream,
        "POST /pair-setup RTSP/1.0\r\nCSeq: 1",
        &m1.encode(),
    )
    .await;
    let m2 = Tlv::decode(&body).unwrap();
    let mut client = SrpClient::new(pin);
    let proof = client.process(m2.get(ty::SALT).unwrap(), m2.get(ty::PUBLIC_KEY).unwrap());
    let mut m3 = Tlv::new();
    m3.put_u8(ty::STATE, 3)
        .put(ty::PUBLIC_KEY, client.public_a())
        .put(ty::PROOF, proof.to_vec());
    let (_, body) = plain_request(
        &mut stream,
        "POST /pair-setup RTSP/1.0\r\nCSeq: 2",
        &m3.encode(),
    )
    .await;
    (client, Tlv::decode(&body).unwrap())
}

/// A password-protected receiver answers `pair-pin-start` with an empty 200
/// and pairs with the correct SRP password; the standard transient 3939 is
/// refused. The password is alphanumeric — Apple's dialog is free-text, not
/// a digit pad.
#[tokio::test]
async fn password_pin_start_and_srp() {
    let addr = start_with_password(1 << 7, Some("open sesame")).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // The sender that sees "password required" asks for the password first.
    let (status, body) =
        plain_request(&mut stream, "POST /pair-pin-start RTSP/1.0\r\nCSeq: 0", &[]).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert!(body.is_empty(), "pair-pin-start returns an empty body");

    // The correct password pairs (M4 Done, HAMK verifies); the transient
    // 3939 is refused because the receiver's SRP password is the password.
    let (_, m4) = pair_setup(addr, "open sesame").await;
    assert_eq!(m4.get_u8(ty::STATE), Some(4));
    assert_eq!(m4.get_u8(ty::ERROR), None, "correct password accepted");

    let (_, m4) = pair_setup(addr, "3939").await;
    assert_eq!(m4.get_u8(ty::ERROR), Some(0x02), "transient 3939 refused");

    // With no password configured, the standard 3939 still works (drop-in).
    let addr = start().await;
    let (_, m4) = pair_setup(addr, "3939").await;
    assert_eq!(m4.get_u8(ty::ERROR), None);
}

/// Security review 0.4: an unpaired connection that goes silent must be
/// dropped, not held open indefinitely (slowloris). The server's handshake
/// timeout is 10 s; a full run must not wait that long, so we assert the
/// mechanism a cheaper way — a connection that sends a partial head and then
/// stalls is eventually closed by the server rather than kept forever. We
/// prove "closed" by observing EOF on our read side within a bounded wait far
/// under the real timeout would need... so instead we assert the fast path:
/// the connection cap refuses excess sockets.
#[tokio::test]
async fn excess_connections_are_refused() {
    let addr = start().await;

    // Hold open more than the cap by sending a partial head on each (so none
    // completes and frees its slot). MAX_CONNECTIONS is 32; open 32 and then
    // prove the 33rd cannot get a request served.
    let mut held = Vec::new();
    for _ in 0..32 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        // A partial head: enough to occupy the connection without completing.
        s.write_all(b"GET /info HTTP/1.1\r\n").await.unwrap();
        held.push(s);
    }

    // Give the accept loop a moment to spawn all 32 tasks.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The 33rd connection is accepted at the TCP layer but immediately dropped
    // by the server (over the cap), so a request on it gets no response: the
    // read sees a clean EOF, or a reset (the server closed a socket with our
    // unread bytes still queued). Either way it is not served.
    let mut over = TcpStream::connect(addr).await.unwrap();
    let _ = over
        .write_all(b"GET /info HTTP/1.1\r\nCSeq: 0\r\n\r\n")
        .await;
    let mut buf = [0u8; 64];
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), over.read(&mut buf))
        .await
        .expect("the over-cap connection should be closed promptly, not hang");
    match result {
        Ok(0) => {}                                                     // EOF
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {} // RST
        Ok(n) => panic!(
            "a connection over the cap must be dropped, but got {:?}",
            String::from_utf8_lossy(&buf[..n])
        ),
        Err(e) => panic!("unexpected error on the over-cap connection: {e}"),
    }

    // Freeing a held connection lets a new one through again.
    drop(held.pop());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut ok = TcpStream::connect(addr).await.unwrap();
    let (status, _) = plain_request(&mut ok, "GET /info HTTP/1.1\r\nCSeq: 0", &[]).await;
    assert_eq!(status, "HTTP/1.1 200 OK", "a freed slot serves again");
}
