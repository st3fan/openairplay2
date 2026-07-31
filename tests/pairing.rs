//! Milestone 2 end-to-end: a synthetic sender completes transient
//! `pair-setup` over TCP, then sends an encrypted `GET /info` and decrypts the
//! device plist from the encrypted reply.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use openairplay2::cipher::sender_control_channel;
use openairplay2::identity::Identity;
use openairplay2::server::{serve, Context};
use openairplay2::srp::SrpClient;
use openairplay2::tlv::{ty, Tlv};
use openairplay2::Config;

async fn start() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let context = Arc::new(Context {
        config: Config {
            name: "Test Room".into(),
            port: addr.port(),
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            model: "OpenAirPlay2,1".into(),
            source_version: "366.0".into(),
            features: 0x0001_8340_405C_4A00,
            status_flags: 0x4,
        },
        identity: Identity::generate(),
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
