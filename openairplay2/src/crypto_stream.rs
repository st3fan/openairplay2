//! The AirPlay control connection, which starts in the clear (discovery +
//! pairing) and switches to the encrypted HomeKit framing once transient
//! pairing completes.
//!
//! To switch cleanly mid-connection we never read past a request boundary
//! while in the clear: the head is read a byte at a time until the blank
//! line, and the body is read to its exact `Content-Length`. Once the cipher
//! is installed, reads pull whole encrypted blocks and serve decrypted bytes.

use std::collections::VecDeque;
use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::cipher::{Decryptor, Encryptor};
use crate::http::{parse_head, Request, Response};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;
const READ_CHUNK: usize = 8 * 1024;

pub struct ControlConnection<R, W> {
    reader: R,
    writer: W,
    enc: Option<Encryptor>,
    dec: Option<Decryptor>,
    /// Decrypted plaintext ready to consume (encrypted mode only).
    plain: VecDeque<u8>,
    /// Ciphertext read but not yet forming a complete block.
    cipher_pending: Vec<u8>,
}

impl<R, W> ControlConnection<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> ControlConnection<R, W> {
        ControlConnection {
            reader,
            writer,
            enc: None,
            dec: None,
            plain: VecDeque::new(),
            cipher_pending: Vec::new(),
        }
    }

    /// Install the channel cipher; subsequent reads/writes are encrypted.
    pub fn enable_encryption(&mut self, enc: Encryptor, dec: Decryptor) {
        self.enc = Some(enc);
        self.dec = Some(dec);
    }

    pub fn is_encrypted(&self) -> bool {
        self.enc.is_some()
    }

    /// Read one request, transparently decrypting when the cipher is active.
    /// Returns `Ok(None)` on a clean EOF at a message boundary.
    pub async fn read_request(&mut self) -> io::Result<Option<Request>> {
        let mut head = Vec::new();
        loop {
            match self.read_byte().await? {
                None if head.is_empty() => return Ok(None),
                None => return Err(eof("EOF inside request head")),
                Some(b) => head.push(b),
            }
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > MAX_HEAD {
                return Err(bad("request head too large"));
            }
        }
        let head_str = std::str::from_utf8(&head[..head.len() - 4])
            .map_err(|_| bad("non-UTF8 request head"))?;
        let (method, target, protocol, headers) = parse_head(head_str)?;

        let len = Request::content_length(&headers)?;
        if len > MAX_BODY {
            return Err(bad("body too large"));
        }
        let body = self.read_exact_n(len).await?;
        Ok(Some(Request::from_parts(
            method, target, protocol, headers, body,
        )))
    }

    /// Write a response, encrypting when the cipher is active.
    pub async fn write_response(&mut self, response: &Response) -> io::Result<()> {
        let bytes = response.to_bytes();
        let framed = match &mut self.enc {
            Some(enc) => enc.encrypt(&bytes),
            None => bytes,
        };
        self.writer.write_all(&framed).await?;
        self.writer.flush().await
    }

    async fn read_byte(&mut self) -> io::Result<Option<u8>> {
        loop {
            if let Some(b) = self.plain.pop_front() {
                return Ok(Some(b));
            }
            match &self.dec {
                None => {
                    let mut b = [0u8; 1];
                    let n = self.reader.read(&mut b).await?;
                    return Ok((n == 1).then_some(b[0]));
                }
                Some(_) => {
                    if !self.fill_encrypted().await? {
                        return Ok(None); // EOF
                    }
                }
            }
        }
    }

    async fn read_exact_n(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            if let Some(b) = self.plain.pop_front() {
                out.push(b);
                continue;
            }
            match &self.dec {
                None => {
                    // Read at most the remaining count so we never overshoot
                    // the message boundary while in the clear.
                    let mut buf = vec![0u8; n - out.len()];
                    let r = self.reader.read(&mut buf).await?;
                    if r == 0 {
                        return Err(eof("EOF inside request body"));
                    }
                    out.extend_from_slice(&buf[..r]);
                }
                Some(_) => {
                    if !self.fill_encrypted().await? {
                        return Err(eof("EOF inside encrypted request body"));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Read more ciphertext and decrypt whatever complete blocks it yields into
    /// `plain`. Returns `false` on EOF with no more data.
    async fn fill_encrypted(&mut self) -> io::Result<bool> {
        let dec = self.dec.as_mut().expect("encrypted mode");
        loop {
            let (plaintext, used) = dec
                .decrypt_available(&self.cipher_pending)
                .ok_or_else(|| bad("channel authentication failed"))?;
            if used > 0 {
                self.cipher_pending.drain(0..used);
            }
            if !plaintext.is_empty() {
                self.plain.extend(plaintext);
                return Ok(true);
            }
            let mut buf = [0u8; READ_CHUNK];
            let n = self.reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(false);
            }
            self.cipher_pending.extend_from_slice(&buf[..n]);
        }
    }
}

fn eof(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg)
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cipher::{control_channel, sender_control_channel};

    #[tokio::test]
    async fn reads_plaintext_then_switches_to_encrypted() {
        let (server_side, sender_side) = tokio::io::duplex(64 * 1024);
        let (srd, swr) = tokio::io::split(server_side);
        let mut conn = ControlConnection::new(srd, swr);
        let (mut sender_rd, mut sender_wr) = tokio::io::split(sender_side);

        // Plaintext request → plaintext reply.
        sender_wr
            .write_all(b"GET /info HTTP/1.1\r\nCSeq: 0\r\n\r\n")
            .await
            .unwrap();
        let req = conn.read_request().await.unwrap().unwrap();
        assert_eq!((req.method.as_str(), req.target.as_str()), ("GET", "/info"));
        conn.write_response(&Response::ok("HTTP/1.1"))
            .await
            .unwrap();

        // Drain the plaintext reply so it doesn't sit ahead of the encrypted one.
        let mut buf = vec![0u8; 256];
        let n = sender_rd.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200 OK"));

        // Both ends install the cipher.
        let secret = [0x55u8; 64];
        let (enc, dec) = control_channel(&secret);
        conn.enable_encryption(enc, dec);
        let (mut sender_enc, mut sender_dec) = sender_control_channel(&secret);

        // Encrypted request with a body decrypts on the server.
        let framed =
            sender_enc.encrypt(b"POST /x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nabc");
        sender_wr.write_all(&framed).await.unwrap();
        let req = conn.read_request().await.unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.body, b"abc");

        // Encrypted response decrypts on the sender.
        conn.write_response(&Response::new("RTSP/1.0", 200, "OK"))
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = sender_rd.read(&mut buf).await.unwrap();
        let (plain, _) = sender_dec.decrypt_available(&buf[..n]).unwrap();
        assert!(String::from_utf8_lossy(&plain).starts_with("RTSP/1.0 200 OK"));
    }
}
