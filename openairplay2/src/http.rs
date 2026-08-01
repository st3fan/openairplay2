//! The AirPlay control connection speaks a hybrid of HTTP/1.1 and RTSP/1.0 on
//! one TCP connection: request lines are either `GET /info HTTP/1.1` or
//! `<METHOD> rtsp://… RTSP/1.0`, and the response echoes the request's
//! protocol token. This module parses requests and writes responses; the body
//! framing (`Content-Length`) is shared by both.

use std::io;

#[cfg(test)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(test)]
const MAX_HEADERS: usize = 128;
#[cfg(test)]
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    /// The protocol token, e.g. `HTTP/1.1` or `RTSP/1.0`.
    pub protocol: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Parse a request head (everything before the blank line, without the
/// trailing CRLFCRLF) into method, target, protocol, and headers. Body length
/// is read from the `Content-Length` header by the caller.
pub fn parse_head(head: &str) -> io::Result<(String, String, String, Headers)> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.splitn(3, ' ');
    let (method, target, protocol) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(p)) if !m.is_empty() => (m, t, p),
        _ => {
            return Err(bad_data(format!(
                "malformed request line: {request_line:?}"
            )))
        }
    };
    if !protocol.starts_with("HTTP/") && !protocol.starts_with("RTSP/") {
        return Err(bad_data(format!("not HTTP or RTSP: {request_line:?}")));
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad_data(format!("malformed header line: {line:?}")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok((
        method.to_string(),
        target.to_string(),
        protocol.to_string(),
        Headers(headers),
    ))
}

impl Request {
    pub fn from_parts(
        method: String,
        target: String,
        protocol: String,
        headers: Headers,
        body: Vec<u8>,
    ) -> Request {
        Request {
            method,
            target,
            protocol,
            headers,
            body,
        }
    }

    /// The declared body length from `Content-Length`, or 0.
    pub fn content_length(headers: &Headers) -> io::Result<usize> {
        match headers.get("Content-Length") {
            Some(v) => v
                .parse()
                .map_err(|_| bad_data(format!("bad Content-Length: {v:?}"))),
            None => Ok(0),
        }
    }
}

/// Read one request. Returns `Ok(None)` on a clean EOF at a message boundary.
///
/// Test-only: the production path is `crypto_stream::ControlConnection`,
/// which must read the head a byte at a time so the cipher can be installed
/// exactly at a message boundary. This buffered reader exercises the shared
/// parsing in this module's tests.
#[cfg(test)]
pub async fn read_request<R>(reader: &mut BufReader<R>) -> io::Result<Option<Request>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let request_line = match read_line(reader).await? {
        None => return Ok(None),
        Some(line) if line.is_empty() => return Ok(None),
        Some(line) => line,
    };

    let mut parts = request_line.splitn(3, ' ');
    let (method, target, protocol) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(p)) if !m.is_empty() => (m, t, p),
        _ => {
            return Err(bad_data(format!(
                "malformed request line: {request_line:?}"
            )))
        }
    };
    if !protocol.starts_with("HTTP/") && !protocol.starts_with("RTSP/") {
        return Err(bad_data(format!("not HTTP or RTSP: {request_line:?}")));
    }

    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)
            .await?
            .ok_or_else(|| bad_data("EOF inside headers".to_string()))?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(bad_data("too many headers".to_string()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad_data(format!("malformed header line: {line:?}")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let headers = Headers(headers);

    let mut body = Vec::new();
    if let Some(len) = headers.get("Content-Length") {
        let len: usize = len
            .parse()
            .map_err(|_| bad_data(format!("bad Content-Length: {len:?}")))?;
        if len > MAX_BODY {
            return Err(bad_data(format!("body of {len} bytes is too large")));
        }
        body.resize(len, 0);
        reader.read_exact(&mut body).await?;
    }

    Ok(Some(Request {
        method: method.to_string(),
        target: target.to_string(),
        protocol: protocol.to_string(),
        headers,
        body,
    }))
}

#[cfg(test)]
async fn read_line<R>(reader: &mut BufReader<R>) -> io::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

fn bad_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[derive(Debug)]
pub struct Response {
    protocol: String,
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    /// Build a response echoing the request's protocol token.
    pub fn new(protocol: &str, status: u16, reason: &'static str) -> Self {
        Response {
            protocol: protocol.to_string(),
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn ok(protocol: &str) -> Self {
        Response::new(protocol, 200, "OK")
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Attach a body and its `Content-Type`.
    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// Serialize to the wire form. AirPlay senders expect a `Content-Length`
    /// on every response, including empty ones.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("{} {} {}\r\n", self.protocol, self.status, self.reason).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }

    /// Test-only: production writes go through `to_bytes` (the encrypted
    /// channel frames them itself).
    #[cfg(test)]
    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.to_bytes()).await?;
        writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn parse(input: &[u8]) -> io::Result<Option<Request>> {
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        read_request(&mut reader).await
    }

    #[tokio::test]
    async fn parses_http_get_info() {
        let req = parse(b"GET /info HTTP/1.1\r\nCSeq: 0\r\nUser-Agent: AirPlay/650.51\r\n\r\n")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.target, "/info");
        assert_eq!(req.protocol, "HTTP/1.1");
        assert_eq!(req.headers.get("cseq"), Some("0"));
    }

    #[tokio::test]
    async fn parses_rtsp_with_body() {
        let req = parse(
            b"POST /pair-setup RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\n\x00\x01\x02\x03",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.protocol, "RTSP/1.0");
        assert_eq!(req.body, [0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn rejects_non_http_rtsp() {
        assert!(parse(b"NOTIFY sip:x SIP/2.0\r\n\r\n").await.is_err());
    }

    #[tokio::test]
    async fn response_echoes_protocol_and_always_has_length() {
        let mut out = Vec::new();
        Response::ok("HTTP/1.1")
            .header("CSeq", "0")
            .write_to(&mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("CSeq: 0\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[tokio::test]
    async fn response_with_body_sets_content_type() {
        let mut out = Vec::new();
        Response::ok("RTSP/1.0")
            .body("application/x-apple-binary-plist", vec![1, 2, 3])
            .write_to(&mut out)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("Content-Type: application/x-apple-binary-plist\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
    }
}
