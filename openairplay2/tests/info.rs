//! Milestone 1 end-to-end: the control server answers GET /info with a device
//! plist and 501s an unimplemented method, over a real TCP connection.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use openairplay2::server::{serve, Context};
use openairplay2::Identity;
use openairplay2::{AudioSink, Config, SinkFactory};

/// Discards all audio — these tests never stream.
struct TestSink;

impl AudioSink for TestSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

async fn start() -> SocketAddr {
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
            features: 0x0001_8340_405C_4A00,
            status_flags: 0x4,
        },
        identity: Identity::generate(),
        sink_factory,
        events,
    });
    tokio::spawn(serve(listener, context));
    addr
}

/// Send a request and return (status_line, headers, body).
async fn request(stream: &mut TcpStream, req: &str) -> (String, Vec<(String, String)>, Vec<u8>) {
    stream.write_all(req.as_bytes()).await.unwrap();

    // Read headers up to the blank line.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(
            stream.read(&mut byte).await.unwrap(),
            1,
            "eof reading headers"
        );
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap().to_string();
    let headers: Vec<(String, String)> = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .map(|(_, v)| v.parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.unwrap();
    (status, headers, body)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn get_info_returns_device_plist() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let (status, headers, body) =
        request(&mut stream, "GET /info HTTP/1.1\r\nCSeq: 0\r\n\r\n").await;

    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "CSeq"), Some("0"));
    assert_eq!(
        header(&headers, "Content-Type"),
        Some("application/x-apple-binary-plist")
    );

    let value = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
    let dict = value.as_dictionary().expect("info body is a plist dict");
    assert_eq!(
        dict.get("deviceID").unwrap().as_string(),
        Some("AA:BB:CC:DD:EE:FF")
    );
    assert_eq!(
        dict.get("features").unwrap().as_unsigned_integer(),
        Some(0x0001_8340_405C_4A00)
    );
    assert_eq!(dict.get("pk").unwrap().as_data().unwrap().len(), 32);
    assert!(dict.get("txtAirPlay").unwrap().as_data().is_some());
}

#[tokio::test]
async fn unimplemented_method_501_and_connection_survives() {
    let addr = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Persistent pairing (pair-verify) isn't implemented; only transient.
    let (status, headers, _) = request(
        &mut stream,
        "POST /pair-verify RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\n\x00\x01",
    )
    .await;
    assert_eq!(status, "RTSP/1.0 501 Not Implemented");
    assert_eq!(header(&headers, "CSeq"), Some("1"));

    // The connection must survive so the sender can keep probing.
    let (status, _, _) = request(&mut stream, "GET /info HTTP/1.1\r\nCSeq: 2\r\n\r\n").await;
    assert_eq!(status, "HTTP/1.1 200 OK");
}
