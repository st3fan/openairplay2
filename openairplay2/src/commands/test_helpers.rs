//! Shared fixtures for command and handler tests: a session wired to a
//! discarding sink with the host's event receiver, a started buffered
//! stream, and builders for requests and DMAP bodies.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use plist::{Dictionary, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::commands::{setup_streams, SetupStreamsParams};
use crate::events::Event;
use crate::http::{parse_head, Request};
use crate::session::{Session, TYPE_BUFFERED};
use crate::sink::{AudioSink, SinkFactory};

pub struct TestSink;

impl AudioSink for TestSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

pub fn local() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

/// The address a test "sender" connects from.
pub fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42))
}

/// A session wired to a discarding sink, plus the host's event receiver.
pub fn session() -> (Session, UnboundedReceiver<Event>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let factory: SinkFactory = Arc::new(|_, _| Box::new(TestSink));
    (Session::new(local(), peer(), factory, tx), rx)
}

/// Encode a plist dictionary the way a sender's body would arrive.
pub fn plist_bytes(dict: &Dictionary) -> Vec<u8> {
    let mut body = Vec::new();
    Value::Dictionary(dict.clone())
        .to_writer_binary(&mut body)
        .unwrap();
    body
}

/// Run a phase-2 SETUP for a buffered stream, starting the session.
pub async fn start_stream(session: &mut Session) {
    let params = SetupStreamsParams {
        stream_type: Some(TYPE_BUFFERED),
        audio_format: None,
        shared_key: Some(vec![7u8; 32]),
        spf: None,
    };
    setup_streams(session, params).await.unwrap();
}

/// Build a request the way the wire would deliver it, through the real head
/// parser.
pub fn request(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
    let mut head = format!("{method} {target} RTSP/1.0");
    for (name, value) in headers {
        head.push_str(&format!("\r\n{name}: {value}"));
    }
    let (method, target, protocol, headers) = parse_head(&head).unwrap();
    Request::from_parts(method, target, protocol, headers, body.to_vec())
}

/// One DMAP entry: 4-byte tag + big-endian u32 length + payload.
pub fn dmap_entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut e = tag.to_vec();
    e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    e.extend_from_slice(payload);
    e
}

/// A complete track statement: `mlit` wrapping title/artist/album.
pub fn dmap_track(title: &str) -> Vec<u8> {
    let children = [
        dmap_entry(b"minm", title.as_bytes()),
        dmap_entry(b"asar", b"Artist"),
        dmap_entry(b"asal", b"Album"),
    ]
    .concat();
    dmap_entry(b"mlit", &children)
}
