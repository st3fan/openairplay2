//! The post-pairing encrypted control channel: HKDF-SHA512 key derivation and
//! the HomeKit secure-session block framing.
//!
//! Framing: the byte stream is a sequence of blocks, each
//! `[u16 length (LE)][ciphertext][16-byte Poly1305 tag]`, `length ≤ 1024`.
//! AAD is the 2-byte length; the nonce is 4 zero bytes followed by an 8-byte
//! little-endian block counter, incremented per block, independently per
//! direction.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha512;

const BLOCK_MAX: usize = 1024;
const TAG_LEN: usize = 16;
const LEN_LEN: usize = 2;

const CONTROL_SALT: &[u8] = b"Control-Salt";
const CONTROL_WRITE_INFO: &[u8] = b"Control-Write-Encryption-Key";
const CONTROL_READ_INFO: &[u8] = b"Control-Read-Encryption-Key";

/// Derive a 32-byte key with HKDF-SHA512.
fn derive_key(shared_secret: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt), shared_secret);
    let mut key = [0u8; 32];
    hk.expand(info, &mut key)
        .expect("32 is a valid HKDF length");
    key
}

fn nonce_for(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&nonce)
}

/// The two independent cipher halves for the receiver (server) side of the
/// control channel. We *decrypt* what the sender wrote (its `Control-Write`
/// key) and *encrypt* what the sender reads (its `Control-Read` key).
pub fn control_channel(shared_secret: &[u8]) -> (Encryptor, Decryptor) {
    let encrypt_key = derive_key(shared_secret, CONTROL_SALT, CONTROL_READ_INFO);
    let decrypt_key = derive_key(shared_secret, CONTROL_SALT, CONTROL_WRITE_INFO);
    (
        Encryptor {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&encrypt_key)),
            counter: 0,
        },
        Decryptor {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&decrypt_key)),
            counter: 0,
        },
    )
}

/// The outbound (encrypting) half of the control channel.
pub struct Encryptor {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Encryptor {
    /// Encrypt `plaintext` into one or more framed blocks.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for block in plaintext.chunks(BLOCK_MAX) {
            let aad = (block.len() as u16).to_le_bytes();
            let nonce = nonce_for(self.counter);
            let ct = self
                .cipher
                .encrypt(
                    &nonce,
                    Payload {
                        msg: block,
                        aad: &aad,
                    },
                )
                .expect("chacha encrypt cannot fail");
            out.extend_from_slice(&aad);
            out.extend_from_slice(&ct);
            self.counter += 1;
        }
        out
    }
}

/// The sender's (peer's) cipher halves — read/write keys swapped relative to
/// [`control_channel`]. For tests and the synthetic sender.
pub fn sender_control_channel(shared_secret: &[u8]) -> (Encryptor, Decryptor) {
    let encrypt_key = derive_key(shared_secret, CONTROL_SALT, CONTROL_WRITE_INFO);
    let decrypt_key = derive_key(shared_secret, CONTROL_SALT, CONTROL_READ_INFO);
    (
        Encryptor {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&encrypt_key)),
            counter: 0,
        },
        Decryptor {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&decrypt_key)),
            counter: 0,
        },
    )
}

/// The inbound (decrypting) half of the control channel.
pub struct Decryptor {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Decryptor {
    /// Decrypt as many complete blocks as are present at the front of `input`,
    /// returning the concatenated plaintext and the number of input bytes
    /// consumed. Leaves a trailing partial block for the caller to buffer.
    /// Returns `None` if authentication fails (fatal for the connection).
    pub fn decrypt_available(&mut self, input: &[u8]) -> Option<(Vec<u8>, usize)> {
        let mut plaintext = Vec::new();
        let mut pos = 0;
        while pos + LEN_LEN <= input.len() {
            let len = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
            let block_end = pos + LEN_LEN + len + TAG_LEN;
            if block_end > input.len() {
                break; // incomplete block; wait for more bytes
            }
            let aad = &input[pos..pos + LEN_LEN];
            let ct = &input[pos + LEN_LEN..block_end];
            let nonce = nonce_for(self.counter);
            let pt = self.cipher.decrypt(&nonce, Payload { msg: ct, aad }).ok()?;
            plaintext.extend_from_slice(&pt);
            self.counter += 1;
            pos = block_end;
        }
        Some((plaintext, pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_deterministic_for_a_secret() {
        let secret = [0x42u8; 64];
        assert_eq!(
            derive_key(&secret, CONTROL_SALT, CONTROL_READ_INFO),
            derive_key(&secret, CONTROL_SALT, CONTROL_READ_INFO)
        );
        assert_ne!(
            derive_key(&secret, CONTROL_SALT, CONTROL_READ_INFO),
            derive_key(&secret, CONTROL_SALT, CONTROL_WRITE_INFO)
        );
    }

    use super::sender_control_channel as peer;

    #[test]
    fn round_trips_across_the_two_ends() {
        let secret = [0x11u8; 64];
        let (mut server_enc, mut server_dec) = control_channel(&secret);
        let (mut client_enc, mut client_dec) = peer(&secret);

        // client -> server
        let msg = b"GET /info HTTP/1.1\r\n\r\n";
        let framed = client_enc.encrypt(msg);
        let (plain, used) = server_dec.decrypt_available(&framed).unwrap();
        assert_eq!(used, framed.len());
        assert_eq!(plain, msg);

        // server -> client
        let reply = b"HTTP/1.1 200 OK\r\n\r\n";
        let framed = server_enc.encrypt(reply);
        let (plain, _) = client_dec.decrypt_available(&framed).unwrap();
        assert_eq!(plain, reply);
    }

    #[test]
    fn multi_block_payload_round_trips() {
        let secret = [0x22u8; 64];
        let (_, mut server_dec) = control_channel(&secret);
        let (mut client_enc, _) = peer(&secret);
        let big: Vec<u8> = (0..2500).map(|i| i as u8).collect(); // 3 blocks
        let framed = client_enc.encrypt(&big);
        let (plain, used) = server_dec.decrypt_available(&framed).unwrap();
        assert_eq!(used, framed.len());
        assert_eq!(plain, big);
    }

    #[test]
    fn partial_trailing_block_is_left_buffered() {
        let secret = [0x33u8; 64];
        let (_, mut server_dec) = control_channel(&secret);
        let (mut client_enc, _) = peer(&secret);
        let framed = client_enc.encrypt(b"hello world");
        let (plain, used) = server_dec
            .decrypt_available(&framed[..framed.len() - 5])
            .unwrap();
        assert!(plain.is_empty());
        assert_eq!(used, 0);
        let (plain, used) = server_dec.decrypt_available(&framed).unwrap();
        assert_eq!(plain, b"hello world");
        assert_eq!(used, framed.len());
    }
}
