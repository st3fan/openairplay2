//! Metadata/artwork end-to-end: a synthetic sender pairs, pushes DMAP track
//! metadata *before* SETUP phase 2 (as a real sender may), completes a
//! buffered-stream SETUP, then pushes cover art — and the host receives
//! `SessionStarted`, the latched `Metadata`, and the `Artwork` events.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

use openairplay2::cipher::{sender_control_channel, Decryptor, Encryptor};
use openairplay2::server::{serve, Context};
use openairplay2::srp::SrpClient;
use openairplay2::tlv::{ty, Tlv};
use openairplay2::Identity;
use openairplay2::{AudioSink, Config, Event, SinkFactory};

struct TestSink;

impl AudioSink for TestSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

/// Start the server, keeping the host-side event receiver.
async fn start() -> (SocketAddr, UnboundedReceiver<Event>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sink_factory: SinkFactory = Arc::new(|_, _| Box::new(TestSink));
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
            pincode: None,
        },
        identity: Identity::generate(),
        sink_factory,
        events,
    });
    tokio::spawn(serve(listener, context));
    (addr, rx)
}

/// Complete transient pair-setup (M1..M4) and return the channel ciphers.
async fn pair(stream: &mut TcpStream) -> (Encryptor, Decryptor) {
    let mut m1 = Tlv::new();
    m1.put_u8(ty::STATE, 1)
        .put_u8(ty::METHOD, 0)
        .put_u8(ty::FLAGS, 0x10);
    let (status, body) =
        plain_request(stream, "POST /pair-setup RTSP/1.0\r\nCSeq: 0", &m1.encode()).await;
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
    let (_, body) =
        plain_request(stream, "POST /pair-setup RTSP/1.0\r\nCSeq: 1", &m3.encode()).await;
    let m4 = Tlv::decode(&body).unwrap();
    assert_eq!(m4.get_u8(ty::STATE), Some(4));

    sender_control_channel(client.session_key().unwrap())
}

#[tokio::test]
async fn metadata_and_artwork_events_end_to_end() {
    let (addr, mut events) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (mut enc, mut dec) = pair(&mut stream).await;

    // Track metadata pushed before the stream exists — must be latched.
    let (status, _) = encrypted_request(
        &mut stream,
        &mut enc,
        &mut dec,
        "SET_PARAMETER",
        "application/x-dmap-tagged",
        &dmap_track("Held Song"),
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    // SETUP phase 1 (no streams), then phase 2 with a buffered stream.
    let mut d = plist::Dictionary::new();
    d.insert("timingProtocol".into(), plist::Value::String("PTP".into()));
    let (status, _) = encrypted_request(
        &mut stream,
        &mut enc,
        &mut dec,
        "SETUP",
        "application/x-apple-binary-plist",
        &plist_bytes(d),
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    let mut s = plist::Dictionary::new();
    s.insert("type".into(), plist::Value::Integer(103u64.into()));
    s.insert("shk".into(), plist::Value::Data(vec![9u8; 32]));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s)]),
    );
    let (status, _) = encrypted_request(
        &mut stream,
        &mut enc,
        &mut dec,
        "SETUP",
        "application/x-apple-binary-plist",
        &plist_bytes(d),
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    // The session started and the latched metadata followed it in.
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionStarted { .. })
    ));
    assert_eq!(
        events.recv().await,
        Some(Event::Metadata {
            title: Some("Held Song".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
        })
    );

    // Cover art mid-session arrives directly.
    let (status, _) = encrypted_request(
        &mut stream,
        &mut enc,
        &mut dec,
        "SET_PARAMETER",
        "image/png",
        b"\x89PNG fake image bytes",
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(
        events.recv().await,
        Some(Event::Artwork {
            content_type: "image/png".into(),
            data: b"\x89PNG fake image bytes".to_vec(),
        })
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

/// Read one HTTP/RTSP response (status line + Content-Length body) from a
/// plaintext stream.
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

/// Send an encrypted RTSP request with the given `Content-Type` and read
/// the encrypted response.
async fn encrypted_request(
    stream: &mut TcpStream,
    enc: &mut Encryptor,
    dec: &mut Decryptor,
    method: &str,
    content_type: &str,
    body: &[u8],
) -> (String, Vec<u8>) {
    let mut req = format!(
        "{method} rtsp://x/1 RTSP/1.0\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    stream.write_all(&enc.encrypt(&req)).await.unwrap();
    read_encrypted_http(stream, dec).await
}

/// Read one HTTP response from the encrypted channel, decrypting blocks
/// until a full status-line + body is available.
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
