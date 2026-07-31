//! Buffered AirPlay 2 audio: the TCP block framing and per-packet
//! ChaCha20-Poly1305 decryption. Yields raw AAC-LC frames.
//!
//! Frame on the wire: `[u16 len (BE, includes the 2 length bytes)][packet]`.
//! The packet is a 12-byte RTP-ish header — `[0..4]` seq (24-bit, mask
//! `0x7FFFFF`), `[4..8]` timestamp, `[8..12]` SSRC — followed by the encrypted
//! payload. Decrypt (from shairport's `ap2_buffered_audio_processor.c`):
//! key = `shk`, nonce = 4 zero bytes ‖ the packet's last 8 bytes, AAD =
//! `packet[4..12]`, ciphertext+tag = `packet[12 .. len-8]`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

const HEADER_LEN: usize = 12;
const NONCE_SUFFIX_LEN: usize = 8;
const TAG_LEN: usize = 16;

/// A decrypted audio packet.
#[derive(Debug, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u32,
    pub timestamp: u32,
    pub ssrc: u32,
    /// The raw AAC-LC frame.
    pub payload: Vec<u8>,
}

/// Decrypts buffered-audio packets with a fixed session key (`shk`).
pub struct AudioDecryptor {
    cipher: ChaCha20Poly1305,
}

impl AudioDecryptor {
    pub fn new(shk: &[u8]) -> Option<AudioDecryptor> {
        (shk.len() == 32).then(|| AudioDecryptor {
            cipher: ChaCha20Poly1305::new(Key::from_slice(shk)),
        })
    }

    /// Decrypt one packet (the block contents *after* the 2-byte length).
    pub fn decrypt(&self, packet: &[u8]) -> Option<AudioPacket> {
        if packet.len() < HEADER_LEN + NONCE_SUFFIX_LEN + TAG_LEN {
            return None;
        }
        let seq = packet_seq(packet).unwrap();
        let timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        let ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());

        let nonce_start = packet.len() - NONCE_SUFFIX_LEN;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&packet[nonce_start..]);
        let aad = &packet[4..12];
        let ciphertext = &packet[HEADER_LEN..nonce_start];

        let payload = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad })
            .ok()?;
        Some(AudioPacket {
            seq,
            timestamp,
            ssrc,
            payload,
        })
    }
}

/// Peek the 24-bit packet sequence number from the (plaintext) header without
/// decrypting — used to drop packets below a `FLUSHBUFFERED` boundary cheaply.
/// Returns `None` if the packet is too short to hold a header.
pub fn packet_seq(packet: &[u8]) -> Option<u32> {
    if packet.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes(packet[0..4].try_into().unwrap()) & 0xFF_FFFF)
}

/// Split a buffer of TCP bytes into complete `[u16 len][packet]` blocks,
/// returning each packet (without the length prefix) and the bytes consumed.
/// A trailing partial block is left for the caller to buffer.
pub fn split_blocks(input: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut packets = Vec::new();
    let mut pos = 0;
    while pos + 2 <= input.len() {
        // `len` counts the whole block including the 2 length bytes.
        let len = u16::from_be_bytes([input[pos], input[pos + 1]]) as usize;
        if len < 2 || pos + len > input.len() {
            break;
        }
        packets.push(&input[pos + 2..pos + len]);
        pos += len;
    }
    (packets, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::AeadInPlace;

    const SHK: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2,
        0xe1, 0xf0,
    ];

    /// Build a valid encrypted packet the way a sender would.
    fn make_packet(seq: u32, timestamp: u32, ssrc: u32, payload: &[u8], nonce8: [u8; 8]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&(seq & 0x7F_FFFF).to_be_bytes());
        header.extend_from_slice(&timestamp.to_be_bytes());
        header.extend_from_slice(&ssrc.to_be_bytes());

        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce8);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&SHK));
        let mut buf = payload.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header[4..12], &mut buf)
            .unwrap();

        let mut packet = header;
        packet.extend_from_slice(&buf);
        packet.extend_from_slice(&tag);
        packet.extend_from_slice(&nonce8);
        packet
    }

    #[test]
    fn decrypts_a_packet() {
        let dec = AudioDecryptor::new(&SHK).unwrap();
        let payload = b"raw aac frame bytes here";
        let packet = make_packet(0x1234, 44100, 0xAABBCCDD, payload, [1, 2, 3, 4, 5, 6, 7, 8]);
        let got = dec.decrypt(&packet).unwrap();
        assert_eq!(got.seq, 0x1234);
        assert_eq!(got.timestamp, 44100);
        assert_eq!(got.ssrc, 0xAABBCCDD);
        assert_eq!(got.payload, payload);
    }

    #[test]
    fn wrong_key_or_tamper_fails() {
        let payload = b"payload";
        let mut packet = make_packet(1, 2, 3, payload, [9; 8]);
        // Flip a ciphertext byte → auth failure.
        packet[HEADER_LEN] ^= 0xff;
        assert!(AudioDecryptor::new(&SHK).unwrap().decrypt(&packet).is_none());
    }

    #[test]
    fn rejects_short_packet_and_bad_key_len() {
        assert!(AudioDecryptor::new(&[0u8; 16]).is_none());
        let dec = AudioDecryptor::new(&SHK).unwrap();
        assert!(dec.decrypt(&[0u8; 20]).is_none());
    }

    #[test]
    fn split_blocks_frames_and_leaves_partial() {
        // Two blocks of payloads [aa,aa,aa] and [bb], plus a partial third.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u16.to_be_bytes()); // len=5 → 3 payload bytes
        buf.extend_from_slice(&[0xaa, 0xaa, 0xaa]);
        buf.extend_from_slice(&3u16.to_be_bytes()); // len=3 → 1 payload byte
        buf.extend_from_slice(&[0xbb]);
        buf.extend_from_slice(&9u16.to_be_bytes()); // len=9 but only 2 more bytes → partial
        buf.extend_from_slice(&[0xcc, 0xcc]);

        let (packets, used) = split_blocks(&buf);
        assert_eq!(packets, vec![&[0xaa, 0xaa, 0xaa][..], &[0xbb][..]]);
        assert_eq!(used, 5 + 3);
    }
}
